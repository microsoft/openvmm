// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall architecture-dependent infrastructure.
//!
//! The hypercall ABI for x64 is well documented in the TLFS.

unsafe extern "C" {
    /// The hypercall page. The actual hypercall page must be mapped on top of
    /// this page before it is used.
    pub static mut HYPERCALL_PAGE: [u8; 4096];
}

core::arch::global_asm! {
    r#"
.globl HYPERCALL_PAGE
.align 4096
HYPERCALL_PAGE:
    ud2
    .skip 4094, 0xcc
"#,
}

/// Invokes a standard hypercall, or a fast hypercall with at most two input
/// words and zero output words.
///
/// # Safety
/// The caller must ensure the hypercall is safe to issue, and that the
/// input/output pages are not being concurrently used elsewhere. For fast
/// hypercalls, the caller must ensure that there are no output words so that
/// there is no register corruption.
pub unsafe fn invoke_hypercall(
    control: hvdef::hypercall::Control,
    input_gpa_or_fast1: u64,
    output_gpa_or_fast2: u64,
) -> hvdef::hypercall::HypercallOutput {
    let output: u64;
    // SAFETY: the caller guarantees the safety of this operation.
    unsafe {
        core::arch::asm! {
            "call {hypercall_page}",
            hypercall_page = sym HYPERCALL_PAGE,
            inout("rcx") u64::from(control) => _,
            in("rdx") input_gpa_or_fast1,
            in("r8") output_gpa_or_fast2,
            out("rax") output,
        }
    }
    output.into()
}
