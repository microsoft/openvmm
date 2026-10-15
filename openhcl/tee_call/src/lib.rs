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
    #[error("failed to get a TDX derived key via /dev/tdx_guest")]
    GetTdxDerivedKey(#[source] tdx_guest_device::Error),
    #[error(
        "TDX signer-based hardware sealing is not supported: TDX has no signer identity register \
         to bind a measurement-independent key to, so refusing to derive an under-bound key"
    )]
    TdxSignerPolicyUnsupported,
    #[error("key derivation SVN does not match the TEE type")]
    KeyDerivationSvnMismatch,
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

// TDX and SNP derived key size are equal so we can return either of them as
// [`HW_DERIVED_KEY_LENGTH`].
static_assertions::const_assert_eq!(
    x86defs::snp::SNP_DERIVED_KEY_SIZE,
    x86defs::tdx::TDX_DERIVED_KEY_SIZE
);

/// Type of the TEE
#[derive(Debug)]
pub enum TeeType {
    /// AMD SEV-SNP
    Snp,
    /// Intel TDX
    Tdx,
    /// ARM CCA
    Cca,
    /// Virtualization-based Security (VBS)
    Vbs,
}

/// TEE-specific SVN material bound into the hardware-derived key for
/// anti-rollback. Each variant carries exactly the fields its TEE mixes into
/// key derivation and maps 1:1 to a `HardwareKeyProtector` header version.
#[derive(Debug, Clone, Copy)]
pub enum KeyDerivationSvn {
    /// SNP reported TCB version (`HW_KEY_PROTECTOR` v2).
    Snp {
        /// `reported_tcb` from the SNP attestation report.
        tcb_version: u64,
    },
    /// TDX report SVNs, both bound into the key (`HW_KEY_PROTECTOR` v3).
    Tdx {
        /// Module `TEE_TCB_SVN` (16 bytes, verbatim from the report).
        tee_tcb_svn: [u8; 16],
        /// Platform `CPU_SVN` (16 bytes, verbatim from the report).
        cpu_svn: [u8; 16],
    },
}

/// The result of the `get_attestation_report`.
pub struct GetAttestationReportResult {
    /// The report in raw bytes
    pub report: Vec<u8>,
    /// SVN material for hardware key derivation; `None` for TEEs that don't
    /// derive keys.
    pub key_derivation_svn: Option<KeyDerivationSvn>,
}

/// Key derivation policy
#[derive(Debug, Clone, Copy)]
pub struct KeyDerivationPolicy {
    /// TEE-specific SVN material bound into the derived key.
    pub svn: KeyDerivationSvn,
    /// Whether to mix measurement into the key derivation.
    pub mix_measurement: bool,
}

/// Trait that defines the get attestation report interface for TEE.
pub trait TeeCall: Send + Sync {
    /// Get the hardware-backed attestation report.
    ///
    /// # Arguments
    /// * `report_data` - The report data to include in the attestation report.
    ///
    /// Returns the attestation report result.
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, Error>;
    /// Whether [`TeeCallGetDerivedKey`] is implemented.
    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey>;
    /// Get the [`TeeType`].
    fn tee_type(&self) -> TeeType;
}

/// Optional sub-trait that defines the get-derived-key interface for a TEE.
pub trait TeeCallGetDerivedKey: TeeCall {
    /// Get the derived key that should be deterministic based on the hardware and software
    /// configurations.
    ///
    /// # Arguments
    /// * `policy` - The key derivation policy to use.
    ///
    /// Returns the derived key.
    fn get_derived_key(
        &self,
        policy: KeyDerivationPolicy,
    ) -> Result<[u8; HW_DERIVED_KEY_LENGTH], Error>;
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
            key_derivation_svn: Some(KeyDerivationSvn::Snp {
                tcb_version: report.reported_tcb,
            }),
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
    fn get_derived_key(
        &self,
        policy: KeyDerivationPolicy,
    ) -> Result<[u8; HW_DERIVED_KEY_LENGTH], Error> {
        let KeyDerivationSvn::Snp { tcb_version } = policy.svn else {
            return Err(Error::KeyDerivationSvnMismatch);
        };

        let dev = sev_guest_device::SevGuestDevice::open().map_err(Error::OpenDevSevGuest)?;

        // Derive a key mixing in following data:
        // - GuestPolicy (do not allow different polices to derive same secret)
        // - Measurement (will not work across release)
        // - TcbVersion (do not derive same key on older TCB that might have a bug)
        let guest_field_select = x86defs::snp::GuestFieldSelect::default()
            .with_guest_policy(true)
            .with_measurement(policy.mix_measurement)
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
pub struct TdxCall {
    /// Whether `TD_CTLS.ENABLE_HW_SEAL_KEYS` was successfully set for this TD
    /// this boot. Unlike SNP (where key derivation is always available), TDX
    /// hardware-bound seal keys are opt-in at runtime and depend on TDX module
    /// support, so this is captured at enable time and used to gate
    /// [`TeeCall::supports_get_derived_key`].
    hw_seal_keys_enabled: bool,
}

impl TdxCall {
    /// Creates a new [`TdxCall`].
    ///
    /// * `hw_seal_keys_enabled` - whether `TD_CTLS.ENABLE_HW_SEAL_KEYS` was
    ///   successfully enabled for this TD, making `TDG.MR.KEY.GET` available.
    pub fn new(hw_seal_keys_enabled: bool) -> Self {
        Self {
            hw_seal_keys_enabled,
        }
    }
}

impl TeeCall for TdxCall {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, Error> {
        let dev = tdx_guest_device::TdxGuestDevice::open().map_err(Error::OpenDevTdxGuest)?;
        let report = dev
            .get_report(*report_data, 0)
            .map_err(Error::GetTdxReport)?;

        let mut tee_tcb_svn = [0u8; 16];
        tee_tcb_svn.copy_from_slice(report.tee_tcb_info.tee_tcb_svn.as_bytes());

        Ok(GetAttestationReportResult {
            report: report.as_bytes().to_vec(),
            key_derivation_svn: Some(KeyDerivationSvn::Tdx {
                tee_tcb_svn,
                cpu_svn: report.report_mac_struct.cpu_svn,
            }),
        })
    }

    /// Key derivation is supported by TDX via `TDG.MR.KEY.GET`, but only when
    /// hardware-bound seal keys were successfully enabled for this TD.
    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        self.hw_seal_keys_enabled
            .then_some(self as &dyn TeeCallGetDerivedKey)
    }

    /// Return TeeType::Tdx.
    fn tee_type(&self) -> TeeType {
        TeeType::Tdx
    }
}

impl TeeCallGetDerivedKey for TdxCall {
    /// Get the derived key from /dev/tdx_guest via the `TDG.MR.KEY.GET` TDCALL.
    fn get_derived_key(
        &self,
        policy: KeyDerivationPolicy,
    ) -> Result<[u8; HW_DERIVED_KEY_LENGTH], Error> {
        let KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn,
        } = policy.svn
        else {
            return Err(Error::KeyDerivationSvnMismatch);
        };

        // Fail safe for the signer sealing policy on TDX.
        //
        // `mix_measurement == false` corresponds to the signer policy, which
        // asks for a key that is *independent* of the OpenHCL measurement (so it
        // survives servicing). On SNP this still binds the key to the guest
        // policy and TCB, but TDX's `TDKEYPOLICY` has no signer/identity
        // register to substitute for `MRTD`: clearing `MRTD` would leave the key
        // bound to nothing TD-specific (only the platform seal secret, the
        // module `TEE_TCB_SVN`, the `CPU_SVN`, and a fixed salt), so any
        // co-located TD on the same platform+TCB could derive the identical key
        // and unseal the VMGS DEK. Refuse rather than seal with an under-bound
        // key; the caller skips
        // hardware sealing (or fails closed if it is the required source). The
        // rejection is logged by the higher-level guard in `underhill_attestation`.
        if !policy.mix_measurement {
            return Err(Error::TdxSignerPolicyUnsupported);
        }

        let dev = tdx_guest_device::TdxGuestDevice::open().map_err(Error::OpenDevTdxGuest)?;

        // Build a `TDKEYREQUEST` that binds the derived key to the TD's identity
        // so that the key is deterministic across boots but unique per-TD. This
        // mirrors the SNP key derivation, which mixes in the guest measurement
        // and TCB version.
        //
        // - `MRTD` (the build-time measurement) is selected via the key policy;
        //   the TDX module reads it from the TD's own measurement state.
        // - `TEE_TCB_SVN` and `CPU_SVN` are the values recorded at seal time
        //   (verbatim 16-byte report fields). The module requires a valid TCB
        //   SVN and rejects an all-zero `TEE_TCB_SVN`; supplying the recorded
        //   values satisfies that check and provides anti-rollback binding on
        //   both the module and CPU/microcode TCB. Using the recorded values
        //   (rather than the current report) keeps the derived key stable so a
        //   previously sealed DEK can still be unsealed.
        //
        // The `salt` carries a fixed label so the derived key is domain
        // separated from any other use of `TDG.MR.KEY.GET`.
        let mut salt = [0u8; 32];
        let label = b"TDXHWSEAL";
        salt[..label.len()].copy_from_slice(label);

        let key_request = x86defs::tdx::TdKeyRequest {
            key_name: x86defs::tdx::TDX_KEY_NAME_SEAL,
            sw_key_name: 0,
            // Request a 256-bit key. `TDX_FEATURES0.SEALKEY_128` enumerates
            // whether a 128-bit key is *available*; when it is clear, only the
            // 256-bit key size is valid, so request 256-bit (which also matches
            // `HW_DERIVED_KEY_LENGTH`).
            key_size: x86defs::tdx::TDX_KEY_SIZE_256,
            _reserved0: [0u8; 4],
            // Always select `MRTD` so the derived key is bound to the TD's
            // build-time measurement (the analog of SNP mixing in the launch
            // measurement). Only the hash policy (`mix_measurement == true`)
            // reaches here; the signer policy is rejected above because TDX has
            // no identity register to bind to when `MRTD` is cleared.
            key_policy: x86defs::tdx::TdxKeyPolicy::new().with_mr_td(true),
            attributes_mask: 0,
            xfam_mask: 0,
            // `CPU_SVN` and `TEE_TCB_SVN` recorded at seal time bind the key to
            // both the CPU/microcode and module TCB for anti-rollback.
            cpu_svn,
            tee_tcb_svn,
            isv_svn: 0,
            mr_config_svn: 0,
            mr_owner_config_svn: 0,
            salt,
            _reserved1: [0u8; 26],
        };

        let derived_key = dev
            .get_derived_key(&key_request)
            .map_err(Error::GetTdxDerivedKey)?;

        if derived_key.iter().all(|&x| x == 0) {
            Err(Error::AllZeroKey)?
        }

        Ok(derived_key)
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
            key_derivation_svn: None,
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
