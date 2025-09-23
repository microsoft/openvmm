// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common TDCALL handling for issuing tdcalls and functionality using tdcalls.

#![no_std]
#![forbid(unsafe_code)]

use hvdef::HV_PAGE_SIZE;
use hvdef::hypercall::HypercallOutput;
use memory_range::MemoryRange;
use thiserror::Error;
use x86defs::tdx::TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT;
use x86defs::tdx::TdCallLeaf;
use x86defs::tdx::TdCallResult;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdGlaVmAndFlags;
use x86defs::tdx::TdReport;
use x86defs::tdx::TdVmCallR10Result;
use x86defs::tdx::TdVmCallSubFunction;
use x86defs::tdx::TdgMemPageAcceptRcx;
use x86defs::tdx::TdgMemPageAttrGpaMappingReadRcxResult;
use x86defs::tdx::TdgMemPageAttrWriteR8;
use x86defs::tdx::TdgMemPageAttrWriteRcx;
use x86defs::tdx::TdgMemPageGpaAttr;
use x86defs::tdx::TdgMemPageLevel;
use x86defs::tdx::TdxExtendedFieldCode;
use x86defs::tdx::TdxGlaListInfo;

/// Input to a tdcall. This is not defined in the TDX specification, but a
/// contract between callers of this module and this module's handling of
/// tdcalls.
#[derive(Debug)]
pub struct TdcallInput {
    /// The leaf for the tdcall (eax)
    pub leaf: TdCallLeaf,
    /// rcx
    pub rcx: u64,
    /// rdx
    pub rdx: u64,
    /// r8
    pub r8: u64,
    /// r9
    pub r9: u64,
    /// r10
    pub r10: u64,
    /// r11
    pub r11: u64,
    /// r12
    pub r12: u64,
    /// r13
    pub r13: u64,
    /// r14
    pub r14: u64,
    /// r15
    pub r15: u64,
}

/// Output from a tdcall. This is not defined in the TDX specification, but a
/// contract between callers of this module and this module's handling of
/// tdcalls.
#[derive(Debug)]
pub struct TdcallOutput {
    /// The tdcall result stored in rax.
    pub rax: TdCallResult,
    /// rcx
    pub rcx: u64,
    /// rdx
    pub rdx: u64,
    /// r8,
    pub r8: u64,
    /// r10
    pub r10: u64,
    /// r11
    pub r11: u64,
}

/// Trait to perform tdcalls used by this module.
pub trait Tdcall {
    /// Perform a tdcall instruction with the specified inputs.
    fn tdcall(&mut self, input: TdcallInput) -> TdcallOutput;
}

/// Perform a tdcall based Hypercall. This is done by issuing a TDG.VP.VMCALL.
pub fn tdcall_hypercall(
    call: &mut impl Tdcall,
    control: hvdef::hypercall::Control,
    input_gpa: u64,
    output_gpa: u64,
) -> HypercallOutput {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_VMCALL,
        rcx: 0x0d04, // pass RDX, R8, R10, R11
        rdx: input_gpa,
        r8: output_gpa,
        r9: 0,
        r10: u64::from(control), // hypercall control code
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    if output.rax.code() != TdCallResultCode::SUCCESS {
        // This means something has gone horribly wrong with the TDX module, as
        // this call should always succeed with hypercall errors returned in
        // r11.
        panic!(
            "unexpected nonzero rax {:x} on tdcall_hypercall",
            u64::from(output.rax)
        );
    }

    // TD.VMCALL for Hypercall passes return code in r11
    HypercallOutput::from(output.r11)
}

/// Perform a tdcall based MSR read. This is done by issuing a TDG.VP.VMCALL.
pub fn tdcall_rdmsr(
    call: &mut impl Tdcall,
    msr_index: u32,
    msr_value: &mut u64,
) -> Result<(), TdVmCallR10Result> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_VMCALL,
        rcx: 0x1c00, // pass R10-R12
        rdx: 0,
        r8: 0,
        r9: 0,
        r10: 0, // must be 0 for ghci call
        r11: TdVmCallSubFunction::RdMsr as u64,
        r12: msr_index as u64,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    // This assertion failing means something has gone horribly wrong with the
    // TDX module, as this call should always succeed with hypercall errors
    // returned in r10.
    assert_eq!(
        output.rax.code(),
        TdCallResultCode::SUCCESS,
        "unexpected nonzero rax {:x} returned by tdcall vmcall",
        u64::from(output.rax)
    );

    let result = TdVmCallR10Result(output.r10);

    *msr_value = output.r11;

    #[cfg(feature = "tracing")]
    tracing::trace!(msr_index, msr_value, output.r10, "tdcall_rdmsr");

    match result {
        TdVmCallR10Result::SUCCESS => Ok(()),
        val => Err(val),
    }
}

/// Perform a tdcall based MSR write. This is done by issuing a TDG.VP.VMCALL.
pub fn tdcall_wrmsr(
    call: &mut impl Tdcall,
    msr_index: u32,
    msr_value: u64,
) -> Result<(), TdVmCallR10Result> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_VMCALL,
        rcx: 0x3c00, // pass R10-R13
        rdx: 0,
        r8: 0,
        r9: 0,
        r10: 0, // must be 0 for ghci call
        r11: TdVmCallSubFunction::WrMsr as u64,
        r12: msr_index as u64,
        r13: msr_value,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    // This assertion failing means something has gone horribly wrong with the
    // TDX module, as this call should always succeed with hypercall errors
    // returned in r10.
    assert_eq!(
        output.rax.code(),
        TdCallResultCode::SUCCESS,
        "unexpected nonzero rax {:x} returned by tdcall vmcall",
        u64::from(output.rax)
    );

    let result = TdVmCallR10Result(output.r10);

    match result {
        TdVmCallR10Result::SUCCESS => Ok(()),
        val => Err(val),
    }
}

/// Perform a tdcall based io port write.
pub fn tdcall_io_out(
    call: &mut impl Tdcall,
    port: u16,
    value: u32,
    size: u8,
) -> Result<(), TdVmCallR10Result> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_VMCALL,
        rcx: 0xFF00, // pass r10-R15
        rdx: 0,
        r8: 0,
        r9: 0,
        r10: 0, // must be 0 for ghci call
        r11: 30,
        r12: size as u64,
        r13: 1, // WRITE
        r14: port as u64,
        r15: value as u64,
    };

    let output = call.tdcall(input);

    // This assertion failing means something has gone horribly wrong with the
    // TDX module, as this call should always succeed with hypercall errors
    // returned in r10.
    assert_eq!(
        output.rax.code(),
        TdCallResultCode::SUCCESS,
        "unexpected nonzero rax {:x} returned by tdcall vmcall",
        u64::from(output.rax)
    );

    if output.rax.code() != TdCallResultCode::SUCCESS {
        // This means something has gone horribly wrong with the TDX module, as
        // this call should always succeed with hypercall errors returned in
        // r10.
        panic!(
            "unexpected nonzero rax {:x} on tdcall_io_out",
            u64::from(output.rax)
        );
    }

    let result = TdVmCallR10Result(output.r10);

    match result {
        TdVmCallR10Result::SUCCESS => Ok(()),
        val => Err(val),
    }
}

/// Perform a tdcall based io port read.
pub fn tdcall_io_in(call: &mut impl Tdcall, port: u16, size: u8) -> Result<u32, TdVmCallR10Result> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_VMCALL,
        rcx: 0xFF00, // pass r10-R15
        rdx: 0,
        r8: 0,
        r9: 0,
        r10: 0, // must be 0 for ghci call
        r11: TdVmCallSubFunction::IoInstr as u64,
        r12: size as u64,
        r13: 0, // READ
        r14: port as u64,
        r15: 0,
    };

    let output = call.tdcall(input);

    // This assertion failing means something has gone horribly wrong with the
    // TDX module, as this call should always succeed with hypercall errors
    // returned in r10.
    assert_eq!(
        output.rax.code(),
        TdCallResultCode::SUCCESS,
        "unexpected nonzero rax {:x} returned by tdcall vmcall",
        u64::from(output.rax)
    );

    let result = TdVmCallR10Result(output.r10);

    match result {
        TdVmCallR10Result::SUCCESS => Ok(output.r11 as u32),
        val => Err(val),
    }
}

/// Issue a TDG.MEM.PAGE.ACCEPT call.
pub fn tdcall_accept_pages(
    call: &mut impl Tdcall,
    gpa_page_number: u64,
    as_large_page: bool,
) -> Result<(), TdCallResultCode> {
    #[cfg(feature = "tracing")]
    tracing::trace!(gpa_page_number, as_large_page, "tdcall_accept_pages");

    let rcx = TdgMemPageAcceptRcx::new()
        .with_gpa_page_number(gpa_page_number)
        .with_level(if as_large_page {
            TdgMemPageLevel::Size2Mb
        } else {
            TdgMemPageLevel::Size4k
        });

    let input = TdcallInput {
        leaf: TdCallLeaf::MEM_PAGE_ACCEPT,
        rcx: rcx.into(),
        rdx: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(()),
        val => Err(val),
    }
}

/// The result returned from [`tdcall_page_attr_rd`].
#[derive(Debug)]
pub struct TdgPageAttrRdResult {
    /// The mapping information for the page.
    pub mapping: TdgMemPageAttrGpaMappingReadRcxResult,
    /// The attributes for the page.
    pub attributes: TdgMemPageGpaAttr,
}

/// Issue a TDG.MEM.PAGE.ATTR.RD call.
pub fn tdcall_page_attr_rd(
    call: &mut impl Tdcall,
    gpa: u64,
) -> Result<TdgPageAttrRdResult, TdCallResultCode> {
    #[cfg(feature = "tracing")]
    tracing::trace!(gpa, "tdcall_page_attr_rd");

    let input = TdcallInput {
        leaf: TdCallLeaf::MEM_PAGE_ATTR_RD,
        rcx: gpa,
        rdx: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(TdgPageAttrRdResult {
            mapping: TdgMemPageAttrGpaMappingReadRcxResult::from(output.rcx),
            attributes: TdgMemPageGpaAttr::from(output.rdx),
        }),
        val => Err(val),
    }
}

/// Issue a TDG.MEM.PAGE.ATTR.WR call.
pub fn tdcall_page_attr_wr(
    call: &mut impl Tdcall,
    mapping: TdgMemPageAttrWriteRcx,
    attributes: TdgMemPageGpaAttr,
    mask: TdgMemPageAttrWriteR8,
) -> Result<(), TdCallResultCode> {
    #[cfg(feature = "tracing")]
    tracing::trace!(?mapping, ?attributes, ?mask, "tdcall_page_attr_wr");

    let input = TdcallInput {
        leaf: TdCallLeaf::MEM_PAGE_ATTR_WR,
        rcx: mapping.into(),
        rdx: attributes.into(),
        r8: mask.into(),
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    // TODO TDX: RCX and RDX also contain info that could be returned

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(()),
        val => Err(val),
    }
}

/// Issue a TDG.MEM.PAGE.ATTR.WR call, but perform additional validation that
/// the attributes were set correctly on debug builds.
fn set_page_attr(
    call: &mut impl Tdcall,
    mapping: TdgMemPageAttrWriteRcx,
    attributes: TdgMemPageGpaAttr,
    mask: TdgMemPageAttrWriteR8,
) -> Result<(), TdCallResultCode> {
    match tdcall_page_attr_wr(call, mapping, attributes, mask) {
        Ok(()) => {
            #[cfg(debug_assertions)]
            {
                let result =
                    tdcall_page_attr_rd(call, mapping.gpa_page_number() * HV_PAGE_SIZE).unwrap();
                assert_eq!(u64::from(mapping), result.mapping.into());
                assert_eq!(attributes.l1(), result.attributes.l1());
                assert_eq!(
                    attributes.into_bits() & mask.with_reserved(0).into_bits(),
                    result.attributes.into_bits() & mask.with_reserved(0).into_bits()
                );
            }

            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// The error returned by [`accept_pages`].
// TODO: why is this an enum with multiple variants--callers don't seem to care.
// Collapse into a struct, or at least collapse some of the variants?
#[derive(Debug, Error)]
pub enum AcceptPagesError {
    /// Unknown error type.
    #[error("unknown error: {0:?}")]
    Unknown(TdCallResultCode),
    /// Setting page attributes failed after accepting,
    #[error("setting page attributes failed after accepting: {0:?}")]
    Attributes(TdCallResultCode),
    /// Invalid operand
    #[error("invalid operand: {0:?}")]
    Invalid(TdCallResultCode),
    /// Busy Operand
    #[error("operand busy: {0:?}")]
    Busy(TdCallResultCode),
}

/// The page attributes to accept pages with.
pub enum AcceptPagesAttributes {
    /// Leave page attributes as is and do not issue TDG.MEM.PAGE.ATTR.WR calls
    /// after accepting pages.
    None,
    /// Issue corresponding TDG.MEM.PAGE.ATTR.WR calls after accepting pages to
    /// set page attributes to the following values.
    Set {
        /// The attributes to set for pages.
        attributes: TdgMemPageGpaAttr,
        /// The mask to use when setting the page attributes.
        mask: TdgMemPageAttrWriteR8,
    },
}

/// Accept pages from `range` using [`tdcall_accept_pages`].
pub fn accept_pages<T: Tdcall>(
    call: &mut T,
    range: MemoryRange,
    attributes: AcceptPagesAttributes,
) -> Result<(), AcceptPagesError> {
    #[cfg(feature = "tracing")]
    tracing::trace!(%range, "accept_pages");

    let set_attributes = |call: &mut T, mapping| -> Result<(), AcceptPagesError> {
        match attributes {
            AcceptPagesAttributes::None => Ok(()),
            AcceptPagesAttributes::Set { attributes, mask } => {
                set_page_attr(call, mapping, attributes, mask).map_err(AcceptPagesError::Attributes)
            }
        }
    };

    let mut range = range;
    while !range.is_empty() {
        // Attempt to accept in large page chunks if possible.
        if range.start().is_multiple_of(x86defs::X64_LARGE_PAGE_SIZE)
            && range.len() >= x86defs::X64_LARGE_PAGE_SIZE
        {
            match tdcall_accept_pages(call, range.start_4k_gpn(), true) {
                Ok(_) => {
                    set_attributes(
                        call,
                        TdgMemPageAttrWriteRcx::new()
                            .with_gpa_page_number(range.start_4k_gpn())
                            .with_level(TdgMemPageLevel::Size2Mb),
                    )?;

                    range =
                        MemoryRange::new(range.start() + x86defs::X64_LARGE_PAGE_SIZE..range.end());
                    continue;
                }
                Err(e) => match e {
                    TdCallResultCode::OPERAND_BUSY => return Err(AcceptPagesError::Busy(e)),
                    TdCallResultCode::OPERAND_INVALID => return Err(AcceptPagesError::Invalid(e)),
                    TdCallResultCode::PAGE_ALREADY_ACCEPTED => {
                        panic!("page {} already accepted", range.start_4k_gpn());
                    }
                    TdCallResultCode::PAGE_SIZE_MISMATCH => {
                        #[cfg(feature = "tracing")]
                        tracing::trace!("accept pages size mismatch returned");
                    }
                    _ => return Err(AcceptPagesError::Unknown(e)),
                },
            }
        }

        // Accept in 4k size pages
        match tdcall_accept_pages(call, range.start_4k_gpn(), false) {
            Ok(_) => {
                set_attributes(
                    call,
                    TdgMemPageAttrWriteRcx::new()
                        .with_gpa_page_number(range.start_4k_gpn())
                        .with_level(TdgMemPageLevel::Size4k),
                )?;

                range = MemoryRange::new(range.start() + HV_PAGE_SIZE..range.end());
            }
            Err(e) => match e {
                TdCallResultCode::OPERAND_BUSY => return Err(AcceptPagesError::Busy(e)),
                TdCallResultCode::OPERAND_INVALID => return Err(AcceptPagesError::Invalid(e)),
                TdCallResultCode::PAGE_ALREADY_ACCEPTED => {
                    panic!("page {} already accepted", range.start_4k_gpn());
                }
                _ => return Err(AcceptPagesError::Unknown(e)),
            },
        }
    }

    Ok(())
}

/// Set page attributes from `range` using
/// [`tdcall_page_attr_wr`].
///
/// This will attempt to set attributes in 2MB chunks if possible.
pub fn set_page_attributes(
    call: &mut impl Tdcall,
    range: MemoryRange,
    attributes: TdgMemPageGpaAttr,
    mask: TdgMemPageAttrWriteR8,
) -> Result<(), TdCallResultCode> {
    #[cfg(feature = "tracing")]
    tracing::trace!(
        %range,
        ?attributes,
        ?mask,
        "set_page_attributes"
    );

    let mut range = range;
    while !range.is_empty() {
        // Attempt to set in large page chunks if possible.
        if range.start().is_multiple_of(x86defs::X64_LARGE_PAGE_SIZE)
            && range.len() >= x86defs::X64_LARGE_PAGE_SIZE
        {
            let mapping = TdgMemPageAttrWriteRcx::new()
                .with_gpa_page_number(range.start_4k_gpn())
                .with_level(TdgMemPageLevel::Size2Mb);

            match set_page_attr(call, mapping, attributes, mask) {
                Ok(()) => {
                    range =
                        MemoryRange::new(range.start() + x86defs::X64_LARGE_PAGE_SIZE..range.end());
                    continue;
                }
                Err(TdCallResultCode::PAGE_SIZE_MISMATCH) => {
                    #[cfg(feature = "tracing")]
                    tracing::trace!("set pages attr size mismatch returned");
                }
                Err(e) => return Err(e),
            }
        }

        // Set in 4k size pages
        let mapping = TdgMemPageAttrWriteRcx::new()
            .with_gpa_page_number(range.start_4k_gpn())
            .with_level(TdgMemPageLevel::Size4k);

        match set_page_attr(call, mapping, attributes, mask) {
            Ok(()) => range = MemoryRange::new(range.start() + HV_PAGE_SIZE..range.end()),
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Issue a map gpa call to change page visibility for accepted pages via a
/// TDG.VP.VMCALL.
///
/// `gpa` should specify the gpa for the address to change visibility for. The
/// shared gpa boundary will be added or masked off as required.
///
/// `len` should specify the length of the region in bytes to change visibility
/// for.
///
/// `host_visible` should specify whether the region should be host visible or
/// private.
pub fn tdcall_map_gpa(
    call: &mut impl Tdcall,
    range: MemoryRange,
    host_visible: bool,
) -> Result<(), TdVmCallR10Result> {
    let mut gpa = if host_visible {
        range.start() | TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT
    } else {
        range.start() & !TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT
    };
    let end = gpa + range.len();

    while gpa < end {
        let input = TdcallInput {
            leaf: TdCallLeaf::VP_VMCALL,
            rcx: 0x3c00, // pass R10-R13
            rdx: 0,
            r8: 0,
            r9: 0,
            r10: 0, // must be 0 for ghci call
            r11: TdVmCallSubFunction::MapGpa as u64,
            r12: gpa,
            r13: end - gpa,
            r14: 0,
            r15: 0,
        };

        let output = call.tdcall(input);

        // This assertion failing means something has gone horribly wrong with the
        // TDX module, as this call should always succeed with hypercall errors
        // returned in r10.
        assert_eq!(
            output.rax.code(),
            TdCallResultCode::SUCCESS,
            "unexpected nonzero rax {:x} returned by tdcall vmcall",
            u64::from(output.rax)
        );

        let result = TdVmCallR10Result(output.r10);

        match result {
            TdVmCallR10Result::SUCCESS => gpa = end,
            TdVmCallR10Result::RETRY => gpa = output.r11,
            val => return Err(val),
        }
    }

    Ok(())
}

/// Issue a TDG.VP.WR call.
///
/// `field_code` is the field code to use for the call.
///
/// `value` is the value to set, with `mask` being the mask controlling which
/// bits will be set from `value`, as specified by the TDX API.
///
/// Returns the old value of the field.
pub fn tdcall_vp_wr(
    call: &mut impl Tdcall,
    field_code: TdxExtendedFieldCode,
    value: u64,
    mask: u64,
) -> Result<u64, TdCallResult> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_WR,
        rcx: 0,
        rdx: field_code.into(),
        r8: value,
        r9: mask,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(output.r8),
        _ => Err(output.rax),
    }
}

/// Issue a TDG.VP.RD call.
///
/// `field_code` is the field code to use for the call.
pub fn tdcall_vp_rd(
    call: &mut impl Tdcall,
    field_code: TdxExtendedFieldCode,
) -> Result<u64, TdCallResult> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_RD,
        rcx: 0,
        rdx: field_code.into(),
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(output.r8),
        _ => Err(output.rax),
    }
}

/// Issue a TDG.VP.INVGLA call.
pub fn tdcall_vp_invgla(
    call: &mut impl Tdcall,
    gla_flags: TdGlaVmAndFlags,
    gla_info: TdxGlaListInfo,
) -> Result<(), TdCallResult> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VP_INVGLA,
        rcx: gla_flags.into(),
        rdx: gla_info.into(),
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(()),
        _ => Err(output.rax),
    }
}

#[repr(C, align(64))]
struct AddlData {
    /// Report data buffer for TDG.MR.REPORT call.
    pub report_data: [u8; 64],
}

/// Issue a TDG.MR.REPORT call with empty additional data.
pub fn tdcall_mr_report(call: &mut impl Tdcall, report: &mut TdReport) -> Result<(), TdCallResult> {
    let addl_data = AddlData {
        report_data: [0; 64],
    };

    let input = TdcallInput {
        leaf: TdCallLeaf::MR_REPORT,
        rcx: core::ptr::from_mut::<TdReport>(report) as u64,
        rdx: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: addl_data.report_data.as_ptr() as u64,
        r13: 0,
        r14: 0,
        r15: 0,
    };

    let output = call.tdcall(input);

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(()),
        _ => Err(output.rax),
    }
}
