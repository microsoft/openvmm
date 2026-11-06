// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "aarch64")]

//! aarch64 specifics.

pub mod hypercall;
mod memory;
mod vp;
mod vsm;

#[cfg(feature = "cvm_boot_log")]
use crate::host_params::shim_params::ShimParams;
pub use memory::physical_address_bits;
pub use memory::setup_vtl2_memory;
pub use memory::verify_imported_regions_hash;
pub use vp::setup_vtl2_vp;
pub use vsm::get_isolation_type;

#[cfg(feature = "cvm_boot_log")]
pub fn initialize_serial_io(_: &ShimParams) {}

#[cfg(feature = "cvm_boot_log")]
pub fn uninitialize_serial_io(_: &ShimParams) {}

/// Installs exception vectors, clears BSS, sets up the stack and FPU, and relocates the image.
///
/// # Safety
/// Must be entered with the boot shim register state and the image identity mapped.
// TODO: Use extern "custom" once supported by the minimum Rust version.
#[cfg(minimal_rt)]
// SAFETY: The minimal_rt boot shim has no CRT entry point; this is its sole `_start`.
#[unsafe(no_mangle)]
// SAFETY: Assembly preserves the boot argument and initializes the stack and FPU
// before calling Rust; it terminates through the debug interface if Rust returns.
#[unsafe(naked)]
unsafe extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        // _DYNAMIC must be supplied by the linker script; making it weak would
        // hide a missing dynamic section and break relocation at boot.
        ".extern _DYNAMIC",
        ".balign 0x10",
        "mov x19, x0",
        "adrp x0, _vector_table_el1",
        "add x0, x0, :lo12:_vector_table_el1",
        "msr VBAR_EL1, x0",
        "isb",
        // The stack is in BSS, so no stack or function calls can be used yet.
        "adrp x0, __bss_start",
        "add x0, x0, :lo12:__bss_start",
        "adrp x1, _end",
        "add x1, x1, :lo12:_end",
        "sub x1, x1, x0",
        "1:",
        "cbz x1, 2f",
        "sub x1, x1, 1",
        "strb wzr, [x0, x1]",
        "b 1b",
        "2:",
        "adrp x0, {stack}",
        "add x0, x0, :lo12:{stack}",
        "mov x1, {STACK_COOKIE_LO}",
        "movk x1, {STACK_COOKIE_HI}, lsl 16",
        "str x1, [x0]",
        "add x0, x0, {STACK_SIZE}",
        "sub x0, x0, #8",
        "str x1, [x0]",
        "sub x0, x0, #8",
        "mov sp, x0",
        "str x19, [sp, #-16]!",
        // Rust code may use NEON and FP registers.
        "mrs x0, CPACR_EL1",
        "orr x0, x0, #(3 << 20)",
        "orr x0, x0, #(3 << 16)",
        "msr CPACR_EL1, x0",
        "isb",
        "adrp x0, __ehdr_start",
        "add x0, x0, :lo12:__ehdr_start",
        "mov x1, x0",
        "adrp x2, _DYNAMIC",
        "add x2, x2, :lo12:_DYNAMIC",
        "bl {relocate}",
        "mov x0, xzr",
        "ldr x1, [sp], #16",
        "bl {start}",
        // If Rust returns, enter the platform debug interface or break.
        "mov x0, 6",
        "movk x0, 0x8600, lsl 16",
        "smc #0",
        "brk #0xffff",
        start = sym crate::rt::start,
        relocate = sym minimal_rt::reloc::relocate,
        stack = sym crate::rt::STACK,
        STACK_COOKIE_LO = const (crate::rt::STACK_COOKIE as u16),
        STACK_COOKIE_HI = const ((crate::rt::STACK_COOKIE >> 16) as u16),
        STACK_SIZE = const crate::rt::STACK_SIZE,
    );
}

// Exception reporting cannot rely on a working stack or a relocated image.
#[cfg(minimal_rt)]
core::arch::global_asm! {
    r#"
    .macro SMC_CALL_DEBUG_INTERFACE
        mov     x0, 6
        movk    x0, 0x8600, lsl 16
        smc     #0
        brk     #0xffff
    .endm

    // Values are staged in X5; this clobbers X0-X6.
    // Literal loads are PC-relative and do not require image relocation.
    .macro SET_VP_REGISTER64 vp_reg_name, val
        ldr     x0, =0x100010051
        mov     x1, #0xffffffffffffffff
        mov     x2, #0xfffffffe
        ldr     x3, =\vp_reg_name
        mov     x4, #0
        mov     x6, #0
        hvc     #1
    .endm

    .macro EXCEPTION_ENTRY source_and_kind
    .align 7
        // Connect to the hypervisor even if boot did not get that far.
        mov                 x5, {OHCL_LOADER_OSID}
        SET_VP_REGISTER64   {HV_REGISTER_OSID}

        mov                 x5, \source_and_kind
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P0}
        mrs                 x5, PAR_EL1
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P1}
        mrs                 x5, ELR_EL1
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P2}
        mrs                 x5, ESR_EL1
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P3}
        mov                 x5, 0
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P4}
        mov                 x5, {GUEST_CRASH_CTRL}
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_CTRL}

        SMC_CALL_DEBUG_INTERFACE
        b       .
    .endm

    // The EL1 vector table requires 2KiB alignment.
    .balign 0x800
    .globl _vector_table_el1
    _vector_table_el1:
    // Same exception level, SP_EL0: synchronous, IRQ, FIQ, SError.
    EXCEPTION_ENTRY 0x00
    EXCEPTION_ENTRY 0x01
    EXCEPTION_ENTRY 0x02
    EXCEPTION_ENTRY 0x03

    // Same exception level, SP_ELx.
    EXCEPTION_ENTRY 0x10
    EXCEPTION_ENTRY 0x11
    EXCEPTION_ENTRY 0x12
    EXCEPTION_ENTRY 0x13

    // Lower exception level, AArch64.
    EXCEPTION_ENTRY 0x20
    EXCEPTION_ENTRY 0x21
    EXCEPTION_ENTRY 0x22
    EXCEPTION_ENTRY 0x23

    // Lower exception level, AArch32.
    EXCEPTION_ENTRY 0x30
    EXCEPTION_ENTRY 0x31
    EXCEPTION_ENTRY 0x32
    EXCEPTION_ENTRY 0x33
    "#,
    HV_REGISTER_OSID = const hvdef::HvArm64RegisterName::GuestOsId.0,
    OHCL_LOADER_OSID = const hvdef::hypercall::HvGuestOsMicrosoft::new()
        .with_os_id(1)
        .into_bits(),
    HV_REGISTER_GUEST_CRASH_P0 = const hvdef::HvArm64RegisterName::GuestCrashP0.0,
    HV_REGISTER_GUEST_CRASH_P1 = const hvdef::HvArm64RegisterName::GuestCrashP1.0,
    HV_REGISTER_GUEST_CRASH_P2 = const hvdef::HvArm64RegisterName::GuestCrashP2.0,
    HV_REGISTER_GUEST_CRASH_P3 = const hvdef::HvArm64RegisterName::GuestCrashP3.0,
    HV_REGISTER_GUEST_CRASH_P4 = const hvdef::HvArm64RegisterName::GuestCrashP4.0,
    HV_REGISTER_GUEST_CRASH_CTRL = const hvdef::HvArm64RegisterName::GuestCrashCtl.0,
    GUEST_CRASH_CTRL = const hvdef::GuestCrashCtl::new()
        .with_no_crash_dump(true)
        .with_crash_notify(true).into_bits()
}
