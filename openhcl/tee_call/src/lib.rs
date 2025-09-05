// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module includes the `TeeCall` trait and its implementation. The trait defines
//! the trusted execution environment (TEE)-specific APIs for attestation and data dealing.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use hcl::ioctl::MshvHvcall;
use hvdef::HypercallCode;
use thiserror::Error;
use zerocopy::IntoBytes;

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to open /dev/sev-guest")]
    OpenDevSevGuest(#[source] sev_guest_device::Error),
    #[error("failed to get an SNP report via /dev/sev-guest")]
    GetSnpReport(#[source] sev_guest_device::Error),
    #[error("failed to get an SNP derived key via /dev/sev-guest")]
    GetSnpDerivedKey(#[source] sev_guest_device::Error),
    #[error("got all-zeros key")]
    AllZeroKey,
    #[error("failed to open /dev/tdx_guest")]
    OpenDevTdxGuest(#[source] tdx_guest_device::Error),
    #[error("failed to get a TDX report via /dev/tdx_guest")]
    GetTdxReport(#[source] tdx_guest_device::Error),
    #[error("failed to open VBS guest device")]
    OpenDevVbsGuest(#[source] hcl::ioctl::Error),
    #[error("failed to get a VBS report via VBS guest device")]
    GetVbsReport(#[source] hvdef::HvError),
}

/// Use the SNP-defined derived key size for now.
pub const HW_DERIVED_KEY_LENGTH: usize = x86defs::snp::SNP_DERIVED_KEY_SIZE;

/// Use the SNP-defined report data size for now.
// DEVNOTE: This value should be upper bound among all the supported TEE types.
pub const REPORT_DATA_SIZE: usize = x86defs::snp::SNP_REPORT_DATA_SIZE;

// TDX and SNP report data size are equal so we can use either of them
static_assertions::const_assert_eq!(
    x86defs::snp::SNP_REPORT_DATA_SIZE,
    x86defs::tdx::TDX_REPORT_DATA_SIZE
);

/// Type of the TEE
#[derive(Debug)]
pub enum TeeType {
    /// AMD SEV-SNP
    Snp,
    /// Intel TDX
    Tdx,
    /// Virtualization-based Security (VBS)
    Vbs,
}

/// The result of the `get_attestation_report`.
pub struct GetAttestationReportResult {
    /// The report in raw bytes
    pub report: Vec<u8>,
    /// The optional tcb version
    pub tcb_version: Option<u64>,
}

/// Trait that defines the get attestation report interface for TEE.
// TODO VBS: Implement the trait for VBS
pub trait TeeCall: Send + Sync {
    /// Get the hardware-backed attestation report.
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, Error>;
    /// Whether [`TeeCallGetDerivedKey`] is implemented.
    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey>;
    /// Get the [`TeeType`].
    fn tee_type(&self) -> TeeType;
}

/// Optional sub-trait that defines get derived key interface for TEE.
pub trait TeeCallGetDerivedKey: TeeCall {
    /// Get the derived key that should be deterministic based on the hardware and software
    /// configurations.
    fn get_derived_key(&self, tcb_version: u64) -> Result<[u8; HW_DERIVED_KEY_LENGTH], Error>;
}

/// Implementation of [`TeeCall`] for SNP
pub struct SnpCall;

impl TeeCall for SnpCall {
    /// Get the attestation report from /dev/sev-guest.
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, Error> {
        let dev = sev_guest_device::SevGuestDevice::open().map_err(Error::OpenDevSevGuest)?;
        let report = dev
            .get_report(*report_data, 0)
            .map_err(Error::GetSnpReport)?;

        Ok(GetAttestationReportResult {
            report: report.as_bytes().to_vec(),
            tcb_version: Some(report.reported_tcb),
        })
    }

    /// Key derivation is supported by SNP
    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        Some(self)
    }

    /// Return TeeType::Snp.
    fn tee_type(&self) -> TeeType {
        TeeType::Snp
    }
}

impl TeeCallGetDerivedKey for SnpCall {
    /// Get the derived key from /dev/sev-guest.
    fn get_derived_key(&self, tcb_version: u64) -> Result<[u8; HW_DERIVED_KEY_LENGTH], Error> {
        let dev = sev_guest_device::SevGuestDevice::open().map_err(Error::OpenDevSevGuest)?;

        // Derive a key mixing in following data:
        // - GuestPolicy (do not allow different polices to derive same secret)
        // - Measurement (will not work across release)
        // - TcbVersion (do not derive same key on older TCB that might have a bug)
        let guest_field_select = x86defs::snp::GuestFieldSelect::default()
            .with_guest_policy(true)
            .with_measurement(true)
            .with_tcb_version(true);

        let derived_key = dev
            .get_derived_key(
                0, // VECK
                guest_field_select.into(),
                0, // VMPL 0
                0, // default guest svn to 0
                tcb_version,
            )
            .map_err(Error::GetSnpDerivedKey)?;

        if derived_key.iter().all(|&x| x == 0) {
            Err(Error::AllZeroKey)?
        }

        Ok(derived_key)
    }
}

/// Implementation of [`TeeCall`] for TDX
pub struct TdxCall;

impl TeeCall for TdxCall {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, Error> {
        let dev = tdx_guest_device::TdxGuestDevice::open().map_err(Error::OpenDevTdxGuest)?;
        let report = dev
            .get_report(*report_data, 0)
            .map_err(Error::GetTdxReport)?;

        Ok(GetAttestationReportResult {
            report: report.as_bytes().to_vec(),
            // Only needed by key derivation, return None for now
            tcb_version: None,
        })
    }

    /// Key derivation is currently not supported by TDX
    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        None
    }

    /// Return TeeType::Tdx.
    fn tee_type(&self) -> TeeType {
        TeeType::Tdx
    }
}

/// Implementation of [`TeeCall`] for VBS
pub struct VbsCall;

impl TeeCall for VbsCall {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, Error> {
        let mshv_hvcall = MshvHvcall::new().map_err(Error::OpenDevVbsGuest)?;
        mshv_hvcall.set_allowed_hypercalls(&[HypercallCode::HvCallVbsVmCallReport]);
        let report = mshv_hvcall
            .vbs_vm_call_report(report_data)
            .map_err(Error::GetVbsReport)?;

        Ok(GetAttestationReportResult {
            report: report[..hvdef::vbs::VBS_REPORT_SIZE].to_vec(),
            // Only needed by key derivation, return None for now
            tcb_version: None,
        })
    }

    /// Key derivation is currently not supported by VBS
    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        None
    }

    /// Return TeeType::Vbs.
    fn tee_type(&self) -> TeeType {
        TeeType::Vbs
    }
}
