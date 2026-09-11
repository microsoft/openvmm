// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Aarch64 entry point and support.

#![cfg(target_arch = "aarch64")]

use super::Scope;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Relaxed;

const GIC_DISTRIBUTOR_BASE: usize = tmk_protocol::aarch64::GIC_DISTRIBUTOR_BASE as usize;
const GIC_REDISTRIBUTOR_BASE: usize = tmk_protocol::aarch64::GIC_REDISTRIBUTOR_BASE as usize;
const GIC_REDISTRIBUTOR_SGI_BASE: usize = GIC_REDISTRIBUTOR_BASE + 0x1_0000;

const GICD_CTLR: usize = GIC_DISTRIBUTOR_BASE;
const GICD_IGROUPR0: usize = GIC_DISTRIBUTOR_BASE + 0x80;
const GICD_ISENABLER0: usize = GIC_DISTRIBUTOR_BASE + 0x100;
const GICD_ICENABLER0: usize = GIC_DISTRIBUTOR_BASE + 0x180;
const GICD_ISPENDR0: usize = GIC_DISTRIBUTOR_BASE + 0x200;
const GICD_IPRIORITYR0: usize = GIC_DISTRIBUTOR_BASE + 0x400;
const GICR_WAKER: usize = GIC_REDISTRIBUTOR_BASE + 0x14;
const GICR_IGROUPR0: usize = GIC_REDISTRIBUTOR_SGI_BASE + 0x80;
const GICR_ISENABLER0: usize = GIC_REDISTRIBUTOR_SGI_BASE + 0x100;
const GICR_ICENABLER0: usize = GIC_REDISTRIBUTOR_SGI_BASE + 0x180;
const GICR_IPRIORITYR0: usize = GIC_REDISTRIBUTOR_SGI_BASE + 0x400;

const GIC_SPECIAL_INTID: u32 = 1020;
const DAIF_IRQ_MASK: u64 = 1 << 7;
const GICD_CTLR_ENABLE_GRP1_AND_ARE: u32 = (1 << 1) | (1 << 4);

/// GIC interrupt ID used by the architectural virtual timer.
pub const VIRTUAL_TIMER_PPI: u32 = tmk_protocol::aarch64::VIRTUAL_TIMER_PPI;

static ARCH_INITIALIZED: AtomicBool = AtomicBool::new(false);
static mut IRQ_HANDLER: [usize; 2] = [0; 2];

/// A context passed to the IRQ handler.
pub struct IrqContext {
    /// The GIC interrupt ID being handled.
    pub intid: u32,
}

#[cfg(minimal_rt)]
mod entry {
    core::arch::global_asm! {
        ".extern _DYNAMIC",
        ".globl _start",
        "_start:",
        "mov x19, x0",
        "adrp x1, {stack}",
        "add x1, x1, :lo12:{stack}",
        "add x1, x1, {STACK_SIZE}",
        "mov sp, x1",

        // Enable the FPU.
        "mrs     x0, CPACR_EL1",
        "orr     x0, x0, #(3 << 20)",
        "orr     x0, x0, #(3 << 16)",
        "msr     CPACR_EL1, x0",
        "isb",

        "adrp x0, __ehdr_start",
        "add x0, x0, :lo12:__ehdr_start",
        "mov x1, x0",
        "adrp x2, _DYNAMIC",
        "add x2, x2, :lo12:_DYNAMIC",
        "bl {relocate}",
        "mov x0, x19",
        "b {entry}",
        relocate = sym minimal_rt::reloc::relocate,
        stack = sym STACK,
        entry = sym crate::entry,
        STACK_SIZE = const STACK_SIZE,
    }

    const STACK_SIZE: usize = 16384;
    #[repr(C, align(16))]
    struct Stack([u8; STACK_SIZE]);
    static mut STACK: Stack = Stack([0; STACK_SIZE]);
}

core::arch::global_asm! {
    ".balign 2048",
    ".globl {exception_vectors}",
    "{exception_vectors}:",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x080",
    "b irq_exception",
    ".org {exception_vectors} + 0x100",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x180",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x200",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x280",
    "b irq_exception",
    ".org {exception_vectors} + 0x300",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x380",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x400",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x480",
    "b irq_exception",
    ".org {exception_vectors} + 0x500",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x580",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x600",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x680",
    "b irq_exception",
    ".org {exception_vectors} + 0x700",
    "b unexpected_exception",
    ".org {exception_vectors} + 0x780",
    "b unexpected_exception",

    "irq_exception:",
    // IRQs are asynchronous, so preserve the complete integer and FP/SIMD
    // context rather than only the registers that are callee-saved by AAPCS64.
    // The 784-byte frame remains 16-byte aligned: 256 bytes for GPRs, 512
    // bytes for q0-q31, and 16 bytes for FPCR and FPSR.
    "sub sp, sp, #784",
    // Preserve the interrupted general-purpose register context.
    "stp x0, x1, [sp, #0]",
    "stp x2, x3, [sp, #16]",
    "stp x4, x5, [sp, #32]",
    "stp x6, x7, [sp, #48]",
    "stp x8, x9, [sp, #64]",
    "stp x10, x11, [sp, #80]",
    "stp x12, x13, [sp, #96]",
    "stp x14, x15, [sp, #112]",
    "stp x16, x17, [sp, #128]",
    "stp x18, x19, [sp, #144]",
    "stp x20, x21, [sp, #160]",
    "stp x22, x23, [sp, #176]",
    "stp x24, x25, [sp, #192]",
    "stp x26, x27, [sp, #208]",
    "stp x28, x29, [sp, #224]",
    "str x30, [sp, #240]",
    // Preserve the interrupted SIMD and floating-point register context.
    "stp q0, q1, [sp, #256]",
    "stp q2, q3, [sp, #288]",
    "stp q4, q5, [sp, #320]",
    "stp q6, q7, [sp, #352]",
    "stp q8, q9, [sp, #384]",
    "stp q10, q11, [sp, #416]",
    "stp q12, q13, [sp, #448]",
    "stp q14, q15, [sp, #480]",
    "stp q16, q17, [sp, #512]",
    "stp q18, q19, [sp, #544]",
    "stp q20, q21, [sp, #576]",
    "stp q22, q23, [sp, #608]",
    "stp q24, q25, [sp, #640]",
    "stp q26, q27, [sp, #672]",
    "stp q28, q29, [sp, #704]",
    "stp q30, q31, [sp, #736]",
    // Preserve the floating-point control and status registers.
    "mrs x0, fpcr",
    "str x0, [sp, #768]",
    "mrs x0, fpsr",
    "str x0, [sp, #776]",
    "bl {irq_handler}",
    // Restore the floating-point control and status registers.
    "ldr x0, [sp, #768]",
    "msr fpcr, x0",
    "ldr x0, [sp, #776]",
    "msr fpsr, x0",
    // Restore the interrupted SIMD and floating-point register context.
    "ldp q30, q31, [sp, #736]",
    "ldp q28, q29, [sp, #704]",
    "ldp q26, q27, [sp, #672]",
    "ldp q24, q25, [sp, #640]",
    "ldp q22, q23, [sp, #608]",
    "ldp q20, q21, [sp, #576]",
    "ldp q18, q19, [sp, #544]",
    "ldp q16, q17, [sp, #512]",
    "ldp q14, q15, [sp, #480]",
    "ldp q12, q13, [sp, #448]",
    "ldp q10, q11, [sp, #416]",
    "ldp q8, q9, [sp, #384]",
    "ldp q6, q7, [sp, #352]",
    "ldp q4, q5, [sp, #320]",
    "ldp q2, q3, [sp, #288]",
    "ldp q0, q1, [sp, #256]",
    // Restore the interrupted general-purpose register context.
    "ldr x30, [sp, #240]",
    "ldp x28, x29, [sp, #224]",
    "ldp x26, x27, [sp, #208]",
    "ldp x24, x25, [sp, #192]",
    "ldp x22, x23, [sp, #176]",
    "ldp x20, x21, [sp, #160]",
    "ldp x18, x19, [sp, #144]",
    "ldp x16, x17, [sp, #128]",
    "ldp x14, x15, [sp, #112]",
    "ldp x12, x13, [sp, #96]",
    "ldp x10, x11, [sp, #80]",
    "ldp x8, x9, [sp, #64]",
    "ldp x6, x7, [sp, #48]",
    "ldp x4, x5, [sp, #32]",
    "ldp x2, x3, [sp, #16]",
    "ldp x0, x1, [sp, #0]",
    "add sp, sp, #784",
    "eret",

    "unexpected_exception:",
    "b {unexpected_exception_handler}",
    exception_vectors = sym EXCEPTION_VECTORS,
    irq_handler = sym irq_handler,
    unexpected_exception_handler = sym unexpected_exception_handler,
}

unsafe extern "C" {
    safe static EXCEPTION_VECTORS: u8;
}

pub(super) struct ArchScopeState {
    old_irq_handler: Option<[usize; 2]>,
    interrupts_enabled: bool,
}

impl<'scope> Scope<'scope, '_> {
    /// Initializes the architecture-specific state for a new scope.
    ///
    /// Installs the EL1 exception vector table on first use and records the
    /// current IRQ delivery state so that [`Self::arch_reset`] can restore it.
    pub(super) fn arch_init() -> ArchScopeState {
        let interrupts_enabled = are_interrupts_enabled();
        arch_init_once();
        ArchScopeState {
            old_irq_handler: None,
            interrupts_enabled,
        }
    }

    /// Restores the IRQ handler and IRQ delivery state saved by this scope.
    pub(super) fn arch_reset(&mut self) {
        if let Some(handler) = self.arch.old_irq_handler.take() {
            let _disable = disable_guarded();
            // SAFETY: IRQ_HANDLER is not concurrently accessed while IRQs are disabled.
            unsafe { IRQ_HANDLER = handler };
        }
        if self.arch.interrupts_enabled {
            enable_interrupts();
        } else {
            disable_interrupts();
        }
    }

    /// Saves the currently installed IRQ handler once for later restoration.
    fn save_irq_handler(&mut self) {
        if self.arch.old_irq_handler.is_some() {
            return;
        }
        let _disable = disable_guarded();
        // SAFETY: IRQ_HANDLER is not concurrently modified while IRQs are disabled.
        self.arch.old_irq_handler = Some(unsafe { IRQ_HANDLER });
    }

    /// Sets the IRQ handler.
    ///
    /// This is reverted when the scope ends.
    pub fn set_irq_handler(&mut self, handler: &'scope (dyn Send + Fn(&mut IrqContext))) {
        self.save_irq_handler();
        let _disable = disable_guarded();
        // SAFETY: IRQ_HANDLER is not concurrently modified while IRQs are disabled.
        unsafe {
            IRQ_HANDLER =
                core::mem::transmute::<&(dyn Send + Fn(&mut IrqContext)), [usize; 2]>(handler)
        };
    }

    /// Enables a GIC interrupt.
    pub fn enable_gic_irq(&self, intid: u32) {
        enable_gic_for_current_vp();
        if intid < 32 {
            write_reg32(GICR_IGROUPR0, read_reg32(GICR_IGROUPR0) | (1 << intid));

            let priority_address = GICR_IPRIORITYR0 + (intid as usize & !3);
            let priority_shift = (intid & 3) * 8;
            let priority = read_reg32(priority_address) & !(0xff << priority_shift);
            write_reg32(priority_address, priority | (0x80 << priority_shift));
            write_reg32(GICR_ISENABLER0, 1 << intid);
        } else {
            assert!(intid < GIC_SPECIAL_INTID, "SPI INTID must be below 1020");
            let word = intid as usize / 32;
            let mask = 1 << (intid & 31);
            write_reg32(
                GICD_IGROUPR0 + word * 4,
                read_reg32(GICD_IGROUPR0 + word * 4) | mask,
            );

            let priority_address = GICD_IPRIORITYR0 + (intid as usize & !3);
            let priority_shift = (intid & 3) * 8;
            let priority = read_reg32(priority_address) & !(0xff << priority_shift);
            write_reg32(priority_address, priority | (0x80 << priority_shift));
            write_reg32(GICD_ISENABLER0 + word * 4, mask);
        }
        memory_barrier();
    }

    /// Disables a GIC interrupt.
    pub fn disable_gic_irq(&self, intid: u32) {
        if intid < 32 {
            write_reg32(GICR_ICENABLER0, 1 << intid);
        } else {
            assert!(intid < GIC_SPECIAL_INTID, "SPI INTID must be below 1020");
            let word = intid as usize / 32;
            write_reg32(GICD_ICENABLER0 + word * 4, 1 << (intid & 31));
        }
        memory_barrier();
    }

    /// Enables IRQ delivery.
    ///
    /// This is reverted when the scope ends.
    pub fn enable_interrupts(&self) {
        enable_interrupts();
    }

    /// Disables IRQ delivery and returns true if it was previously enabled.
    ///
    /// This is reverted when the scope ends.
    pub fn disable_interrupts(&self) -> bool {
        disable_interrupts()
    }
}

/// Reads the virtual counter frequency.
pub fn read_cntfrq() -> u64 {
    let value;
    // SAFETY: reading CNTFRQ_EL0 is side-effect free.
    unsafe {
        core::arch::asm!("mrs {value}, CNTFRQ_EL0", value = out(reg) value);
    }
    value
}

/// Reads the virtual counter.
pub fn read_cntvct() -> u64 {
    let value;
    // SAFETY: reading CNTVCT_EL0 is side-effect free.
    unsafe {
        core::arch::asm!("isb; mrs {value}, CNTVCT_EL0", value = out(reg) value);
    }
    value
}

/// Arms the virtual timer to fire at `count`.
pub fn set_virtual_timer_compare(count: u64) {
    // SAFETY: programming the virtual timer is expected in TMK tests.
    unsafe {
        core::arch::asm!(
            "msr CNTV_CVAL_EL0, {count}",
            "msr CNTV_CTL_EL0, {control}",
            "isb",
            count = in(reg) count,
            control = in(reg) 1u64,
        );
    }
}

/// Disables the virtual timer.
pub fn disable_virtual_timer() {
    // SAFETY: programming the virtual timer is expected in TMK tests.
    unsafe {
        core::arch::asm!(
            "msr CNTV_CTL_EL0, {control}",
            "isb",
            control = in(reg) 0u64,
        );
    }
}

/// Polls the emulated GIC while waiting for an interrupt.
///
/// This gives VMMs that emulate the GIC a regular exit on which to observe
/// changes to interrupt sources that are managed outside the guest.
pub fn poll_interrupts() {
    let _ = read_reg32(GICD_CTLR);
}

/// Sends a Group 1 SGI to the current VP.
pub fn send_sgi_to_self(intid: u32) {
    assert!(intid < 16, "SGI INTID must be in the range 0..16");

    let mpidr = read_mpidr();
    let aff0 = mpidr & 0xff;
    let aff1 = (mpidr >> 8) & 0xff;
    let aff2 = (mpidr >> 16) & 0xff;
    let aff3 = (mpidr >> 32) & 0xff;

    assert!(aff0 < 16, "simple SGI target list only supports Aff0 < 16");

    let value =
        (aff3 << 48) | (aff2 << 32) | (u64::from(intid) << 24) | (aff1 << 16) | (1u64 << aff0);

    // SAFETY: programming ICC_SGI1R_EL1 is expected for SGI tests.
    unsafe {
        core::arch::asm!(
            "msr ICC_SGI1R_EL1, {value}",
            "isb",
            value = in(reg) value,
        );
    }
}

/// Sets the pending bit for a GIC SPI through GICD_ISPENDR.
pub fn pend_spi(intid: u32) {
    assert!(
        (32..GIC_SPECIAL_INTID).contains(&intid),
        "SPI INTID must be in 32..1020"
    );
    let word = intid as usize / 32;
    write_reg32(GICD_ISPENDR0 + word * 4, 1 << (intid & 31));
    memory_barrier();
}

#[cfg_attr(not(minimal_rt), expect(dead_code))]
/// Handles a Group 1 IRQ delivered through the EL1 exception vector.
///
/// Acknowledges the highest-priority pending interrupt, invokes the registered
/// scoped handler, and signals end-of-interrupt to the GIC.
extern "C" fn irq_handler() {
    let intid = read_icc_iar1();
    if intid >= GIC_SPECIAL_INTID {
        return;
    }

    let handler = {
        // SAFETY: IRQ_HANDLER is changed only while IRQs are disabled.
        let handler = unsafe { IRQ_HANDLER };
        // SAFETY: this is the underlying type stored by set_irq_handler.
        unsafe {
            core::mem::transmute::<[usize; 2], Option<&(dyn Send + Fn(&mut IrqContext))>>(handler)
        }
    };

    let Some(handler) = handler else {
        panic!("unhandled IRQ {intid}");
    };

    handler(&mut IrqContext { intid });
    write_icc_eoir1(intid);
}

#[cfg_attr(not(minimal_rt), expect(dead_code))]
/// Reports an exception for which TMK does not install a dedicated handler.
///
/// Reads the EL1 exception registers and panics without returning to the vector.
extern "C" fn unexpected_exception_handler() -> ! {
    let esr: u64;
    let elr: u64;
    let far: u64;
    // SAFETY: reading exception syndrome registers is side-effect free.
    unsafe {
        core::arch::asm!(
            "mrs {esr}, ESR_EL1",
            "mrs {elr}, ELR_EL1",
            "mrs {far}, FAR_EL1",
            esr = out(reg) esr,
            elr = out(reg) elr,
            far = out(reg) far,
        );
    }
    panic!("unexpected exception: esr={esr:#x}, elr={elr:#x}, far={far:#x}");
}

/// Performs the one-time installation of the EL1 exception vector table.
///
/// IRQ delivery is masked before `VBAR_EL1` is updated.
fn arch_init_once() {
    if ARCH_INITIALIZED.swap(true, Relaxed) {
        return;
    }

    disable_interrupts();
    // SAFETY: installing the EL1 exception vector is expected during TMK init.
    unsafe {
        core::arch::asm!(
            "msr VBAR_EL1, {vectors}",
            "isb",
            vectors = in(reg) &EXCEPTION_VECTORS as *const u8 as u64,
        );
    }
}

/// Enables the GIC distributor, redistributor, and CPU interface for this VP.
fn enable_gic_for_current_vp() {
    write_reg32(GICD_CTLR, GICD_CTLR_ENABLE_GRP1_AND_ARE);

    let waker = read_reg32(GICR_WAKER) & !(1 << 1);
    write_reg32(GICR_WAKER, waker);
    while read_reg32(GICR_WAKER) & (1 << 2) != 0 {
        core::hint::spin_loop();
    }

    // SAFETY: programming the GIC CPU system-register interface is expected in TMK tests.
    unsafe {
        core::arch::asm!(
            "msr ICC_SRE_EL1, {sre}",
            "isb",
            "msr ICC_PMR_EL1, {pmr}",
            "msr ICC_IGRPEN1_EL1, {enable}",
            "isb",
            sre = in(reg) 1u64,
            pmr = in(reg) 0xffu64,
            enable = in(reg) 1u64,
        );
    }
}

/// Acknowledges and returns the highest-priority pending Group 1 interrupt ID.
fn read_icc_iar1() -> u32 {
    let value: u64;
    // SAFETY: reading ICC_IAR1_EL1 acknowledges the pending interrupt.
    unsafe {
        core::arch::asm!("mrs {value}, ICC_IAR1_EL1", value = out(reg) value);
    }
    value as u32
}

/// Signals completion of a Group 1 interrupt to the GIC CPU interface.
fn write_icc_eoir1(intid: u32) {
    // SAFETY: writing ICC_EOIR1_EL1 completes handling of `intid`.
    unsafe {
        core::arch::asm!("msr ICC_EOIR1_EL1, {value}", value = in(reg) intid as u64);
    }
}

/// Reads the affinity information for the current processing element.
fn read_mpidr() -> u64 {
    let value: u64;
    // SAFETY: reading MPIDR_EL1 is side-effect free.
    unsafe {
        core::arch::asm!("mrs {value}, MPIDR_EL1", value = out(reg) value);
    }
    value
}

/// Performs a volatile 32-bit read from a GIC MMIO register.
fn read_reg32(address: usize) -> u32 {
    // SAFETY: callers pass known GIC MMIO register addresses.
    unsafe { (address as *const u32).read_volatile() }
}

/// Performs a volatile 32-bit write to a GIC MMIO register.
fn write_reg32(address: usize, value: u32) {
    // SAFETY: callers pass known GIC MMIO register addresses.
    unsafe { (address as *mut u32).write_volatile(value) }
}

/// Ensures prior memory accesses and system-register changes are observable.
fn memory_barrier() {
    // SAFETY: a barrier has no memory-safety requirements.
    unsafe {
        core::arch::asm!("dsb sy", "isb");
    }
}

#[must_use]
struct DisableGuard(bool);

/// Masks IRQ delivery until the returned guard is dropped.
///
/// The guard restores IRQ delivery if it was enabled when the guard was created.
fn disable_guarded() -> DisableGuard {
    DisableGuard(disable_interrupts())
}

impl Drop for DisableGuard {
    /// Restores IRQ delivery if it was enabled when this guard was created.
    fn drop(&mut self) {
        if self.0 {
            enable_interrupts();
        }
    }
}

/// Masks IRQ delivery and reports whether it was previously enabled.
fn disable_interrupts() -> bool {
    let enabled = are_interrupts_enabled();
    if enabled {
        // SAFETY: disabling IRQs is always memory safe.
        unsafe {
            core::arch::asm!("msr daifset, #2");
        }
    }
    enabled
}

/// Unmasks IRQ delivery at the current exception level.
fn enable_interrupts() {
    // SAFETY: callers ensure an IRQ handler is installed first.
    unsafe {
        core::arch::asm!("msr daifclr, #2", "isb");
    }
}

// I = 1 → IRQs are masked/disabled
// I = 0 → IRQs are unmasked/enabled
/// Returns whether IRQ exceptions are currently unmasked.
fn are_interrupts_enabled() -> bool {
    let daif: u64;
    // SAFETY: reading DAIF is side-effect free.
    unsafe {
        core::arch::asm!("mrs {daif}, DAIF", daif = out(reg) daif);
    }
    daif & DAIF_IRQ_MASK == 0
}
