import enum


class TestOption(enum.Enum):
    # Build and run tests on aarch64 or arm32 only
    BUILD_ARM_ONLY = "build_arm_only"
    # Build and run tests on x86_64 only
    BUILD_X86_ONLY = "build_x86_only"
    # Do not build nor run tests
    DO_NOT_BUILD = "do_not_build"
    # Build but do not run tests
    DO_NOT_RUN = "do_not_run"
    # Build for all platforms, but only run on arm
    RUN_ARM_ONLY = "run_arm_only"
    # Build for all platforms, but only run on x86
    RUN_X86_ONLY = "run_x86_only"
    # Run tests single-threaded
    SINGLE_THREADED = "single_threaded"


# Configuration to restrict how and where tests of a certain crate can
# be build and run.
#
# Please add a bug number when restricting a tests.
CRATE_OPTIONS: dict[str, list[TestOption]] = {
    "aarch64": [TestOption.BUILD_ARM_ONLY],
    "enumn": [TestOption.RUN_X86_ONLY],  # b/203100960
    "cros_async": [TestOption.DO_NOT_BUILD],  # b/202293468
    "crosvm_plugin": [TestOption.BUILD_X86_ONLY],
    "devices": [TestOption.SINGLE_THREADED],
    "disk": [TestOption.RUN_X86_ONLY],  # b/202294155
    "crosvm-fuzz": [TestOption.DO_NOT_BUILD],  # b/194499769
    "hypervisor": [TestOption.RUN_X86_ONLY],  # b/181672912
    "integration_tests": [
        TestOption.SINGLE_THREADED,
        TestOption.RUN_X86_ONLY,  # b/180196508
    ],
    "io_uring": [TestOption.DO_NOT_BUILD],  # b/202294403
    "kvm": [TestOption.RUN_X86_ONLY],  # b/181674144
    "libcras_stub": [TestOption.DO_NOT_BUILD],  # empty stub crate
    "libvda": [TestOption.DO_NOT_BUILD],  # b/202293971
    "system_api_stub": [TestOption.DO_NOT_BUILD],  # empty stub crate
    "x86_64": [TestOption.BUILD_X86_ONLY],
    "sys_util": [TestOption.SINGLE_THREADED],
}
