// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Bus;
use base::{error, Error, Event, Result};
use hypervisor::kvm::KvmVcpu;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use hypervisor::VmAArch64;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use hypervisor::VmX86_64;
use hypervisor::{HypervisorCap, IrqRoute, MPState, Vcpu};
use kvm_sys::kvm_mp_state;
use resources::SystemAllocator;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86_64;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub use x86_64::*;

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
mod aarch64;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub use aarch64::*;

use crate::{IrqChip, IrqChipCap, IrqEventIndex, VcpuRunState};

/// This IrqChip only works with Kvm so we only implement it for KvmVcpu.
impl IrqChip for KvmKernelIrqChip {
    /// Add a vcpu to the irq chip.
    fn add_vcpu(&mut self, vcpu_id: usize, vcpu: &dyn Vcpu) -> Result<()> {
        let vcpu: &KvmVcpu = vcpu
            .downcast_ref()
            .expect("KvmKernelIrqChip::add_vcpu called with non-KvmVcpu");
        self.vcpus.lock()[vcpu_id] = Some(vcpu.try_clone()?);
        Ok(())
    }

    /// Register an event that can trigger an interrupt for a particular GSI.
    fn register_irq_event(
        &mut self,
        irq: u32,
        irq_event: &Event,
        resample_event: Option<&Event>,
    ) -> Result<Option<IrqEventIndex>> {
        self.vm.register_irqfd(irq, irq_event, resample_event)?;
        Ok(None)
    }

    /// Unregister an event for a particular GSI.
    fn unregister_irq_event(&mut self, irq: u32, irq_event: &Event) -> Result<()> {
        self.vm.unregister_irqfd(irq, irq_event)
    }

    /// Route an IRQ line to an interrupt controller, or to a particular MSI vector.
    fn route_irq(&mut self, route: IrqRoute) -> Result<()> {
        let mut routes = self.routes.lock();
        routes.retain(|r| r.gsi != route.gsi);

        routes.push(route);

        self.vm.set_gsi_routing(&*routes)
    }

    /// Replace all irq routes with the supplied routes
    fn set_irq_routes(&mut self, routes: &[IrqRoute]) -> Result<()> {
        let mut current_routes = self.routes.lock();
        *current_routes = routes.to_vec();

        self.vm.set_gsi_routing(&*current_routes)
    }

    /// Return a vector of all registered irq numbers and their associated events and event
    /// indices. These should be used by the main thread to wait for irq events.
    /// For the KvmKernelIrqChip, the kernel handles listening to irq events being triggered by
    /// devices, so this function always returns an empty Vec.
    fn irq_event_tokens(&self) -> Result<Vec<(IrqEventIndex, u32, Event)>> {
        Ok(Vec::new())
    }

    /// Either assert or deassert an IRQ line.  Sends to either an interrupt controller, or does
    /// a send_msi if the irq is associated with an MSI.
    /// For the KvmKernelIrqChip this simply calls the KVM_SET_IRQ_LINE ioctl.
    fn service_irq(&mut self, irq: u32, level: bool) -> Result<()> {
        self.vm.set_irq_line(irq, level)
    }

    /// Service an IRQ event by asserting then deasserting an IRQ line. The associated Event
    /// that triggered the irq event will be read from. If the irq is associated with a resample
    /// Event, then the deassert will only happen after an EOI is broadcast for a vector
    /// associated with the irq line.
    /// This function should never be called on KvmKernelIrqChip.
    fn service_irq_event(&mut self, _event_index: IrqEventIndex) -> Result<()> {
        error!("service_irq_event should never be called for KvmKernelIrqChip");
        Ok(())
    }

    /// Broadcast an end of interrupt.
    /// This should never be called on a KvmKernelIrqChip because a KVM vcpu should never exit
    /// with the KVM_EXIT_EOI_BROADCAST reason when an in-kernel irqchip exists.
    fn broadcast_eoi(&self, _vector: u8) -> Result<()> {
        error!("broadcast_eoi should never be called for KvmKernelIrqChip");
        Ok(())
    }

    /// Injects any pending interrupts for `vcpu`.
    /// For KvmKernelIrqChip this is a no-op because KVM is responsible for injecting all
    /// interrupts.
    fn inject_interrupts(&self, _vcpu: &dyn Vcpu) -> Result<()> {
        Ok(())
    }

    /// Notifies the irq chip that the specified VCPU has executed a halt instruction.
    /// For KvmKernelIrqChip this is a no-op because KVM handles VCPU blocking.
    fn halted(&self, _vcpu_id: usize) {}

    /// Blocks until `vcpu` is in a runnable state or until interrupted by
    /// `IrqChip::kick_halted_vcpus`.  Returns `VcpuRunState::Runnable if vcpu is runnable, or
    /// `VcpuRunState::Interrupted` if the wait was interrupted.
    /// For KvmKernelIrqChip this is a no-op and always returns Runnable because KVM handles VCPU
    /// blocking.
    fn wait_until_runnable(&self, _vcpu: &dyn Vcpu) -> Result<VcpuRunState> {
        Ok(VcpuRunState::Runnable)
    }

    /// Makes unrunnable VCPUs return immediately from `wait_until_runnable`.
    /// For KvmKernelIrqChip this is a no-op because KVM handles VCPU blocking.
    fn kick_halted_vcpus(&self) {}

    /// Get the current MP state of the specified VCPU.
    fn get_mp_state(&self, vcpu_id: usize) -> Result<MPState> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => Ok(MPState::from(&vcpu.get_mp_state()?)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Set the current MP state of the specified VCPU.
    fn set_mp_state(&mut self, vcpu_id: usize, state: &MPState) -> Result<()> {
        match self.vcpus.lock().get(vcpu_id) {
            Some(Some(vcpu)) => vcpu.set_mp_state(&kvm_mp_state::from(state)),
            _ => Err(Error::new(libc::ENOENT)),
        }
    }

    /// Attempt to clone this IrqChip instance.
    fn try_clone(&self) -> Result<Self> {
        // Because the KvmKernelIrqchip struct contains arch-specific fields we leave the
        // cloning to arch-specific implementations
        self.arch_try_clone()
    }

    /// Finalize irqchip setup. Should be called once all devices have registered irq events and
    /// been added to the io_bus and mmio_bus.
    /// KvmKernelIrqChip does not need to do anything here.
    fn finalize_devices(
        &mut self,
        _resources: &mut SystemAllocator,
        _io_bus: &Bus,
        _mmio_bus: &Bus,
    ) -> Result<()> {
        Ok(())
    }

    /// The KvmKernelIrqChip doesn't process irq events itself so this function does nothing.
    fn process_delayed_irq_events(&mut self) -> Result<()> {
        Ok(())
    }

    fn check_capability(&self, c: IrqChipCap) -> bool {
        match c {
            IrqChipCap::TscDeadlineTimer => self
                .vm
                .get_hypervisor()
                .check_capability(&HypervisorCap::TscDeadlineTimer),
            IrqChipCap::X2Apic => true,
        }
    }
}

#[cfg(test)]
mod tests {

    use hypervisor::kvm::{Kvm, KvmVm};
    use hypervisor::{MPState, Vm};
    use vm_memory::GuestMemory;

    use crate::irqchip::{IrqChip, KvmKernelIrqChip};

    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    use hypervisor::VmAArch64;
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    use hypervisor::VmX86_64;

    #[test]
    fn create_kvm_kernel_irqchip() {
        let kvm = Kvm::new().expect("failed to instantiate Kvm");
        let mem = GuestMemory::new(&[]).unwrap();
        let vm = KvmVm::new(&kvm, mem).expect("failed to instantiate vm");

        let mut chip = KvmKernelIrqChip::new(vm.try_clone().expect("failed to clone vm"), 1)
            .expect("failed to instantiate KvmKernelIrqChip");

        let vcpu = vm.create_vcpu(0).expect("failed to instantiate vcpu");
        chip.add_vcpu(0, vcpu.as_vcpu())
            .expect("failed to add vcpu");
    }

    #[test]
    fn mp_state() {
        let kvm = Kvm::new().expect("failed to instantiate Kvm");
        let mem = GuestMemory::new(&[]).unwrap();
        let vm = KvmVm::new(&kvm, mem).expect("failed to instantiate vm");

        let mut chip = KvmKernelIrqChip::new(vm.try_clone().expect("failed to clone vm"), 1)
            .expect("failed to instantiate KvmKernelIrqChip");

        let vcpu = vm.create_vcpu(0).expect("failed to instantiate vcpu");
        chip.add_vcpu(0, vcpu.as_vcpu())
            .expect("failed to add vcpu");

        let state = chip.get_mp_state(0).expect("failed to get mp state");
        assert_eq!(state, MPState::Runnable);

        chip.set_mp_state(0, &MPState::Stopped)
            .expect("failed to set mp state");

        let state = chip.get_mp_state(0).expect("failed to get mp state");
        assert_eq!(state, MPState::Stopped);
    }
}
