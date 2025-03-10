// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
mod aarch64;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use aarch64::*;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86_64;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use x86_64::*;

use std::cell::RefCell;
use std::cmp::{min, Reverse};
use std::collections::{BTreeMap, BinaryHeap};
use std::convert::TryFrom;
use std::ffi::CString;
use std::mem::{size_of, ManuallyDrop};
use std::os::raw::{c_int, c_ulong, c_void};
use std::os::unix::{io::AsRawFd, prelude::OsStrExt};
use std::path::{Path, PathBuf};
use std::ptr::copy_nonoverlapping;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use libc::{
    open, sigset_t, EBUSY, EFAULT, EINVAL, EIO, ENOENT, ENOSPC, EOVERFLOW, O_CLOEXEC, O_RDWR,
};

use base::{
    block_signal, errno_result, error, ioctl, ioctl_with_mut_ref, ioctl_with_ref, ioctl_with_val,
    pagesize, signal, unblock_signal, AsRawDescriptor, Error, Event, FromRawDescriptor,
    MappedRegion, MemoryMapping, MemoryMappingBuilder, MemoryMappingBuilderUnix, MmapError,
    Protection, RawDescriptor, Result, SafeDescriptor,
};
use data_model::vec_with_array_field;
use kvm_sys::*;
use sync::Mutex;
use vm_memory::{GuestAddress, GuestMemory};

use crate::{
    ClockState, Datamatch, DeviceKind, Hypervisor, HypervisorCap, IoEventAddress, IrqRoute,
    IrqSource, MPState, MemSlot, Vcpu, VcpuExit, VcpuRunHandle, Vm, VmCap,
};

// Wrapper around KVM_SET_USER_MEMORY_REGION ioctl, which creates, modifies, or deletes a mapping
// from guest physical to host user pages.
//
// Safe when the guest regions are guaranteed not to overlap.
unsafe fn set_user_memory_region(
    descriptor: &SafeDescriptor,
    slot: MemSlot,
    read_only: bool,
    log_dirty_pages: bool,
    guest_addr: u64,
    memory_size: u64,
    userspace_addr: *mut u8,
) -> Result<()> {
    let mut flags = if read_only { KVM_MEM_READONLY } else { 0 };
    if log_dirty_pages {
        flags |= KVM_MEM_LOG_DIRTY_PAGES;
    }
    let region = kvm_userspace_memory_region {
        slot,
        flags,
        guest_phys_addr: guest_addr,
        memory_size,
        userspace_addr: userspace_addr as u64,
    };

    let ret = ioctl_with_ref(descriptor, KVM_SET_USER_MEMORY_REGION(), &region);
    if ret == 0 {
        Ok(())
    } else {
        errno_result()
    }
}

/// Helper function to determine the size in bytes of a dirty log bitmap for the given memory region
/// size.
///
/// # Arguments
///
/// * `size` - Number of bytes in the memory region being queried.
pub fn dirty_log_bitmap_size(size: usize) -> usize {
    let page_size = pagesize();
    (((size + page_size - 1) / page_size) + 7) / 8
}

pub struct Kvm {
    kvm: SafeDescriptor,
}

type KvmCap = kvm::Cap;

impl Kvm {
    pub fn new_with_path(device_path: &Path) -> Result<Kvm> {
        // Open calls are safe because we give a nul-terminated string and verify the result.
        let c_path = CString::new(device_path.as_os_str().as_bytes()).unwrap();
        let ret = unsafe { open(c_path.as_ptr(), O_RDWR | O_CLOEXEC) };
        if ret < 0 {
            return errno_result();
        }
        // Safe because we verify that ret is valid and we own the fd.
        Ok(Kvm {
            kvm: unsafe { SafeDescriptor::from_raw_descriptor(ret) },
        })
    }

    /// Opens `/dev/kvm/` and returns a Kvm object on success.
    pub fn new() -> Result<Kvm> {
        Kvm::new_with_path(&PathBuf::from("/dev/kvm"))
    }

    /// Gets the size of the mmap required to use vcpu's `kvm_run` structure.
    pub fn get_vcpu_mmap_size(&self) -> Result<usize> {
        // Safe because we know that our file is a KVM fd and we verify the return result.
        let res = unsafe { ioctl(self, KVM_GET_VCPU_MMAP_SIZE()) };
        if res > 0 {
            Ok(res as usize)
        } else {
            errno_result()
        }
    }
}

impl AsRawDescriptor for Kvm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.kvm.as_raw_descriptor()
    }
}

impl Hypervisor for Kvm {
    fn try_clone(&self) -> Result<Self> {
        Ok(Kvm {
            kvm: self.kvm.try_clone()?,
        })
    }

    fn check_capability(&self, cap: &HypervisorCap) -> bool {
        if let Ok(kvm_cap) = KvmCap::try_from(cap) {
            // this ioctl is safe because we know this kvm descriptor is valid,
            // and we are copying over the kvm capability (u32) as a c_ulong value.
            unsafe { ioctl_with_val(self, KVM_CHECK_EXTENSION(), kvm_cap as c_ulong) == 1 }
        } else {
            // this capability cannot be converted on this platform, so return false
            false
        }
    }
}

/// A wrapper around creating and using a KVM VM.
pub struct KvmVm {
    kvm: Kvm,
    vm: SafeDescriptor,
    guest_mem: GuestMemory,
    mem_regions: Arc<Mutex<BTreeMap<MemSlot, Box<dyn MappedRegion>>>>,
    /// A min heap of MemSlot numbers that were used and then removed and can now be re-used
    mem_slot_gaps: Arc<Mutex<BinaryHeap<Reverse<MemSlot>>>>,
}

impl KvmVm {
    /// Constructs a new `KvmVm` using the given `Kvm` instance.
    pub fn new(kvm: &Kvm, guest_mem: GuestMemory) -> Result<KvmVm> {
        // Safe because we know kvm is a real kvm fd as this module is the only one that can make
        // Kvm objects.
        let ret = unsafe { ioctl_with_val(kvm, KVM_CREATE_VM(), kvm.get_vm_type()) };
        if ret < 0 {
            return errno_result();
        }
        // Safe because we verify that ret is valid and we own the fd.
        let vm_descriptor = unsafe { SafeDescriptor::from_raw_descriptor(ret) };
        guest_mem.with_regions(|index, guest_addr, size, host_addr, _, _| {
            unsafe {
                // Safe because the guest regions are guaranteed not to overlap.
                set_user_memory_region(
                    &vm_descriptor,
                    index as MemSlot,
                    false,
                    false,
                    guest_addr.offset(),
                    size as u64,
                    host_addr as *mut u8,
                )
            }
        })?;

        Ok(KvmVm {
            kvm: kvm.try_clone()?,
            vm: vm_descriptor,
            guest_mem,
            mem_regions: Arc::new(Mutex::new(BTreeMap::new())),
            mem_slot_gaps: Arc::new(Mutex::new(BinaryHeap::new())),
        })
    }

    fn create_vcpu(&self, id: usize) -> Result<KvmVcpu> {
        let run_mmap_size = self.kvm.get_vcpu_mmap_size()?;

        // Safe because we know that our file is a VM fd and we verify the return result.
        let fd = unsafe { ioctl_with_val(self, KVM_CREATE_VCPU(), c_ulong::try_from(id).unwrap()) };
        if fd < 0 {
            return errno_result();
        }

        // Wrap the vcpu now in case the following ? returns early. This is safe because we verified
        // the value of the fd and we own the fd.
        let vcpu = unsafe { SafeDescriptor::from_raw_descriptor(fd) };

        let run_mmap = MemoryMappingBuilder::new(run_mmap_size)
            .from_descriptor(&vcpu)
            .build()
            .map_err(|_| Error::new(ENOSPC))?;

        Ok(KvmVcpu {
            vm: self.vm.try_clone()?,
            vcpu,
            id,
            run_mmap,
            vcpu_run_handle_fingerprint: Default::default(),
        })
    }

    /// Creates an in kernel interrupt controller.
    ///
    /// See the documentation on the KVM_CREATE_IRQCHIP ioctl.
    pub fn create_irq_chip(&self) -> Result<()> {
        // Safe because we know that our file is a VM fd and we verify the return result.
        let ret = unsafe { ioctl(self, KVM_CREATE_IRQCHIP()) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Sets the level on the given irq to 1 if `active` is true, and 0 otherwise.
    pub fn set_irq_line(&self, irq: u32, active: bool) -> Result<()> {
        let mut irq_level = kvm_irq_level::default();
        irq_level.__bindgen_anon_1.irq = irq;
        irq_level.level = if active { 1 } else { 0 };

        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IRQ_LINE(), &irq_level) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Registers an event that will, when signalled, trigger the `gsi` irq, and `resample_evt`
    /// ( when not None ) will be triggered when the irqchip is resampled.
    pub fn register_irqfd(
        &self,
        gsi: u32,
        evt: &Event,
        resample_evt: Option<&Event>,
    ) -> Result<()> {
        let mut irqfd = kvm_irqfd {
            fd: evt.as_raw_descriptor() as u32,
            gsi,
            ..Default::default()
        };

        if let Some(r_evt) = resample_evt {
            irqfd.flags = KVM_IRQFD_FLAG_RESAMPLE;
            irqfd.resamplefd = r_evt.as_raw_descriptor() as u32;
        }

        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IRQFD(), &irqfd) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Unregisters an event that was previously registered with
    /// `register_irqfd`.
    ///
    /// The `evt` and `gsi` pair must be the same as the ones passed into
    /// `register_irqfd`.
    pub fn unregister_irqfd(&self, gsi: u32, evt: &Event) -> Result<()> {
        let irqfd = kvm_irqfd {
            fd: evt.as_raw_descriptor() as u32,
            gsi,
            flags: KVM_IRQFD_FLAG_DEASSIGN,
            ..Default::default()
        };
        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IRQFD(), &irqfd) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Sets the GSI routing table, replacing any table set with previous calls to
    /// `set_gsi_routing`.
    pub fn set_gsi_routing(&self, routes: &[IrqRoute]) -> Result<()> {
        let mut irq_routing =
            vec_with_array_field::<kvm_irq_routing, kvm_irq_routing_entry>(routes.len());
        irq_routing[0].nr = routes.len() as u32;

        // Safe because we ensured there is enough space in irq_routing to hold the number of
        // route entries.
        let irq_routes = unsafe { irq_routing[0].entries.as_mut_slice(routes.len()) };
        for (route, irq_route) in routes.iter().zip(irq_routes.iter_mut()) {
            *irq_route = kvm_irq_routing_entry::from(route);
        }

        let ret = unsafe { ioctl_with_ref(self, KVM_SET_GSI_ROUTING(), &irq_routing[0]) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    fn ioeventfd(
        &self,
        evt: &Event,
        addr: IoEventAddress,
        datamatch: Datamatch,
        deassign: bool,
    ) -> Result<()> {
        let (do_datamatch, datamatch_value, datamatch_len) = match datamatch {
            Datamatch::AnyLength => (false, 0, 0),
            Datamatch::U8(v) => match v {
                Some(u) => (true, u as u64, 1),
                None => (false, 0, 1),
            },
            Datamatch::U16(v) => match v {
                Some(u) => (true, u as u64, 2),
                None => (false, 0, 2),
            },
            Datamatch::U32(v) => match v {
                Some(u) => (true, u as u64, 4),
                None => (false, 0, 4),
            },
            Datamatch::U64(v) => match v {
                Some(u) => (true, u as u64, 8),
                None => (false, 0, 8),
            },
        };
        let mut flags = 0;
        if deassign {
            flags |= 1 << kvm_ioeventfd_flag_nr_deassign;
        }
        if do_datamatch {
            flags |= 1 << kvm_ioeventfd_flag_nr_datamatch
        }
        if let IoEventAddress::Pio(_) = addr {
            flags |= 1 << kvm_ioeventfd_flag_nr_pio;
        }
        let ioeventfd = kvm_ioeventfd {
            datamatch: datamatch_value,
            len: datamatch_len,
            addr: match addr {
                IoEventAddress::Pio(p) => p as u64,
                IoEventAddress::Mmio(m) => m,
            },
            fd: evt.as_raw_descriptor(),
            flags,
            ..Default::default()
        };
        // Safe because we know that our file is a VM fd, we know the kernel will only read the
        // correct amount of memory from our pointer, and we verify the return result.
        let ret = unsafe { ioctl_with_ref(self, KVM_IOEVENTFD(), &ioeventfd) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    /// Checks whether a particular KVM-specific capability is available for this VM.
    fn check_raw_capability(&self, capability: KvmCap) -> bool {
        // Safe because we know that our file is a KVM fd, and if the cap is invalid KVM assumes
        // it's an unavailable extension and returns 0.
        unsafe { ioctl_with_val(self, KVM_CHECK_EXTENSION(), capability as c_ulong) == 1 }
    }

    // Currently only used on aarch64, but works on any architecture.
    #[allow(dead_code)]
    /// Enables a KVM-specific capability for this VM, with the given arguments.
    ///
    /// # Safety
    /// This function is marked as unsafe because `args` may be interpreted as pointers for some
    /// capabilities. The caller must ensure that any pointers passed in the `args` array are
    /// allocated as the kernel expects, and that mutable pointers are owned.
    unsafe fn enable_raw_capability(
        &self,
        capability: KvmCap,
        flags: u32,
        args: &[u64; 4],
    ) -> Result<()> {
        let kvm_cap = kvm_enable_cap {
            cap: capability as u32,
            args: *args,
            flags,
            ..Default::default()
        };
        // Safe because we allocated the struct and we know the kernel will read exactly the size of
        // the struct, and because we assume the caller has allocated the args appropriately.
        let ret = ioctl_with_ref(self, KVM_ENABLE_CAP(), &kvm_cap);
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }
}

impl Vm for KvmVm {
    fn try_clone(&self) -> Result<Self> {
        Ok(KvmVm {
            kvm: self.kvm.try_clone()?,
            vm: self.vm.try_clone()?,
            guest_mem: self.guest_mem.clone(),
            mem_regions: self.mem_regions.clone(),
            mem_slot_gaps: self.mem_slot_gaps.clone(),
        })
    }

    fn check_capability(&self, c: VmCap) -> bool {
        if let Some(val) = self.check_capability_arch(c) {
            return val;
        }
        match c {
            VmCap::DirtyLog => true,
            VmCap::PvClock => false,
            VmCap::PvClockSuspend => self.check_raw_capability(KvmCap::KvmclockCtrl),
            VmCap::Protected => self.check_raw_capability(KvmCap::ArmProtectedVm),
        }
    }

    fn get_memory(&self) -> &GuestMemory {
        &self.guest_mem
    }

    fn add_memory_region(
        &mut self,
        guest_addr: GuestAddress,
        mem: Box<dyn MappedRegion>,
        read_only: bool,
        log_dirty_pages: bool,
    ) -> Result<MemSlot> {
        let pgsz = pagesize() as u64;
        // KVM require to set the user memory region with page size aligned size. Safe to extend
        // the mem.size() to be page size aligned because the mmap will round up the size to be
        // page size aligned if it is not.
        let size = (mem.size() as u64 + pgsz - 1) / pgsz * pgsz;
        let end_addr = guest_addr
            .checked_add(size)
            .ok_or_else(|| Error::new(EOVERFLOW))?;
        if self.guest_mem.range_overlap(guest_addr, end_addr) {
            return Err(Error::new(ENOSPC));
        }
        let mut regions = self.mem_regions.lock();
        let mut gaps = self.mem_slot_gaps.lock();
        let slot = match gaps.pop() {
            Some(gap) => gap.0,
            None => (regions.len() + self.guest_mem.num_regions() as usize) as MemSlot,
        };

        // Safe because we check that the given guest address is valid and has no overlaps. We also
        // know that the pointer and size are correct because the MemoryMapping interface ensures
        // this. We take ownership of the memory mapping so that it won't be unmapped until the slot
        // is removed.
        let res = unsafe {
            set_user_memory_region(
                &self.vm,
                slot,
                read_only,
                log_dirty_pages,
                guest_addr.offset() as u64,
                size,
                mem.as_ptr(),
            )
        };

        if let Err(e) = res {
            gaps.push(Reverse(slot));
            return Err(e);
        }
        regions.insert(slot, mem);
        Ok(slot)
    }

    fn msync_memory_region(&mut self, slot: MemSlot, offset: usize, size: usize) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        let mem = regions.get_mut(&slot).ok_or_else(|| Error::new(ENOENT))?;

        mem.msync(offset, size).map_err(|err| match err {
            MmapError::InvalidAddress => Error::new(EFAULT),
            MmapError::NotPageAligned => Error::new(EINVAL),
            MmapError::SystemCallFailed(e) => e,
            _ => Error::new(EIO),
        })
    }

    fn remove_memory_region(&mut self, slot: MemSlot) -> Result<Box<dyn MappedRegion>> {
        let mut regions = self.mem_regions.lock();
        if !regions.contains_key(&slot) {
            return Err(Error::new(ENOENT));
        }
        // Safe because the slot is checked against the list of memory slots.
        unsafe {
            set_user_memory_region(&self.vm, slot, false, false, 0, 0, std::ptr::null_mut())?;
        }
        self.mem_slot_gaps.lock().push(Reverse(slot));
        // This remove will always succeed because of the contains_key check above.
        Ok(regions.remove(&slot).unwrap())
    }

    fn create_device(&self, kind: DeviceKind) -> Result<SafeDescriptor> {
        let device = if let Some(dev) = self.get_device_params_arch(kind) {
            dev
        } else {
            match kind {
                DeviceKind::Vfio => kvm_create_device {
                    type_: kvm_device_type_KVM_DEV_TYPE_VFIO,
                    fd: 0,
                    flags: 0,
                },

                // ARM has additional DeviceKinds, so it needs the catch-all pattern
                #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
                _ => return Err(Error::new(libc::ENXIO)),
            }
        };

        // Safe because we know that our file is a VM fd, we know the kernel will only write correct
        // amount of memory to our pointer, and we verify the return result.
        let ret = unsafe { base::ioctl_with_ref(self, KVM_CREATE_DEVICE(), &device) };
        if ret == 0 {
            // Safe because we verify that ret is valid and we own the fd.
            Ok(unsafe { SafeDescriptor::from_raw_descriptor(device.fd as i32) })
        } else {
            errno_result()
        }
    }

    fn get_dirty_log(&self, slot: MemSlot, dirty_log: &mut [u8]) -> Result<()> {
        let regions = self.mem_regions.lock();
        let mmap = regions.get(&slot).ok_or_else(|| Error::new(ENOENT))?;
        // Ensures that there are as many bytes in dirty_log as there are pages in the mmap.
        if dirty_log_bitmap_size(mmap.size()) > dirty_log.len() {
            return Err(Error::new(EINVAL));
        }

        let mut dirty_log_kvm = kvm_dirty_log {
            slot,
            ..Default::default()
        };
        dirty_log_kvm.__bindgen_anon_1.dirty_bitmap = dirty_log.as_ptr() as *mut c_void;
        // Safe because the `dirty_bitmap` pointer assigned above is guaranteed to be valid (because
        // it's from a slice) and we checked that it will be large enough to hold the entire log.
        let ret = unsafe { ioctl_with_ref(self, KVM_GET_DIRTY_LOG(), &dirty_log_kvm) };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    fn register_ioevent(
        &mut self,
        evt: &Event,
        addr: IoEventAddress,
        datamatch: Datamatch,
    ) -> Result<()> {
        self.ioeventfd(evt, addr, datamatch, false)
    }

    fn unregister_ioevent(
        &mut self,
        evt: &Event,
        addr: IoEventAddress,
        datamatch: Datamatch,
    ) -> Result<()> {
        self.ioeventfd(evt, addr, datamatch, true)
    }

    fn handle_io_events(&self, _addr: IoEventAddress, _data: &[u8]) -> Result<()> {
        // KVM delivers IO events in-kernel with ioeventfds, so this is a no-op
        Ok(())
    }

    fn get_pvclock(&self) -> Result<ClockState> {
        self.get_pvclock_arch()
    }

    fn set_pvclock(&self, state: &ClockState) -> Result<()> {
        self.set_pvclock_arch(state)
    }

    fn add_fd_mapping(
        &mut self,
        slot: u32,
        offset: usize,
        size: usize,
        fd: &dyn AsRawFd,
        fd_offset: u64,
        prot: Protection,
    ) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        let region = regions.get_mut(&slot).ok_or_else(|| Error::new(EINVAL))?;

        match region.add_fd_mapping(offset, size, fd, fd_offset, prot) {
            Ok(()) => Ok(()),
            Err(MmapError::SystemCallFailed(e)) => Err(e),
            Err(_) => Err(Error::new(EIO)),
        }
    }

    fn remove_mapping(&mut self, slot: u32, offset: usize, size: usize) -> Result<()> {
        let mut regions = self.mem_regions.lock();
        let region = regions.get_mut(&slot).ok_or_else(|| Error::new(EINVAL))?;

        match region.remove_mapping(offset, size) {
            Ok(()) => Ok(()),
            Err(MmapError::SystemCallFailed(e)) => Err(e),
            Err(_) => Err(Error::new(EIO)),
        }
    }
}

impl AsRawDescriptor for KvmVm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.vm.as_raw_descriptor()
    }
}

/// A wrapper around using a KVM Vcpu.
pub struct KvmVcpu {
    vm: SafeDescriptor,
    vcpu: SafeDescriptor,
    id: usize,
    run_mmap: MemoryMapping,
    vcpu_run_handle_fingerprint: Arc<AtomicU64>,
}

pub(super) struct VcpuThread {
    run: *mut kvm_run,
    signal_num: Option<c_int>,
}

thread_local!(static VCPU_THREAD: RefCell<Option<VcpuThread>> = RefCell::new(None));

impl Vcpu for KvmVcpu {
    fn try_clone(&self) -> Result<Self> {
        let vm = self.vm.try_clone()?;
        let vcpu = self.vcpu.try_clone()?;
        let run_mmap = MemoryMappingBuilder::new(self.run_mmap.size())
            .from_descriptor(&vcpu)
            .build()
            .map_err(|_| Error::new(ENOSPC))?;
        let vcpu_run_handle_fingerprint = self.vcpu_run_handle_fingerprint.clone();

        Ok(KvmVcpu {
            vm,
            vcpu,
            id: self.id,
            run_mmap,
            vcpu_run_handle_fingerprint,
        })
    }

    fn as_vcpu(&self) -> &dyn Vcpu {
        self
    }

    #[allow(clippy::cast_ptr_alignment)]
    fn take_run_handle(&self, signal_num: Option<c_int>) -> Result<VcpuRunHandle> {
        fn vcpu_run_handle_drop() {
            VCPU_THREAD.with(|v| {
                // This assumes that a failure in `BlockedSignal::new` means the signal is already
                // blocked and there it should not be unblocked on exit.
                let _blocked_signal = &(*v.borrow())
                    .as_ref()
                    .and_then(|state| state.signal_num)
                    .map(BlockedSignal::new);

                *v.borrow_mut() = None;
            });
        }

        // Prevent `vcpu_run_handle_drop` from being called until we actually setup the signal
        // blocking. The handle needs to be made now so that we can use the fingerprint.
        let vcpu_run_handle = ManuallyDrop::new(VcpuRunHandle::new(vcpu_run_handle_drop));

        // AcqRel ordering is sufficient to ensure only one thread gets to set its fingerprint to
        // this Vcpu and subsequent `run` calls will see the fingerprint.
        if self
            .vcpu_run_handle_fingerprint
            .compare_exchange(
                0,
                vcpu_run_handle.fingerprint().as_u64(),
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return Err(Error::new(EBUSY));
        }

        // Block signal while we add -- if a signal fires (very unlikely,
        // as this means something is trying to pause the vcpu before it has
        // even started) it'll try to grab the read lock while this write
        // lock is grabbed and cause a deadlock.
        // Assuming that a failure to block means it's already blocked.
        let _blocked_signal = signal_num.map(BlockedSignal::new);

        VCPU_THREAD.with(|v| {
            if v.borrow().is_none() {
                *v.borrow_mut() = Some(VcpuThread {
                    run: self.run_mmap.as_ptr() as *mut kvm_run,
                    signal_num,
                });
                Ok(())
            } else {
                Err(Error::new(EBUSY))
            }
        })?;

        Ok(ManuallyDrop::into_inner(vcpu_run_handle))
    }

    fn id(&self) -> usize {
        self.id
    }

    #[allow(clippy::cast_ptr_alignment)]
    fn set_immediate_exit(&self, exit: bool) {
        // Safe because we know we mapped enough memory to hold the kvm_run struct because the
        // kernel told us how large it was. The pointer is page aligned so casting to a different
        // type is well defined, hence the clippy allow attribute.
        let run = unsafe { &mut *(self.run_mmap.as_ptr() as *mut kvm_run) };
        run.immediate_exit = if exit { 1 } else { 0 };
    }

    fn set_local_immediate_exit(exit: bool) {
        VCPU_THREAD.with(|v| {
            if let Some(state) = &(*v.borrow()) {
                unsafe {
                    (*state.run).immediate_exit = if exit { 1 } else { 0 };
                };
            }
        });
    }

    fn set_local_immediate_exit_fn(&self) -> extern "C" fn() {
        extern "C" fn f() {
            KvmVcpu::set_local_immediate_exit(true);
        }
        f
    }

    #[allow(clippy::cast_ptr_alignment)]
    fn set_data(&self, data: &[u8]) -> Result<()> {
        // Safe because we know we mapped enough memory to hold the kvm_run struct because the
        // kernel told us how large it was. The pointer is page aligned so casting to a different
        // type is well defined, hence the clippy allow attribute.
        let run = unsafe { &mut *(self.run_mmap.as_ptr() as *mut kvm_run) };
        match run.exit_reason {
            KVM_EXIT_IO => {
                let run_start = run as *mut kvm_run as *mut u8;
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let io = unsafe { run.__bindgen_anon_1.io };
                if io.direction as u32 != KVM_EXIT_IO_IN {
                    return Err(Error::new(EINVAL));
                }
                let data_size = (io.count as usize) * (io.size as usize);
                if data_size != data.len() {
                    return Err(Error::new(EINVAL));
                }
                // The data_offset is defined by the kernel to be some number of bytes into the
                // kvm_run structure, which we have fully mmap'd.
                unsafe {
                    let data_ptr = run_start.offset(io.data_offset as isize);
                    copy_nonoverlapping(data.as_ptr(), data_ptr, data_size);
                }
                Ok(())
            }
            KVM_EXIT_MMIO => {
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let mmio = unsafe { &mut run.__bindgen_anon_1.mmio };
                if mmio.is_write != 0 {
                    return Err(Error::new(EINVAL));
                }
                let len = mmio.len as usize;
                if len != data.len() {
                    return Err(Error::new(EINVAL));
                }
                mmio.data[..len].copy_from_slice(data);
                Ok(())
            }
            KVM_EXIT_HYPERV => {
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let hyperv = unsafe { &mut run.__bindgen_anon_1.hyperv };
                if hyperv.type_ != KVM_EXIT_HYPERV_HCALL {
                    return Err(Error::new(EINVAL));
                }
                let hcall = unsafe { &mut hyperv.u.hcall };
                if data.len() != std::mem::size_of::<u64>() {
                    return Err(Error::new(EINVAL));
                }
                hcall.result.to_ne_bytes().copy_from_slice(data);
                Ok(())
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn pvclock_ctrl(&self) -> Result<()> {
        self.pvclock_ctrl_arch()
    }

    fn set_signal_mask(&self, signals: &[c_int]) -> Result<()> {
        let sigset = signal::create_sigset(signals)?;

        let mut kvm_sigmask = vec_with_array_field::<kvm_signal_mask, sigset_t>(1);
        // Rust definition of sigset_t takes 128 bytes, but the kernel only
        // expects 8-bytes structure, so we can't write
        // kvm_sigmask.len  = size_of::<sigset_t>() as u32;
        kvm_sigmask[0].len = 8;
        // Ensure the length is not too big.
        const _ASSERT: usize = size_of::<sigset_t>() - 8usize;

        // Safe as we allocated exactly the needed space
        unsafe {
            copy_nonoverlapping(
                &sigset as *const sigset_t as *const u8,
                kvm_sigmask[0].sigset.as_mut_ptr(),
                8,
            );
        }

        let ret = unsafe {
            // The ioctl is safe because the kernel will only read from the
            // kvm_signal_mask structure.
            ioctl_with_ref(self, KVM_SET_SIGNAL_MASK(), &kvm_sigmask[0])
        };
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    unsafe fn enable_raw_capability(&self, cap: u32, args: &[u64; 4]) -> Result<()> {
        let kvm_cap = kvm_enable_cap {
            cap,
            args: *args,
            ..Default::default()
        };
        // Safe because we allocated the struct and we know the kernel will read exactly the size of
        // the struct, and because we assume the caller has allocated the args appropriately.
        let ret = ioctl_with_ref(self, KVM_ENABLE_CAP(), &kvm_cap);
        if ret == 0 {
            Ok(())
        } else {
            errno_result()
        }
    }

    #[allow(clippy::cast_ptr_alignment)]
    // The pointer is page aligned so casting to a different type is well defined, hence the clippy
    // allow attribute.
    fn run(&self, run_handle: &VcpuRunHandle) -> Result<VcpuExit> {
        // Acquire is used to ensure this check is ordered after the `compare_exchange` in `run`.
        if self
            .vcpu_run_handle_fingerprint
            .load(std::sync::atomic::Ordering::Acquire)
            != run_handle.fingerprint().as_u64()
        {
            panic!("invalid VcpuRunHandle used to run Vcpu");
        }

        // Safe because we know that our file is a VCPU fd and we verify the return result.
        let ret = unsafe { ioctl(self, KVM_RUN()) };
        if ret != 0 {
            return errno_result();
        }

        // Safe because we know we mapped enough memory to hold the kvm_run struct because the
        // kernel told us how large it was.
        let run = unsafe { &*(self.run_mmap.as_ptr() as *const kvm_run) };
        match run.exit_reason {
            KVM_EXIT_IO => {
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let io = unsafe { run.__bindgen_anon_1.io };
                let port = io.port;
                let size = (io.count as usize) * (io.size as usize);
                match io.direction as u32 {
                    KVM_EXIT_IO_IN => Ok(VcpuExit::IoIn { port, size }),
                    KVM_EXIT_IO_OUT => {
                        let mut data = [0; 8];
                        let run_start = run as *const kvm_run as *const u8;
                        // The data_offset is defined by the kernel to be some number of bytes
                        // into the kvm_run structure, which we have fully mmap'd.
                        unsafe {
                            let data_ptr = run_start.offset(io.data_offset as isize);
                            copy_nonoverlapping(data_ptr, data.as_mut_ptr(), min(size, data.len()));
                        }
                        Ok(VcpuExit::IoOut { port, size, data })
                    }
                    _ => Err(Error::new(EINVAL)),
                }
            }
            KVM_EXIT_MMIO => {
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let mmio = unsafe { &run.__bindgen_anon_1.mmio };
                let address = mmio.phys_addr;
                let size = min(mmio.len as usize, mmio.data.len());
                if mmio.is_write != 0 {
                    Ok(VcpuExit::MmioWrite {
                        address,
                        size,
                        data: mmio.data,
                    })
                } else {
                    Ok(VcpuExit::MmioRead { address, size })
                }
            }
            KVM_EXIT_IOAPIC_EOI => {
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let vector = unsafe { run.__bindgen_anon_1.eoi.vector };
                Ok(VcpuExit::IoapicEoi { vector })
            }
            KVM_EXIT_HYPERV => {
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let hyperv = unsafe { &run.__bindgen_anon_1.hyperv };
                match hyperv.type_ as u32 {
                    KVM_EXIT_HYPERV_SYNIC => {
                        let synic = unsafe { &hyperv.u.synic };
                        Ok(VcpuExit::HypervSynic {
                            msr: synic.msr,
                            control: synic.control,
                            evt_page: synic.evt_page,
                            msg_page: synic.msg_page,
                        })
                    }
                    KVM_EXIT_HYPERV_HCALL => {
                        let hcall = unsafe { &hyperv.u.hcall };
                        Ok(VcpuExit::HypervHcall {
                            input: hcall.input,
                            params: hcall.params,
                        })
                    }
                    _ => Err(Error::new(EINVAL)),
                }
            }
            KVM_EXIT_UNKNOWN => Ok(VcpuExit::Unknown),
            KVM_EXIT_EXCEPTION => Ok(VcpuExit::Exception),
            KVM_EXIT_HYPERCALL => Ok(VcpuExit::Hypercall),
            KVM_EXIT_DEBUG => Ok(VcpuExit::Debug),
            KVM_EXIT_HLT => Ok(VcpuExit::Hlt),
            KVM_EXIT_IRQ_WINDOW_OPEN => Ok(VcpuExit::IrqWindowOpen),
            KVM_EXIT_SHUTDOWN => Ok(VcpuExit::Shutdown),
            KVM_EXIT_FAIL_ENTRY => {
                // Safe because the exit_reason (which comes from the kernel) told us which
                // union field to use.
                let hardware_entry_failure_reason = unsafe {
                    run.__bindgen_anon_1
                        .fail_entry
                        .hardware_entry_failure_reason
                };
                Ok(VcpuExit::FailEntry {
                    hardware_entry_failure_reason,
                })
            }
            KVM_EXIT_INTR => Ok(VcpuExit::Intr),
            KVM_EXIT_SET_TPR => Ok(VcpuExit::SetTpr),
            KVM_EXIT_TPR_ACCESS => Ok(VcpuExit::TprAccess),
            KVM_EXIT_S390_SIEIC => Ok(VcpuExit::S390Sieic),
            KVM_EXIT_S390_RESET => Ok(VcpuExit::S390Reset),
            KVM_EXIT_DCR => Ok(VcpuExit::Dcr),
            KVM_EXIT_NMI => Ok(VcpuExit::Nmi),
            KVM_EXIT_INTERNAL_ERROR => Ok(VcpuExit::InternalError),
            KVM_EXIT_OSI => Ok(VcpuExit::Osi),
            KVM_EXIT_PAPR_HCALL => Ok(VcpuExit::PaprHcall),
            KVM_EXIT_S390_UCONTROL => Ok(VcpuExit::S390Ucontrol),
            KVM_EXIT_WATCHDOG => Ok(VcpuExit::Watchdog),
            KVM_EXIT_S390_TSCH => Ok(VcpuExit::S390Tsch),
            KVM_EXIT_EPR => Ok(VcpuExit::Epr),
            KVM_EXIT_SYSTEM_EVENT => {
                // Safe because we know the exit reason told us this union
                // field is valid
                let event_type = unsafe { run.__bindgen_anon_1.system_event.type_ };
                let event_flags = unsafe { run.__bindgen_anon_1.system_event.flags };
                Ok(VcpuExit::SystemEvent(event_type, event_flags))
            }
            r => panic!("unknown kvm exit reason: {}", r),
        }
    }
}

impl KvmVcpu {
    /// Gets the vcpu's current "multiprocessing state".
    ///
    /// See the documentation for KVM_GET_MP_STATE. This call can only succeed after
    /// a call to `Vm::create_irq_chip`.
    ///
    /// Note that KVM defines the call for both x86 and s390 but we do not expect anyone
    /// to run crosvm on s390.
    pub fn get_mp_state(&self) -> Result<kvm_mp_state> {
        // Safe because we know that our file is a VCPU fd, we know the kernel will only write the
        // correct amount of memory to our pointer, and we verify the return result.
        let mut state: kvm_mp_state = unsafe { std::mem::zeroed() };
        let ret = unsafe { ioctl_with_mut_ref(self, KVM_GET_MP_STATE(), &mut state) };
        if ret < 0 {
            return errno_result();
        }
        Ok(state)
    }

    /// Sets the vcpu's current "multiprocessing state".
    ///
    /// See the documentation for KVM_SET_MP_STATE. This call can only succeed after
    /// a call to `Vm::create_irq_chip`.
    ///
    /// Note that KVM defines the call for both x86 and s390 but we do not expect anyone
    /// to run crosvm on s390.
    pub fn set_mp_state(&self, state: &kvm_mp_state) -> Result<()> {
        let ret = unsafe {
            // The ioctl is safe because the kernel will only read from the kvm_mp_state struct.
            ioctl_with_ref(self, KVM_SET_MP_STATE(), state)
        };
        if ret < 0 {
            return errno_result();
        }
        Ok(())
    }
}

impl AsRawDescriptor for KvmVcpu {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.vcpu.as_raw_descriptor()
    }
}

impl<'a> TryFrom<&'a HypervisorCap> for KvmCap {
    type Error = Error;

    fn try_from(cap: &'a HypervisorCap) -> Result<KvmCap> {
        match cap {
            HypervisorCap::ArmPmuV3 => Ok(KvmCap::ArmPmuV3),
            HypervisorCap::ImmediateExit => Ok(KvmCap::ImmediateExit),
            HypervisorCap::S390UserSigp => Ok(KvmCap::S390UserSigp),
            HypervisorCap::TscDeadlineTimer => Ok(KvmCap::TscDeadlineTimer),
            HypervisorCap::UserMemory => Ok(KvmCap::UserMemory),
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            HypervisorCap::Xcrs => Ok(KvmCap::Xcrs),
        }
    }
}

impl From<&IrqRoute> for kvm_irq_routing_entry {
    fn from(item: &IrqRoute) -> Self {
        match &item.source {
            IrqSource::Irqchip { chip, pin } => kvm_irq_routing_entry {
                gsi: item.gsi,
                type_: KVM_IRQ_ROUTING_IRQCHIP,
                u: kvm_irq_routing_entry__bindgen_ty_1 {
                    irqchip: kvm_irq_routing_irqchip {
                        irqchip: chip_to_kvm_chip(*chip),
                        pin: *pin,
                    },
                },
                ..Default::default()
            },
            IrqSource::Msi { address, data } => kvm_irq_routing_entry {
                gsi: item.gsi,
                type_: KVM_IRQ_ROUTING_MSI,
                u: kvm_irq_routing_entry__bindgen_ty_1 {
                    msi: kvm_irq_routing_msi {
                        address_lo: *address as u32,
                        address_hi: (*address >> 32) as u32,
                        data: *data,
                        ..Default::default()
                    },
                },
                ..Default::default()
            },
        }
    }
}

// Represents a temporarily blocked signal. It will unblock the signal when dropped.
struct BlockedSignal {
    signal_num: c_int,
}

impl BlockedSignal {
    // Returns a `BlockedSignal` if the specified signal can be blocked, otherwise None.
    fn new(signal_num: c_int) -> Option<BlockedSignal> {
        if block_signal(signal_num).is_ok() {
            Some(BlockedSignal { signal_num })
        } else {
            None
        }
    }
}

impl Drop for BlockedSignal {
    fn drop(&mut self) {
        let _ = unblock_signal(self.signal_num).expect("failed to restore signal mask");
    }
}

impl From<&kvm_mp_state> for MPState {
    fn from(item: &kvm_mp_state) -> Self {
        match item.mp_state {
            KVM_MP_STATE_RUNNABLE => MPState::Runnable,
            KVM_MP_STATE_UNINITIALIZED => MPState::Uninitialized,
            KVM_MP_STATE_INIT_RECEIVED => MPState::InitReceived,
            KVM_MP_STATE_HALTED => MPState::Halted,
            KVM_MP_STATE_SIPI_RECEIVED => MPState::SipiReceived,
            KVM_MP_STATE_STOPPED => MPState::Stopped,
            state => {
                error!(
                    "unrecognized kvm_mp_state {}, setting to KVM_MP_STATE_RUNNABLE",
                    state
                );
                MPState::Runnable
            }
        }
    }
}

impl From<&MPState> for kvm_mp_state {
    fn from(item: &MPState) -> Self {
        kvm_mp_state {
            mp_state: match item {
                MPState::Runnable => KVM_MP_STATE_RUNNABLE,
                MPState::Uninitialized => KVM_MP_STATE_UNINITIALIZED,
                MPState::InitReceived => KVM_MP_STATE_INIT_RECEIVED,
                MPState::Halted => KVM_MP_STATE_HALTED,
                MPState::SipiReceived => KVM_MP_STATE_SIPI_RECEIVED,
                MPState::Stopped => KVM_MP_STATE_STOPPED,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::{pagesize, FromRawDescriptor, MemoryMappingArena, MemoryMappingBuilder};
    use std::thread;
    use vm_memory::GuestAddress;

    #[test]
    fn dirty_log_size() {
        let page_size = pagesize();
        assert_eq!(dirty_log_bitmap_size(0), 0);
        assert_eq!(dirty_log_bitmap_size(page_size), 1);
        assert_eq!(dirty_log_bitmap_size(page_size * 8), 1);
        assert_eq!(dirty_log_bitmap_size(page_size * 8 + 1), 2);
        assert_eq!(dirty_log_bitmap_size(page_size * 100), 13);
    }

    #[test]
    fn new() {
        Kvm::new().unwrap();
    }

    #[test]
    fn check_capability() {
        let kvm = Kvm::new().unwrap();
        assert!(kvm.check_capability(&HypervisorCap::UserMemory));
        assert!(!kvm.check_capability(&HypervisorCap::S390UserSigp));
    }

    #[test]
    fn create_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        KvmVm::new(&kvm, gm).unwrap();
    }

    #[test]
    fn clone_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        vm.try_clone().unwrap();
    }

    #[test]
    fn send_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        thread::spawn(move || {
            let _vm = vm;
        })
        .join()
        .unwrap();
    }

    #[test]
    fn check_vm_capability() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        assert!(vm.check_raw_capability(KvmCap::UserMemory));
        // I assume nobody is testing this on s390
        assert!(!vm.check_raw_capability(KvmCap::S390UserSigp));
    }

    #[test]
    fn create_vcpu() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        vm.create_vcpu(0).unwrap();
    }

    #[test]
    fn get_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let obj_addr = GuestAddress(0xf0);
        vm.get_memory().write_obj_at_addr(67u8, obj_addr).unwrap();
        let read_val: u8 = vm.get_memory().read_obj_from_addr(obj_addr).unwrap();
        assert_eq!(read_val, 67u8);
    }

    #[test]
    fn add_memory() {
        let kvm = Kvm::new().unwrap();
        let gm =
            GuestMemory::new(&[(GuestAddress(0), 0x1000), (GuestAddress(0x5000), 0x5000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMappingBuilder::new(mem_size).build().unwrap();
        vm.add_memory_region(GuestAddress(0x1000), Box::new(mem), false, false)
            .unwrap();
        let mem = MemoryMappingBuilder::new(mem_size).build().unwrap();
        vm.add_memory_region(GuestAddress(0x10000), Box::new(mem), false, false)
            .unwrap();
    }

    #[test]
    fn add_memory_ro() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMappingBuilder::new(mem_size).build().unwrap();
        vm.add_memory_region(GuestAddress(0x1000), Box::new(mem), true, false)
            .unwrap();
    }

    #[test]
    fn remove_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMappingBuilder::new(mem_size).build().unwrap();
        let mem_ptr = mem.as_ptr();
        let slot = vm
            .add_memory_region(GuestAddress(0x1000), Box::new(mem), false, false)
            .unwrap();
        let removed_mem = vm.remove_memory_region(slot).unwrap();
        assert_eq!(removed_mem.size(), mem_size);
        assert_eq!(removed_mem.as_ptr(), mem_ptr);
    }

    #[test]
    fn remove_invalid_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        assert!(vm.remove_memory_region(0).is_err());
    }

    #[test]
    fn overlap_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x2000;
        let mem = MemoryMappingBuilder::new(mem_size).build().unwrap();
        assert!(vm
            .add_memory_region(GuestAddress(0x2000), Box::new(mem), false, false)
            .is_err());
    }

    #[test]
    fn sync_memory() {
        let kvm = Kvm::new().unwrap();
        let gm =
            GuestMemory::new(&[(GuestAddress(0), 0x1000), (GuestAddress(0x5000), 0x5000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let mem_size = 0x1000;
        let mem = MemoryMappingArena::new(mem_size).unwrap();
        let slot = vm
            .add_memory_region(GuestAddress(0x1000), Box::new(mem), false, false)
            .unwrap();
        vm.msync_memory_region(slot, mem_size, 0).unwrap();
        assert!(vm.msync_memory_region(slot, mem_size + 1, 0).is_err());
        assert!(vm.msync_memory_region(slot + 1, mem_size, 0).is_err());
    }

    #[test]
    fn register_irqfd() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd1 = Event::new().unwrap();
        let evtfd2 = Event::new().unwrap();
        let evtfd3 = Event::new().unwrap();
        vm.create_irq_chip().unwrap();
        vm.register_irqfd(4, &evtfd1, None).unwrap();
        vm.register_irqfd(8, &evtfd2, None).unwrap();
        vm.register_irqfd(4, &evtfd3, None).unwrap();
        vm.register_irqfd(4, &evtfd3, None).unwrap_err();
    }

    #[test]
    fn unregister_irqfd() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd1 = Event::new().unwrap();
        let evtfd2 = Event::new().unwrap();
        let evtfd3 = Event::new().unwrap();
        vm.create_irq_chip().unwrap();
        vm.register_irqfd(4, &evtfd1, None).unwrap();
        vm.register_irqfd(8, &evtfd2, None).unwrap();
        vm.register_irqfd(4, &evtfd3, None).unwrap();
        vm.unregister_irqfd(4, &evtfd1).unwrap();
        vm.unregister_irqfd(8, &evtfd2).unwrap();
        vm.unregister_irqfd(4, &evtfd3).unwrap();
    }

    #[test]
    fn irqfd_resample() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd1 = Event::new().unwrap();
        let evtfd2 = Event::new().unwrap();
        vm.create_irq_chip().unwrap();
        vm.register_irqfd(4, &evtfd1, Some(&evtfd2)).unwrap();
        vm.unregister_irqfd(4, &evtfd1).unwrap();
        // Ensures the ioctl is actually reading the resamplefd.
        vm.register_irqfd(4, &evtfd1, Some(unsafe { &Event::from_raw_descriptor(-1) }))
            .unwrap_err();
    }

    #[test]
    fn set_signal_mask() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let vcpu = vm.create_vcpu(0).unwrap();
        vcpu.set_signal_mask(&[base::SIGRTMIN() + 0]).unwrap();
    }

    #[test]
    fn vcpu_mmap_size() {
        let kvm = Kvm::new().unwrap();
        let mmap_size = kvm.get_vcpu_mmap_size().unwrap();
        let page_size = pagesize();
        assert!(mmap_size >= page_size);
        assert!(mmap_size % page_size == 0);
    }

    #[test]
    fn register_ioevent() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd = Event::new().unwrap();
        vm.register_ioevent(&evtfd, IoEventAddress::Pio(0xf4), Datamatch::AnyLength)
            .unwrap();
        vm.register_ioevent(&evtfd, IoEventAddress::Mmio(0x1000), Datamatch::AnyLength)
            .unwrap();
        vm.register_ioevent(
            &evtfd,
            IoEventAddress::Pio(0xc1),
            Datamatch::U8(Some(0x7fu8)),
        )
        .unwrap();
        vm.register_ioevent(
            &evtfd,
            IoEventAddress::Pio(0xc2),
            Datamatch::U16(Some(0x1337u16)),
        )
        .unwrap();
        vm.register_ioevent(
            &evtfd,
            IoEventAddress::Pio(0xc4),
            Datamatch::U32(Some(0xdeadbeefu32)),
        )
        .unwrap();
        vm.register_ioevent(
            &evtfd,
            IoEventAddress::Pio(0xc8),
            Datamatch::U64(Some(0xdeadbeefdeadbeefu64)),
        )
        .unwrap();
    }

    #[test]
    fn unregister_ioevent() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x10000)]).unwrap();
        let mut vm = KvmVm::new(&kvm, gm).unwrap();
        let evtfd = Event::new().unwrap();
        vm.register_ioevent(&evtfd, IoEventAddress::Pio(0xf4), Datamatch::AnyLength)
            .unwrap();
        vm.register_ioevent(&evtfd, IoEventAddress::Mmio(0x1000), Datamatch::AnyLength)
            .unwrap();
        vm.register_ioevent(
            &evtfd,
            IoEventAddress::Mmio(0x1004),
            Datamatch::U8(Some(0x7fu8)),
        )
        .unwrap();
        vm.unregister_ioevent(&evtfd, IoEventAddress::Pio(0xf4), Datamatch::AnyLength)
            .unwrap();
        vm.unregister_ioevent(&evtfd, IoEventAddress::Mmio(0x1000), Datamatch::AnyLength)
            .unwrap();
        vm.unregister_ioevent(
            &evtfd,
            IoEventAddress::Mmio(0x1004),
            Datamatch::U8(Some(0x7fu8)),
        )
        .unwrap();
    }
}
