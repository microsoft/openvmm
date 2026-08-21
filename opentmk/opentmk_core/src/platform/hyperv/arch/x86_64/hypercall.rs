// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: This module contains unsafe code to perform low-level operations such as invoking hypercalls
#![expect(unsafe_code)]

use core::arch::asm;

use hvdef::Vtl;
use hvdef::hypercall::InitialVpContextX64;
use zerocopy::IntoBytes;

use crate::platform::hyperv::arch::hypercall::HvCall;

// avoiding inline for debuggability in release builds.
#[inline(never)]
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch cpu-intrinsic
/// Invokes a hypercall specifically for switching to a VTL context.
///
///  # Safety
///  The caller must ensure that the hypercall is invoked in a context where it is safe to do so.
unsafe fn invoke_hypercall_vtl(control: hvdef::hypercall::Control) {
    // SAFETY: the caller guarantees the safety of this operation.
    unsafe {
        core::arch::asm! {
            "call {hypercall_page}",
            hypercall_page = sym minimal_rt::arch::hypercall::HYPERCALL_PAGE,
            inout("rcx") u64::from(control) => _,
            in("rdx") 0,
            in("rax") 0,
        }
    }
}

impl HvCall {
    /// Starts a virtual processor (VP) with the specified VTL and context on x86_64.
    pub fn start_virtual_processor(
        &mut self,
        vp_index: u32,
        target_vtl: Vtl,
        vp_context: Option<InitialVpContextX64>,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::StartVirtualProcessorX64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            target_vtl: target_vtl.into(),
            vp_context: vp_context.unwrap_or(zerocopy::FromZeros::new_zeroed()),
            rsvd0: 0u8,
            rsvd1: 0u16,
        };

        header
            .write_to_prefix(self.input_page().buffer.as_mut())
            .expect("size of start_virtual_processor header is not correct");

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallStartVirtualProcessor, None);
        output.result()
    }

    /// Enables a VTL for a specific virtual processor (VP) on x86_64.
    pub fn enable_vp_vtl(
        &mut self,
        vp_index: u32,
        target_vtl: Vtl,
        vp_context: Option<InitialVpContextX64>,
    ) -> Result<(), hvdef::HvError> {
        let header = hvdef::hypercall::EnableVpVtlX64 {
            partition_id: hvdef::HV_PARTITION_ID_SELF,
            vp_index,
            target_vtl: target_vtl.into(),
            reserved: [0; 3],
            vp_vtl_context: vp_context.unwrap_or(zerocopy::FromZeros::new_zeroed()),
        };

        header
            .write_to_prefix(self.input_page().buffer.as_mut_slice())
            .expect("size of enable_vp_vtl header is not correct");

        let output = self.dispatch_hvcall(hvdef::HypercallCode::HvCallEnableVpVtl, None);
        output.result()
    }

    /// Retrieves the current VTL context by reading the necessary registers.
    pub fn get_current_vtl_vp_context(&mut self) -> Result<InitialVpContextX64, hvdef::HvError> {
        use minimal_rt::arch::msr::read_msr;
        use zerocopy::FromZeros;
        let mut context: InitialVpContextX64 = FromZeros::new_zeroed();

        let rsp: u64;
        // SAFETY: we are reading the stack pointer register.
        unsafe { asm!("mov {0:r}, rsp", out(reg) rsp, options(nomem, nostack)) };

        let cr0;
        // SAFETY: we are reading the control register.
        unsafe { asm!("mov {0:r}, cr0", out(reg) cr0, options(nomem, nostack)) };
        let cr3;
        // SAFETY: we are reading the control register.
        unsafe { asm!("mov {0:r}, cr3", out(reg) cr3, options(nomem, nostack)) };
        let cr4;
        // SAFETY: we are reading the control register.
        unsafe { asm!("mov {0:r}, cr4", out(reg) cr4, options(nomem, nostack)) };

        let rflags: u64;
        // SAFETY: we are reading the rflags register.
        unsafe {
            asm!(
                "pushfq",
                "pop {0}",
                out(reg) rflags,
            );
        }

        context.cr0 = cr0;
        context.cr3 = cr3;
        context.cr4 = cr4;

        context.rsp = rsp;
        context.rip = 0;

        context.rflags = rflags;

        // load segment registers

        let cs: u16;
        let ss: u16;
        let ds: u16;
        let es: u16;
        let fs: u16;
        let gs: u16;

        // SAFETY: we are reading the segment registers.
        unsafe {
            asm!("
                mov {0:x}, cs
                mov {1:x}, ss
                mov {2:x}, ds
                mov {3:x}, es
                mov {4:x}, fs
                mov {5:x}, gs
            ", out(reg) cs, out(reg) ss, out(reg) ds, out(reg) es, out(reg) fs, out(reg) gs, options(nomem, nostack))
        }

        context.cs.selector = cs;
        context.cs.attributes = 0xA09B;
        context.cs.limit = 0xFFFFFFFF;

        context.ss.selector = ss;
        context.ss.attributes = 0xC093;
        context.ss.limit = 0xFFFFFFFF;

        context.ds.selector = ds;
        context.ds.attributes = 0xC093;
        context.ds.limit = 0xFFFFFFFF;

        context.es.selector = es;
        context.es.attributes = 0xC093;
        context.es.limit = 0xFFFFFFFF;

        context.fs.selector = fs;
        context.fs.attributes = 0xC093;
        context.fs.limit = 0xFFFFFFFF;

        context.gs.selector = gs;
        context.gs.attributes = 0xC093;
        context.gs.limit = 0xFFFFFFFF;

        context.tr.selector = 0;
        context.tr.attributes = 0x8B;
        context.tr.limit = 0xFFFF;

        let idt = x86_64::instructions::tables::sidt();
        context.idtr.base = idt.base.as_u64();
        context.idtr.limit = idt.limit;

        let gdtr = x86_64::instructions::tables::sgdt();
        context.gdtr.base = gdtr.base.as_u64();
        context.gdtr.limit = gdtr.limit;

        // SAFETY: we are reading a valid MSR.
        let efer = unsafe { read_msr(0xC0000080) };
        context.efer = efer;

        Ok(context)
    }

    // avoiding inline for debuggability in release builds.
    #[inline(never)]
    /// Invokes the VtlCall hypercall.
    pub(crate) fn vtl_call() {
        let control: hvdef::hypercall::Control = hvdef::hypercall::Control::new()
            .with_code(hvdef::HypercallCode::HvCallVtlCall.0)
            .with_rep_count(0);
        // SAFETY: This is safe because we are calling a hypercall with a valid control structure.
        unsafe { invoke_hypercall_vtl(control) };
    }

    // avoiding inline for debuggability in release builds.
    #[inline(never)]
    /// Invokes the VtlReturn hypercall.
    pub(crate) fn vtl_return() {
        let control: hvdef::hypercall::Control = hvdef::hypercall::Control::new()
            .with_code(hvdef::HypercallCode::HvCallVtlReturn.0)
            .with_rep_count(0);
        // SAFETY: This is safe because we are calling a hypercall with a valid control structure.
        unsafe { invoke_hypercall_vtl(control) };
    }
}
