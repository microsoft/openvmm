// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The exception handlers that the shim installs before it calls the Linux
//! EFI stub.
//!
//! The EFI stub runs `CPUID`, and under SEV-SNP that raises a `#VC`
//! exception. Firmware must handle it, as OVMF does. The `#VC` handler answers
//! `CPUID` from the measured SNP CPUID page, as the Linux early `#VC` handler
//! does. Any other `#VC` reason, and every other exception, stops the guest.
//!
//! The handlers stay installed after ExitBootServices, until Linux loads its
//! own IDT. The shim image is runtime-services memory, so Linux does not reuse
//! it.

use x86defs::snp::HV_PSP_CPUID_LEAF_COUNT_MAX;
use x86defs::snp::HvPspCpuidPage;

/// Returns `true` if the CPUID leaf has subleaves selected by ECX.
///
/// This is Linux's `cpuid_function_is_indexed()`
/// (`arch/x86/include/asm/cpuid/api.h`), plus the other leaves that the Intel
/// SDM (0x20, 0x23) and the AMD APM (0x8000_0020, 0x8000_0026) define with
/// subleaves.
fn is_indexed_leaf(leaf: u32) -> bool {
    matches!(
        leaf,
        0x4 | 0x7
            | 0xb
            | 0xd
            | 0xf
            | 0x10
            | 0x12
            | 0x14
            | 0x17
            | 0x18
            | 0x1d
            | 0x1e
            | 0x1f
            | 0x20
            | 0x23
            | 0x24
            | 0x8000_001d
            | 0x8000_0020
            | 0x8000_0026
    )
}

/// Returns EAX, EBX, ECX, and EDX for `CPUID(leaf, subleaf)` from the SNP
/// CPUID page. A leaf that the page does not list returns zeros.
pub(crate) fn cpuid(page: &HvPspCpuidPage, leaf: u32, subleaf: u32) -> [u32; 4] {
    let count = (page.count as usize).min(HV_PSP_CPUID_LEAF_COUNT_MAX);
    page.cpuid_leaf_info[..count]
        .iter()
        .find(|entry| entry.eax_in == leaf && (!is_indexed_leaf(leaf) || entry.ecx_in == subleaf))
        .map_or([0; 4], |entry| {
            [entry.eax_out, entry.ebx_out, entry.ecx_out, entry.edx_out]
        })
}

#[cfg(minimal_rt)]
pub(crate) use guest::install;

#[cfg(minimal_rt)]
mod guest {
    use crate::Failure;
    use crate::terminate;
    use x86defs::Exception;
    use x86defs::IdtAttributes;
    use x86defs::IdtEntry64;
    use x86defs::X64_IDT_GATE_TYPE_INTERRUPT;
    use x86defs::snp::HvPspCpuidPage;
    use x86defs::snp::SevExitCode;

    /// The number of vectors that x86 reserves for exceptions (Intel SDM
    /// Vol. 3A 6.2; AMD APM Vol. 2 8.2).
    const EXCEPTION_COUNT: usize = 32;
    /// The length of the `CPUID` instruction (`0F A2`).
    const CPUID_LENGTH: u64 = 2;

    #[repr(C, align(16))]
    struct Idt([IdtEntry64; EXCEPTION_COUNT]);

    static mut IDT: Idt = Idt([IdtEntry64 {
        offset_low: 0,
        selector: 0,
        attributes: IdtAttributes::new(),
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    }; EXCEPTION_COUNT]);

    /// The measured CPUID page. `install` sets it before any `#VC` can occur.
    static mut CPUID_PAGE: u64 = 0;

    /// The registers that `vc_entry` saves, followed by the CPU's exception
    /// frame.
    #[repr(C)]
    struct VcFrame {
        r15: u64,
        r14: u64,
        r13: u64,
        r12: u64,
        r11: u64,
        r10: u64,
        r9: u64,
        r8: u64,
        rbp: u64,
        rdi: u64,
        rsi: u64,
        rdx: u64,
        rcx: u64,
        rbx: u64,
        rax: u64,
        error_code: u64,
        rip: u64,
        cs: u64,
        rflags: u64,
        rsp: u64,
        ss: u64,
    }

    core::arch::global_asm! {
        ".globl vc_entry",
        "vc_entry:",
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // The CPU aligns RSP to 16 bytes before it pushes the six-quadword
        // frame. The fifteen pushes above leave RSP 8 bytes below alignment.
        "mov rdi, rsp",
        "sub rsp, 8",
        "cld",
        "call {vc_handler}",
        "add rsp, 8",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        // Drop the error code.
        "add rsp, 8",
        "iretq",
        ".globl exception_entry",
        "exception_entry:",
        "and rsp, -16",
        "cld",
        "call {exception_handler}",
        "ud2",
        vc_handler = sym vc_handler,
        exception_handler = sym exception_handler,
    }

    unsafe extern "C" {
        fn vc_entry();
        fn exception_entry();
    }

    extern "C" fn vc_handler(frame: &mut VcFrame) {
        if frame.error_code != SevExitCode::CPUID.0 {
            terminate(Failure::UnhandledVc);
        }
        // SAFETY: `install` set this to the measured, accepted CPUID page,
        // which nothing writes.
        let page =
            unsafe { &*(core::ptr::read_volatile(&raw const CPUID_PAGE) as *const HvPspCpuidPage) };
        let [eax, ebx, ecx, edx] = super::cpuid(page, frame.rax as u32, frame.rcx as u32);
        frame.rax = eax.into();
        frame.rbx = ebx.into();
        frame.rcx = ecx.into();
        frame.rdx = edx.into();
        frame.rip += CPUID_LENGTH;
    }

    extern "C" fn exception_handler() -> ! {
        terminate(Failure::Exception)
    }

    fn gate(handler: u64, selector: u16) -> IdtEntry64 {
        IdtEntry64 {
            offset_low: handler as u16,
            selector,
            attributes: IdtAttributes::new()
                .with_gate_type(X64_IDT_GATE_TYPE_INTERRUPT)
                .with_present(true),
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }

    /// Installs the exception handlers. `cpuid_page` is the GPA of the
    /// measured SNP CPUID page.
    pub fn install(cpuid_page: u64) {
        let selector: u16;
        // SAFETY: Reading CS has no side effects.
        unsafe {
            core::arch::asm!("mov {0:x}, cs", out(reg) selector, options(nomem, nostack, preserves_flags));
        }
        let idt = &raw mut IDT;
        // SAFETY: The shim runs on one processor with interrupts disabled,
        // and nothing else uses these statics until the IDT is loaded.
        unsafe {
            core::ptr::write_volatile(&raw mut CPUID_PAGE, cpuid_page);
            for (vector, entry) in (*idt).0.iter_mut().enumerate() {
                let handler = if vector == Exception::SEV_VMM_COMMUNICATION.0.into() {
                    vc_entry as *const () as usize
                } else {
                    exception_entry as *const () as usize
                };
                *entry = gate(handler as u64, selector);
            }
        }

        #[repr(C, packed)]
        struct Descriptor {
            limit: u16,
            base: u64,
        }
        let descriptor = Descriptor {
            limit: (size_of::<Idt>() - 1) as u16,
            base: idt as u64,
        };
        // SAFETY: The IDT is a static with valid gates for every exception
        // vector, and the handlers stay in memory until Linux loads its own
        // IDT.
        unsafe {
            core::arch::asm!("lidt [{0}]", in(reg) &raw const descriptor, options(readonly, nostack, preserves_flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use x86defs::snp::HvPspCpuidLeaf;
    use zerocopy::FromZeros;

    fn leaf(eax_in: u32, ecx_in: u32, out: u32) -> HvPspCpuidLeaf {
        HvPspCpuidLeaf {
            eax_in,
            ecx_in,
            xfem_in: 0,
            xss_in: 0,
            eax_out: out,
            ebx_out: out + 1,
            ecx_out: out + 2,
            edx_out: out + 3,
            reserved_z: 0,
        }
    }

    #[test]
    fn looks_up_leaves_and_subleaves() {
        let mut page = HvPspCpuidPage::new_zeroed();
        page.cpuid_leaf_info[0] = leaf(0, 0, 0x10);
        page.cpuid_leaf_info[1] = leaf(7, 0, 0x20);
        page.cpuid_leaf_info[2] = leaf(7, 1, 0x30);
        page.cpuid_leaf_info[3] = leaf(0x8000_001f, 0, 0x40);
        page.count = 4;

        // ECX does not select subleaves of leaf 0.
        assert_eq!(cpuid(&page, 0, 5), [0x10, 0x11, 0x12, 0x13]);
        assert_eq!(cpuid(&page, 7, 1), [0x30, 0x31, 0x32, 0x33]);
        assert_eq!(cpuid(&page, 0x8000_001f, 0), [0x40, 0x41, 0x42, 0x43]);
        // Unlisted leaves and subleaves return zeros.
        assert_eq!(cpuid(&page, 7, 2), [0; 4]);
        assert_eq!(cpuid(&page, 0x21, 0), [0; 4]);
    }

    #[test]
    fn ignores_entries_past_the_count() {
        let mut page = HvPspCpuidPage::new_zeroed();
        page.cpuid_leaf_info[1] = leaf(1, 0, 0x10);
        page.count = 1;
        assert_eq!(cpuid(&page, 1, 0), [0; 4]);
        page.count = u32::MAX;
        assert_eq!(cpuid(&page, 1, 0), [0x10, 0x11, 0x12, 0x13]);
    }
}
