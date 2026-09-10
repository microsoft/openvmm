// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common TDCALL handling for issuing tdcalls and functionality using tdcalls.

#![no_std]
#![forbid(unsafe_code)]

use hvdef::hypercall::HypercallOutput;
use memory_range::AlignedSubranges;
use memory_range::MemoryRange;
use thiserror::Error;
use x86defs::tdx::DmarTarget;
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
use x86defs::tdx::TdgMemPageReleaseRcx;
use x86defs::tdx::TdgMemPageReleaseRcxResult;
use x86defs::tdx::TdgTdiMmioAcceptR9;
use x86defs::tdx::TdgTdiMmioAcceptRcx;
use x86defs::tdx::TdgVmRdResult;
use x86defs::tdx::TdiRdField;
use x86defs::tdx::TdxExtendedFieldCode;
use x86defs::tdx::TdxFunctionId;
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

/// The error information returned from [`tdcall_release_page`].
#[derive(Debug, Error)]
pub enum TdgPageReleaseError {
    /// Unknown error type.
    #[error("unknown error: {0:?}")]
    Unknown(TdCallResultCode),
    /// Page not allocated to TD's GPA Space
    #[error("page is not allocated to GPA space: {0:?}")]
    NotAllocated(TdCallResultCode),
    /// Page is in an invalid entry state
    #[error("page has invalid state [pending: {pending:?}, mmio: {mmio:?}]: {result:?}")]
    EntryStateInvalid {
        /// Associated TDX Result Code
        result: TdCallResultCode,
        /// Page is in PENDING state
        pending: bool,
        /// Page is MMIO
        mmio: bool,
    },
    /// Size mismatch
    #[error("page size mismatch. Expected page level {expected_level:?}: {result:?}")]
    PageSizeMismatch {
        /// Associated TDX Result Code
        result: TdCallResultCode,
        /// Expected page level
        expected_level: TdgMemPageLevel,
    },
    /// Invalid operand
    #[error("invalid operand: {0:?}")]
    Invalid(TdCallResultCode),
    /// Busy Operand
    #[error("operand busy: {0:?}")]
    Busy(TdCallResultCode),
}

/// Issue a TDG.MEM.PAGE.RELEASE call
pub fn tdcall_release_page(
    call: &mut impl Tdcall,
    gpa_page_number: u64,
    as_large_page: bool,
) -> Result<(), TdgPageReleaseError> {
    #[cfg(feature = "tracing")]
    tracing::trace!(gpa_page_number, as_large_page, "tdcall_release_page");

    let rcx = TdgMemPageReleaseRcx::new()
        .with_gpa_page_number(gpa_page_number)
        .with_level(if as_large_page {
            TdgMemPageLevel::Size2Mb
        } else {
            TdgMemPageLevel::Size4k
        });

    let input = TdcallInput {
        leaf: TdCallLeaf::MEM_PAGE_RELEASE,
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

    let result_code = output.rax.code();
    let result_info = TdgMemPageReleaseRcxResult::from(output.rcx);

    match result_code {
        TdCallResultCode::SUCCESS => Ok(()),
        TdCallResultCode::EPT_ENTRY_FREE => Err(TdgPageReleaseError::NotAllocated(result_code)),
        TdCallResultCode::EPT_ENTRY_STATE_INCORRECT => {
            Err(TdgPageReleaseError::EntryStateInvalid {
                result: result_code,
                pending: result_info.pending(),
                mmio: result_info.mmio(),
            })
        }
        TdCallResultCode::PAGE_SIZE_MISMATCH => Err(TdgPageReleaseError::PageSizeMismatch {
            result: result_code,
            expected_level: result_info.level(),
        }),
        TdCallResultCode::OPERAND_INVALID => Err(TdgPageReleaseError::Invalid(result_code)),
        TdCallResultCode::OPERAND_BUSY => Err(TdgPageReleaseError::Busy(result_code)),
        val => Err(TdgPageReleaseError::Unknown(val)),
    }
}

/// Releases memory from `range` using [`tdcall_release_page`].
pub fn release_pages(
    call: &mut impl Tdcall,
    range: MemoryRange,
) -> Result<(), TdgPageReleaseError> {
    #[cfg(feature = "tracing")]
    tracing::trace!(%range, "release_pages");

    for_each_tdcall_page(range, |gpn, is_large_page| {
        match tdcall_release_page(call, gpn, is_large_page) {
            Ok(_) => Ok(TdcallPageOperationOutcome::Advance),
            Err(TdgPageReleaseError::PageSizeMismatch {
                expected_level: TdgMemPageLevel::Size4k,
                ..
            }) if is_large_page => Ok(TdcallPageOperationOutcome::Retry4k),
            Err(e) => Err(e),
        }
    })
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
                // TDX Connect gives MMIO pages different ATTR.WR/ATTR.RD rules,
                // so a readback is not required to report back what the write
                // asked for. Skip the check on a TDX Connect partition.
                let config_flags = x86defs::tdx::TdConfigFlags::from_bits(
                    tdcall_vm_rd(call, x86defs::tdx::TDX_FIELD_CODE_CONFIG_FLAGS)
                        .expect("TDG.VM.RD should not fail for CONFIG_FLAGS"),
                );

                if !config_flags.tdx_connect() {
                    let result = tdcall_page_attr_rd(
                        call,
                        mapping.gpa_page_number() * x86defs::X64_PAGE_SIZE,
                    )
                    .unwrap();
                    assert_eq!(u64::from(mapping), result.mapping.into());
                    assert_eq!(attributes.l1(), result.attributes.l1());
                    assert_eq!(
                        attributes.into_bits() & mask.with_reserved(0).into_bits(),
                        result.attributes.into_bits() & mask.with_reserved(0).into_bits()
                    );
                }
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

    for_each_tdcall_page(range, |gpn, is_large_page| {
        match tdcall_accept_pages(call, gpn, is_large_page) {
            Ok(_) => {
                set_attributes(
                    call,
                    TdgMemPageAttrWriteRcx::new()
                        .with_gpa_page_number(gpn)
                        .with_level(if is_large_page {
                            TdgMemPageLevel::Size2Mb
                        } else {
                            TdgMemPageLevel::Size4k
                        }),
                )?;
                Ok(TdcallPageOperationOutcome::Advance)
            }
            Err(e) => match e {
                TdCallResultCode::OPERAND_BUSY => Err(AcceptPagesError::Busy(e)),
                TdCallResultCode::OPERAND_INVALID => Err(AcceptPagesError::Invalid(e)),
                TdCallResultCode::PAGE_ALREADY_ACCEPTED => {
                    panic!("page {} already accepted", gpn);
                }
                TdCallResultCode::PAGE_SIZE_MISMATCH if is_large_page => {
                    #[cfg(feature = "tracing")]
                    tracing::trace!("accept pages size mismatch returned");
                    Ok(TdcallPageOperationOutcome::Retry4k)
                }
                _ => Err(AcceptPagesError::Unknown(e)),
            },
        }
    })
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

    for_each_tdcall_page(range, |gpn, is_large_page| {
        let level = if is_large_page {
            TdgMemPageLevel::Size2Mb
        } else {
            TdgMemPageLevel::Size4k
        };
        let mapping = TdgMemPageAttrWriteRcx::new()
            .with_gpa_page_number(gpn)
            .with_level(level);

        match set_page_attr(call, mapping, attributes, mask) {
            Ok(()) => Ok(TdcallPageOperationOutcome::Advance),
            Err(TdCallResultCode::PAGE_SIZE_MISMATCH) if is_large_page => {
                #[cfg(feature = "tracing")]
                tracing::trace!("set pages attr size mismatch returned");
                Ok(TdcallPageOperationOutcome::Retry4k)
            }
            Err(e) => Err(e),
        }
    })
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

/// Issue a TDG.VM.WR call to write a TD-scope metadata field (e.g. `TD_CTLS`).
///
/// `field_code` is the field code to use for the call.
///
/// `value` is the value to set, with `mask` being the mask controlling which
/// bits will be set from `value`, as specified by the TDX API.
///
/// Returns the old value of the field.
pub fn tdcall_vm_wr(
    call: &mut impl Tdcall,
    field_code: TdxExtendedFieldCode,
    value: u64,
    mask: u64,
) -> Result<u64, TdCallResult> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VM_WR,
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

/// Issue a TDG.SYS.RD call to read a global-scope TDX module metadata field
/// (e.g. `TDX_FEATURES0`).
///
/// `field_id` is the metadata field ID to read.
///
/// Returns the field value.
pub fn tdcall_sys_rd(call: &mut impl Tdcall, field_id: u64) -> Result<u64, TdCallResult> {
    let input = TdcallInput {
        leaf: TdCallLeaf::SYS_RD,
        rcx: 0,
        rdx: field_id,
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

/// Issue a TDG.VM.RD call
pub fn tdcall_vm_rd(
    call: &mut impl Tdcall,
    field_id: TdxExtendedFieldCode,
) -> Result<TdgVmRdResult, TdCallResult> {
    let input = TdcallInput {
        leaf: TdCallLeaf::VM_RD,
        rcx: 0,
        rdx: field_id.into_bits(),
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
    if output.rax.code() != TdCallResultCode::SUCCESS {
        return Err(output.rax);
    }

    Ok(output.r8)
}

/// Builds the [`TdcallInput`] shared by the TDX Connect guest-side leaves.
///
/// All of them take `GFUNCTION_ID` in RCX and leave R10-R15 zero, which the
/// runtime (ioctl-based) tdcall path requires.
fn tdi_input(
    leaf: TdCallLeaf,
    function_id: TdxFunctionId,
    rdx: u64,
    r8: u64,
    r9: u64,
) -> TdcallInput {
    TdcallInput {
        leaf,
        rcx: u32::from(function_id).into(),
        rdx,
        r8,
        r9,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    }
}

/// Issue a TDG.TDI.RD call to read a field of a TDI's control structure.
///
/// `out_buf_gpa` is the GPA of a 4K private page receiving the hash for the
/// `GET_TDISP_REPORT_HASH` and `GET_DEVICE_ATTESTATION_INFO_HASH` field codes,
/// in which case the returned value is the hash length in bytes. It must be
/// zero for every other field code, where the returned value is the field
/// itself.
///
/// Note that a failure status is meaningful on its own: per the TDX Connect ABI
/// EAS, a `GET_TDISP_STATE` read succeeds only while the TDI is bound, so
/// `TDI_NOT_PRESENT`, `TDI_INVALID_METADATA`, and `TDI_INVALID_STATE` all
/// report an unbound TDI rather than a malformed call.
pub fn tdcall_tdi_rd(
    call: &mut impl Tdcall,
    function_id: TdxFunctionId,
    field: TdiRdField,
    out_buf_gpa: u64,
) -> Result<u64, TdCallResult> {
    #[cfg(feature = "tracing")]
    tracing::trace!(?function_id, ?field, out_buf_gpa, "tdcall_tdi_rd");

    let output = call.tdcall(tdi_input(
        TdCallLeaf::TDI_RD,
        function_id,
        field.0,
        out_buf_gpa,
        0,
    ));

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(output.rcx),
        _ => Err(output.rax),
    }
}

/// Issue a TDG.TDI.START call to authorize starting a TDI's interface.
///
/// `exp_bind_session` is the bind session ID the TD expects the TDI to still be
/// on, previously read with
/// [`tdcall_tdi_rd`]`(.., TdiRdField::GET_BIND_SESSION_ID, 0)`.
pub fn tdcall_tdi_start(
    call: &mut impl Tdcall,
    function_id: TdxFunctionId,
    exp_bind_session: u64,
) -> Result<(), TdCallResult> {
    #[cfg(feature = "tracing")]
    tracing::trace!(?function_id, exp_bind_session, "tdcall_tdi_start");

    let output = call.tdcall(tdi_input(
        TdCallLeaf::TDI_START,
        function_id,
        exp_bind_session,
        0,
        0,
    ));

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(()),
        _ => Err(output.rax),
    }
}

/// Issue a TDG.DMAR.ACCEPT call to accept a TDI's DMA remapping entry, allowing
/// the device to DMA into the TD's private memory.
pub fn tdcall_dmar_accept(
    call: &mut impl Tdcall,
    function_id: TdxFunctionId,
    target: DmarTarget,
) -> Result<(), TdCallResult> {
    #[cfg(feature = "tracing")]
    tracing::trace!(?function_id, ?target, "tdcall_dmar_accept");

    let output = call.tdcall(tdi_input(
        TdCallLeaf::DMAR_ACCEPT,
        function_id,
        target.into(),
        0,
        0,
    ));

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(()),
        _ => Err(output.rax),
    }
}

/// Issue a TDG.TDI.MMIO.ACCEPT call to accept a sub-range of a TDI's MMIO into
/// the TD's private address space.
///
/// `mmio_range_idx` is the MMIO range index from the device interface report,
/// i.e. which of the TDI's ranges `gpa_base_and_level` falls in.
///
/// The leaf is interruptible and architecturally returns how much of the range
/// is left in R9 so the caller can resume. The runtime tdcall path cannot read
/// R9 back (`struct mshv_tdcall` only surfaces R10/R11 as outputs), so callers
/// on that path must pass a `range` of a single page and drive their own
/// cursor. Once the R9 output is plumbed through the ioctl this helper should
/// return the remaining range instead.
pub fn tdcall_tdi_mmio_accept(
    call: &mut impl Tdcall,
    function_id: TdxFunctionId,
    gpa_base_and_level: TdgTdiMmioAcceptRcx,
    mmio_range_idx: u16,
    range: TdgTdiMmioAcceptR9,
) -> Result<(), TdCallResult> {
    #[cfg(feature = "tracing")]
    tracing::trace!(
        ?function_id,
        ?gpa_base_and_level,
        mmio_range_idx,
        ?range,
        "tdcall_tdi_mmio_accept"
    );

    // Unlike the other Connect leaves, this one takes the GPA in RCX and the
    // function id in R8.
    let output = call.tdcall(TdcallInput {
        leaf: TdCallLeaf::TDI_MMIO_ACCEPT,
        rcx: gpa_base_and_level.into(),
        rdx: mmio_range_idx.into(),
        r8: u32::from(function_id).into(),
        r9: range.into(),
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
    });

    match output.rax.code() {
        TdCallResultCode::SUCCESS => Ok(()),
        _ => Err(output.rax),
    }
}

/// Outcome of a per-page TDCall operation.
enum TdcallPageOperationOutcome {
    /// The operation succeeded; advance past this page.
    Advance,

    /// The operation returned PAGE_SIZE_MISMATCH on a 2MB attempt;
    /// retry the same page as 4K.
    Retry4k,
}

/// Processes a memory range for memory operation TDCalls. Prefers 2MB when possible and handles page size mismatch.
fn for_each_tdcall_page<E>(
    range: MemoryRange,
    mut op: impl FnMut(u64, bool) -> Result<TdcallPageOperationOutcome, E>,
) -> Result<(), E> {
    let range = AlignedSubranges::new(range).with_max_range_len(x86defs::X64_LARGE_PAGE_SIZE);

    for mut subrange in range {
        let mut is_large_page = subrange.alignment(0) == x86defs::X64_LARGE_PAGE_SIZE;

        while !subrange.is_empty() {
            match op(subrange.start_4k_gpn(), is_large_page)? {
                TdcallPageOperationOutcome::Advance => {
                    let page_size = if is_large_page {
                        x86defs::X64_LARGE_PAGE_SIZE
                    } else {
                        x86defs::X64_PAGE_SIZE
                    };

                    subrange = MemoryRange::new(subrange.start() + page_size..subrange.end())
                }
                TdcallPageOperationOutcome::Retry4k => {
                    assert!(
                        is_large_page,
                        "Retry4k requested while already retrying as 4K — would loop forever",
                    );
                    is_large_page = false;
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use x86defs::tdx::TdispInterfaceState;

    /// A [`Tdcall`] that records the input it was handed and replays a canned
    /// output, so the tests can assert on register encoding without a TDX host.
    struct RecordingTdcall {
        input: Option<TdcallInput>,
        rax: TdCallResultCode,
        rcx: u64,
    }

    impl RecordingTdcall {
        fn new(rax: TdCallResultCode) -> Self {
            Self {
                input: None,
                rax,
                rcx: 0,
            }
        }

        fn with_rcx(mut self, rcx: u64) -> Self {
            self.rcx = rcx;
            self
        }

        /// The recorded input, panicking if no tdcall was issued.
        fn input(&self) -> &TdcallInput {
            self.input.as_ref().expect("no tdcall was issued")
        }
    }

    impl Tdcall for RecordingTdcall {
        fn tdcall(&mut self, input: TdcallInput) -> TdcallOutput {
            // The runtime path only supports one tdcall per marshalled ioctl,
            // so a helper issuing more than one would be a bug.
            assert!(self.input.is_none(), "more than one tdcall issued");

            // The runtime path asserts these are zero before marshalling; catch
            // a violation here rather than as a panic on a live TD.
            assert_eq!(input.r10, 0);
            assert_eq!(input.r11, 0);
            assert_eq!(input.r12, 0);
            assert_eq!(input.r13, 0);
            assert_eq!(input.r14, 0);
            assert_eq!(input.r15, 0);

            self.input = Some(input);

            TdcallOutput {
                rax: TdCallResult::new().with_code(self.rax),
                rcx: self.rcx,
                rdx: 0,
                r8: 0,
                r10: 0,
                r11: 0,
            }
        }
    }

    fn test_function_id() -> TdxFunctionId {
        TdxFunctionId::new().with_requester_id(0x1a2b)
    }

    #[test]
    fn tdi_rd_encoding() {
        let mut call =
            RecordingTdcall::new(TdCallResultCode::SUCCESS).with_rcx(TdispInterfaceState::RUN.0);

        let value = tdcall_tdi_rd(
            &mut call,
            test_function_id(),
            TdiRdField::GET_TDISP_STATE,
            0,
        )
        .unwrap();

        assert_eq!(TdispInterfaceState(value), TdispInterfaceState::RUN);
        let input = call.input();
        assert_eq!(input.leaf, TdCallLeaf::TDI_RD);
        assert_eq!(input.rcx, 0x1a2b);
        assert_eq!(input.rdx, TdiRdField::GET_TDISP_STATE.0);
        assert_eq!(input.r8, 0);
    }

    #[test]
    fn tdi_rd_reports_failure_status() {
        let mut call = RecordingTdcall::new(TdCallResultCode::TDI_NOT_PRESENT);

        let err = tdcall_tdi_rd(
            &mut call,
            test_function_id(),
            TdiRdField::GET_TDISP_VERSION,
            0,
        )
        .unwrap_err();

        assert_eq!(err.code(), TdCallResultCode::TDI_NOT_PRESENT);
    }

    #[test]
    fn tdi_rd_passes_out_buf() {
        let mut call = RecordingTdcall::new(TdCallResultCode::SUCCESS).with_rcx(48);

        let len = tdcall_tdi_rd(
            &mut call,
            test_function_id(),
            TdiRdField::GET_TDISP_REPORT_HASH,
            0x4000,
        )
        .unwrap();

        assert_eq!(len, 48);
        assert_eq!(call.input().r8, 0x4000);
    }

    #[test]
    fn function_id_encodes_segment() {
        let function_id = TdxFunctionId::new()
            .with_requester_id(0x1a2b)
            .with_requester_segment(0x7)
            .with_segment_valid(true);

        let mut call = RecordingTdcall::new(TdCallResultCode::SUCCESS);
        tdcall_tdi_rd(&mut call, function_id, TdiRdField::GET_TDISP_VERSION, 0).unwrap();

        // segment_valid in bit 24, segment in bits 23:16, requester id in 15:0.
        assert_eq!(call.input().rcx, 0x0107_1a2b);
    }

    #[test]
    fn tdi_start_encoding() {
        let mut call = RecordingTdcall::new(TdCallResultCode::SUCCESS);

        tdcall_tdi_start(&mut call, test_function_id(), 0xfeed).unwrap();

        let input = call.input();
        assert_eq!(input.leaf, TdCallLeaf::TDI_START);
        assert_eq!(input.rcx, 0x1a2b);
        assert_eq!(input.rdx, 0xfeed);
    }

    #[test]
    fn dmar_accept_encoding() {
        let mut call = RecordingTdcall::new(TdCallResultCode::SUCCESS);

        tdcall_dmar_accept(
            &mut call,
            test_function_id(),
            DmarTarget::new().with_vm_idx(0),
        )
        .unwrap();

        let input = call.input();
        assert_eq!(input.leaf, TdCallLeaf::DMAR_ACCEPT);
        assert_eq!(input.rcx, 0x1a2b);
        assert_eq!(input.rdx, 0);
        assert_eq!(input.r8, 0);
    }

    #[test]
    fn tdi_mmio_accept_encoding() {
        let mut call = RecordingTdcall::new(TdCallResultCode::SUCCESS);

        tdcall_tdi_mmio_accept(
            &mut call,
            test_function_id(),
            TdgTdiMmioAcceptRcx::new()
                .with_level(TdgMemPageLevel::Size4k)
                .with_gpa_page_number(0x8000),
            2,
            TdgTdiMmioAcceptR9::new()
                .with_range_size(1)
                .with_range_offset(3),
        )
        .unwrap();

        let input = call.input();
        assert_eq!(input.leaf, TdCallLeaf::TDI_MMIO_ACCEPT);
        // The GPA goes in RCX and the function id in R8 for this leaf, unlike
        // the other Connect leaves.
        assert_eq!(
            TdgTdiMmioAcceptRcx::from(input.rcx).gpa_page_number(),
            0x8000
        );
        assert_eq!(input.rdx, 2);
        assert_eq!(input.r8, 0x1a2b);
        assert_eq!(TdgTdiMmioAcceptR9::from(input.r9).range_size(), 1);
        assert_eq!(TdgTdiMmioAcceptR9::from(input.r9).range_offset(), 3);
    }
}
