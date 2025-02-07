// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall infrastructure.
//!
//! The hypercall ABI for x64 is well documented in the TLFS. For
//! ARM64, the TLFS doesn't seem to have the details so here they are.
//!
//! The hypercall instruction on ARM64 has an immediate in it:
//!
//! ```asm
//! hvc #imm16
//! ```
//!
//! whose value indicates the ABI used to the hypervisor:
//!
//! - `0`: ARM's SMCCC ABI specification.
//! - `1`: Hyper-V's Aa64 ABI specification.
//! - `2`: Hyper-V's Aa64 ABI specification, VTL entry.
//! - `3`: Hyper-V's Aa64 ABI specification, VTL exit.
//! - `4`: Hyper-V's Aa64 ABI specification, VP launch.
//!
//! If not using the SMCCC ABI, the hypercall call code is passed in `X0`.
//!
//! In the SMCCC ABI, the hypercall call code is passed in `X1`, and
//! `X0` holds the SMCCC function code that is formed from the various
//! other codes, the vendor code included.
//!
//! The parameters and the output might be passed via the input page
//! and the output page. In the SMCCC case, the hypervisor expects their
//! GPA's in `X2` and `X3`, otherwise in `X1` and `X2`.
//!
//! For the fast hypercalls, the parameters are passed in 16 registers.
//! Specifically, in `X2`..`X16` when using the SMCCC conventions, and starting
//! from `X1` otherwise. The hypervisor accesses the trap frame to work
//! with the registers.
//!
//! The hypercall status is returned in `X0`.
//!
//! The code in the architecture-independent part uses the regular hypercalls
//! to make it possible to merge with the x64 case easily.
//!

use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvResult;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

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
    input: u64,
    output: u64,
) -> hvdef::hypercall::HypercallOutput {
    let mut status: u64;

    // Note `#1` for the Hyper-V Aa64 ABI.
    // SAFETY: following the ABI.
    unsafe {
        core::arch::asm!("hvc #1", inout("x0") u64::from(control) => status, in("x1") input, in("x2") output);
    }

    status.into()
}

/// 6 input words, 2 output words.
unsafe fn invoke_hypercall_fast_6_2(
    control: hvdef::hypercall::Control,
    input: [u64; 6],
) -> (hvdef::hypercall::HypercallOutput, [u64; 2]) {
    assert!(control.fast());

    let mut status: u64;
    let mut output0;
    let mut output1;

    // Note `#1` for the Hyper-V Aa64 ABI.
    // SAFETY: following the ABI.
    unsafe {
        core::arch::asm!(
            "hvc #1",
            inout("x0") u64::from(control) => status,
            in("x1") input[0],
            in("x2") input[1],
            in("x3") input[2],
            in("x4") input[3],
            in("x5") input[4],
            in("x6") input[5],
            lateout("x15") output0,
            lateout("x16") output1,
        );
    }

    (status.into(), [output0, output1])
}

/// Sets a register for the current VTL using a fast hypercall.
pub fn set_register_fast(name: HvRegisterName, value: HvRegisterValue) -> HvResult<()> {
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    struct Input {
        header: hvdef::hypercall::GetSetVpRegisters,
        assoc: hvdef::hypercall::HvRegisterAssoc,
    }

    let control = hvdef::hypercall::Control::new()
        .with_code(hvdef::HypercallCode::HvCallSetVpRegisters.0)
        .with_rep_count(1)
        .with_fast(true);

    // SAFETY: invoking the fast hypercall with the appropriate input size.
    // There is no output.
    let (status, _) = unsafe {
        invoke_hypercall_fast_6_2(
            control,
            zerocopy::transmute!(Input {
                header: hvdef::hypercall::GetSetVpRegisters {
                    partition_id: hvdef::HV_PARTITION_ID_SELF,
                    vp_index: hvdef::HV_VP_INDEX_SELF,
                    target_vtl: hvdef::hypercall::HvInputVtl::CURRENT_VTL,
                    rsvd: Default::default(),
                },
                assoc: hvdef::hypercall::HvRegisterAssoc {
                    name,
                    pad: Default::default(),
                    value,
                }
            }),
        )
    };

    status.result()
}

/// Gets a register for the current VTL using a fast hypercall.
pub fn get_register_fast(name: HvRegisterName) -> HvResult<HvRegisterValue> {
    #[repr(C)]
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    struct Input {
        header: hvdef::hypercall::GetSetVpRegisters,
        name: HvRegisterName,
        pad: u32,
        pad2: u64,
        pad3: u128,
    }

    let control = hvdef::hypercall::Control::new()
        .with_code(hvdef::HypercallCode::HvCallGetVpRegisters.0)
        .with_rep_count(1)
        .with_fast(true);

    // SAFETY: invoking the fast hypercall with the appropriate input and output sizes.
    let (status, output) = unsafe {
        invoke_hypercall_fast_6_2(
            control,
            zerocopy::transmute!(Input {
                header: hvdef::hypercall::GetSetVpRegisters {
                    partition_id: hvdef::HV_PARTITION_ID_SELF,
                    vp_index: hvdef::HV_VP_INDEX_SELF,
                    target_vtl: hvdef::hypercall::HvInputVtl::CURRENT_VTL,
                    rsvd: Default::default(),
                },
                name,
                pad: 0,
                pad2: 0,
                pad3: 0,
            }),
        )
    };

    status.result()?;
    Ok(zerocopy::transmute!(output))
}
