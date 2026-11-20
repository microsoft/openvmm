// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local hardware protector verification and recreation for runtime recovery.
//!
//! These operations neither access VMGS nor make GET/SKR callouts. The caller
//! supplies the active DEK and is responsible for persisting a replacement
//! protector. Hardware-derived keys are freshly obtained on every operation;
//! they must not be cached across migration, even when the SVN is unchanged.
//!
//! Runtime recovery must use a resident [`RuntimeSealingState`], initialized from
//! trusted local hardware or restored through authenticated, VM-bound, fresh
//! servicing state before worker events are accepted. It enforces historical
//! TCB checks for SNP only; TDX destination policy belongs to migration admission.
//! SNP's host-configurable `reported_tcb` can lower the requested sealing SVN
//! without a firmware change or migration. Its resident floor prevents a host
//! notification from turning that change into a lower-SVN protector. TDX uses
//! local module/CPU SVN metadata, not SNP's configurable reported-TCB selection;
//! comparing it to the source platform would impose a separate admission policy.
//! Both TEEs require fresh candidate verification and independent key derivation.

mod saved_state;
mod tcb_floor;

pub use saved_state::SavedRuntimeSealingState;
pub use saved_state::SavedRuntimeSealingTee;

use self::tcb_floor::SnpDomain;
use self::tcb_floor::SnpTcbFloor;
use crate::hardware_key_sealing::HardwareDerivedKeys;
use crate::hardware_key_sealing::HardwareDerivedKeysError;
use crate::hardware_key_sealing::HardwareKeySealingError;
use crate::hardware_key_sealing::HwKeyProtector;
use crate::hardware_key_sealing::seal_key;
use crate::vmgs::parse_hardware_key_protector;
use cvm_tracing::CVM_ALLOWED;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::HardwareSealingPolicy;
use openhcl_attestation_protocol::vmgs;
use tee_call::GetAttestationReportResult;
use tee_call::KeyDerivationPolicy;
use tee_call::KeyDerivationSvn;
use tee_call::REPORT_DATA_SIZE;
use tee_call::TeeCall;
use tee_call::TeeCallGetDerivedKey;
use tee_call::TeeType;
use thiserror::Error;
use zerocopy::IntoBytes;

/// A local runtime sealing failure, as distinct from a stale or invalid
/// protector. No error contains the DEK, hardware secret, or derived keys.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(ErrorInner);

#[derive(Debug, Error)]
enum ErrorInner {
    #[error("hardware sealing policy is disabled")]
    DisabledPolicy,
    #[error("TDX signer-based hardware sealing is unsupported")]
    TdxSignerPolicyUnsupported,
    #[error("TEE does not support hardware sealing")]
    UnsupportedTee,
    #[error("failed to obtain a local TEE report")]
    Report(#[source] tee_call::Error),
    #[error("local TEE report has no key derivation SVN")]
    MissingKeyDerivationSvn,
    #[error("local report key derivation SVN does not match the TEE")]
    ReportSvnMismatch,
    #[error("local SNP report is malformed or truncated")]
    MalformedReport,
    #[error("local SNP report TCB does not match its key derivation SVN")]
    ReportTcbMismatch,
    #[error("local TCB has a component below the runtime floor")]
    TcbLowered,
    #[error("local TCB is incompatible with the runtime floor comparison domain")]
    TcbIncompatible,
    #[error("saved runtime sealing state has an invalid version, TEE, or comparison domain")]
    InvalidSavedSealingState,
    #[error("failed to freshly derive hardware sealing keys")]
    Derive(#[source] HardwareDerivedKeysError),
    #[error("failed to unseal the hardware protector")]
    Unseal(#[source] HardwareKeySealingError),
    #[error("failed to seal the active DEK")]
    Seal(#[source] HardwareKeySealingError),
}

/// Runtime sealing initialized from trusted local boot reports or authenticated,
/// VM-bound, fresh servicing state. Only SNP retains a historical TCB floor.
///
/// Retain the same state across attempts and errors. The caller must hold its
/// shared lock across each complete operation so accepted SNP increases survive
/// later failures. Never initialize state from VMGS or a host notification.
#[derive(Debug)]
pub struct RuntimeSealingState {
    policy: TcbPolicy,
}

#[derive(Debug)]
enum TcbPolicy {
    Snp(SnpTcbFloor),
    Tdx,
}

/// Collect trusted boot observations across unlock retries, without extra I/O.
/// Invalid observations permanently disable export for this boot. The caller
/// must additionally require durable hardware sealing of the active DEK.
pub(crate) struct BootRuntimeSealing {
    enabled: bool,
    state: Option<RuntimeSealingState>,
    invalid: bool,
}

impl BootRuntimeSealing {
    pub(crate) fn new(tee: Option<&dyn TeeCall>, config: &AttestationVmConfig) -> Self {
        Self {
            enabled: tee.is_some_and(|tee| sealing_context(tee, config).is_ok()),
            state: None,
            invalid: false,
        }
    }

    /// Pass only an existing trusted local report, before fallible host callouts
    /// or key unwrap. Collection never changes boot-unlock behavior.
    pub(crate) fn observe(&mut self, tee: &dyn TeeCall, report: &GetAttestationReportResult) {
        if !self.enabled || self.invalid {
            return;
        }
        let observed = RuntimeSealingState::from_report(tee, report).and_then(|(observed, _)| {
            if let Some(state) = &self.state {
                state.check_successor(&observed)?;
            }
            Ok(observed)
        });
        match observed {
            Ok(state) => self.state = Some(state),
            Err(err) => {
                self.invalid = true;
                tracelimit::warn_ratelimited!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "Boot report cannot establish runtime sealing state; runtime hardware resealing disabled"
                );
            }
        }
    }

    pub(crate) fn finish(self) -> Option<RuntimeSealingState> {
        if self.enabled && !self.invalid {
            self.state
        } else {
            None
        }
    }
}

impl RuntimeSealingState {
    fn from_report(
        tee: &dyn TeeCall,
        report: &GetAttestationReportResult,
    ) -> Result<(Self, KeyDerivationSvn), Error> {
        let svn = report_svn(tee, report)?;
        let policy = match svn {
            KeyDerivationSvn::Snp { tcb_version } => {
                TcbPolicy::Snp(SnpTcbFloor::from_report(&report.report, tcb_version)?)
            }
            KeyDerivationSvn::Tdx { .. } => TcbPolicy::Tdx,
        };
        Ok((Self { policy }, svn))
    }

    /// Save only SNP floor metadata or the initialized TDX marker. Transport
    /// must provide authentication, VM binding, and freshness.
    pub fn save(&self) -> SavedRuntimeSealingState {
        let tee = match &self.policy {
            TcbPolicy::Snp(floor) => SavedRuntimeSealingTee::Snp {
                tcb_version: floor.tcb_version,
                report_version: floor.domain.version,
                cpuid: floor.domain.cpuid.map(|cpuid| cpuid.to_vec()),
            },
            TcbPolicy::Tdx => SavedRuntimeSealingTee::Tdx,
        };
        SavedRuntimeSealingState {
            version: 1,
            tee: Some(tee),
        }
    }

    /// Restore trusted servicing state without hardware I/O or bootstrapping a
    /// new floor. The DTO itself provides no authentication or freshness.
    /// Before replacing resident state, call [`Self::check_successor`]
    /// under the shared lock. Neither failure may alter the resident state.
    pub fn restore(
        state: SavedRuntimeSealingState,
        tee: &dyn TeeCall,
        config: &AttestationVmConfig,
    ) -> Result<Self, Error> {
        sealing_context(tee, config)?;
        let invalid = || Error(ErrorInner::InvalidSavedSealingState);
        if state.version != 1 {
            return Err(invalid());
        }
        let policy = match (state.tee.ok_or_else(invalid)?, tee.tee_type()) {
            (
                SavedRuntimeSealingTee::Snp {
                    tcb_version,
                    report_version,
                    cpuid,
                },
                TeeType::Snp,
            ) => {
                let cpuid = match (report_version >= 3, cpuid) {
                    (false, None) => None,
                    (true, Some(cpuid)) => Some(cpuid.try_into().map_err(|_| invalid())?),
                    _ => return Err(invalid()),
                };
                TcbPolicy::Snp(SnpTcbFloor {
                    tcb_version,
                    domain: SnpDomain {
                        version: report_version,
                        cpuid,
                    },
                })
            }
            (SavedRuntimeSealingTee::Tdx, TeeType::Tdx) => TcbPolicy::Tdx,
            _ => return Err(invalid()),
        };
        Ok(Self { policy })
    }

    /// Pure same-TEE check, also enforcing SNP floor/domain continuity.
    pub fn check_successor(&self, restored: &Self) -> Result<(), Error> {
        match (&self.policy, &restored.policy) {
            (TcbPolicy::Snp(floor), TcbPolicy::Snp(next)) => floor.check_successor(next),
            (TcbPolicy::Tdx, TcbPolicy::Tdx) => Ok(()),
            _ => Err(Error(ErrorInner::TcbIncompatible)),
        }
    }

    /// Obtain the operation's SVN and commit any SNP increase before fallible
    /// derivation, candidate parsing, or persistence. TDX retains no SVN.
    fn observe_svn(&mut self, tee: &dyn TeeCall) -> Result<KeyDerivationSvn, Error> {
        let report = local_report(tee)?;
        let (observed, svn) = Self::from_report(tee, &report)?;
        self.check_successor(&observed)?;
        *self = observed;
        Ok(svn)
    }

    /// Seal the active DEK using a fresh local report, enforcing SNP's floor.
    /// Derivation/sealing failures cannot undo an accepted SNP increase.
    pub fn create_protector(
        &mut self,
        tee: &dyn TeeCall,
        config: &AttestationVmConfig,
        dek: &[u8; 32],
    ) -> Result<Vec<u8>, Error> {
        let (hardware, mix_measurement) = sealing_context(tee, config)?;
        let svn = self.observe_svn(tee)?;
        create_protector_with_svn(hardware, config, svn, mix_measurement, dek)
    }

    /// Check a fresh report and exact candidate SVN before independently
    /// deriving keys and unsealing. Mismatches and errors retain SNP increases.
    /// Report acquisition and derivation are not atomic with migration; callers
    /// still need pre-write/post-flush checks and retry handling.
    pub fn verify_protector(
        &mut self,
        tee: &dyn TeeCall,
        config: &AttestationVmConfig,
        protector: &[u8],
        dek: &[u8; 32],
    ) -> Result<bool, Error> {
        let (hardware, mix_measurement) = sealing_context(tee, config)?;
        let svn = self.observe_svn(tee)?;
        let Ok(candidate) = parse_hardware_key_protector(protector) else {
            return Ok(false);
        };
        let Some(policy) = validated_policy(&candidate) else {
            return Ok(false);
        };
        if policy.mix_measurement != mix_measurement || !svn_equal(policy.svn, svn) {
            return Ok(false);
        }
        unseal_matches(hardware, config, &candidate, policy, dek)
    }
}

fn local_report(tee: &dyn TeeCall) -> Result<GetAttestationReportResult, Error> {
    tee.get_attestation_report(&[0; REPORT_DATA_SIZE])
        .map_err(|err| Error(ErrorInner::Report(err)))
}

fn report_svn(
    tee: &dyn TeeCall,
    report: &GetAttestationReportResult,
) -> Result<KeyDerivationSvn, Error> {
    let svn = report
        .key_derivation_svn
        .ok_or(Error(ErrorInner::MissingKeyDerivationSvn))?;
    if !svn_matches_tee(svn, tee.tee_type()) {
        return Err(Error(ErrorInner::ReportSvnMismatch));
    }
    Ok(svn)
}

/// Validate the configured policy before making any hardware calls. In
/// particular, never turn disabled sealing or TDX signer sealing into Hash.
fn sealing_context<'a>(
    tee: &'a dyn TeeCall,
    config: &AttestationVmConfig,
) -> Result<(&'a dyn TeeCallGetDerivedKey, bool), Error> {
    let mix_measurement = match config.hardware_sealing_policy {
        HardwareSealingPolicy::None => return Err(Error(ErrorInner::DisabledPolicy)),
        HardwareSealingPolicy::Hash => true,
        HardwareSealingPolicy::Signer => false,
    };
    match tee.tee_type() {
        TeeType::Snp => {}
        TeeType::Tdx if mix_measurement => {}
        TeeType::Tdx => return Err(Error(ErrorInner::TdxSignerPolicyUnsupported)),
        _ => return Err(Error(ErrorInner::UnsupportedTee)),
    }
    let hardware = tee
        .supports_get_derived_key()
        .ok_or(Error(ErrorInner::UnsupportedTee))?;
    Ok((hardware, mix_measurement))
}

fn svn_matches_tee(svn: KeyDerivationSvn, tee_type: TeeType) -> bool {
    matches!(
        (svn, tee_type),
        (KeyDerivationSvn::Snp { .. }, TeeType::Snp) | (KeyDerivationSvn::Tdx { .. }, TeeType::Tdx)
    )
}

/// Reject unknown formats and noncanonical headers before interpreting SVN or
/// policy bytes. The shared byte parser has already checked the exact blob size.
fn validated_policy(protector: &HwKeyProtector) -> Option<KeyDerivationPolicy> {
    match protector {
        HwKeyProtector::Legacy(p) => {
            if p.header.version != vmgs::HW_KEY_PROTECTOR_VERSION_2
                || p.header.length as usize != vmgs::HW_KEY_PROTECTOR_SIZE
                || p.header.mix_measurement > 1
                || p.header._reserved != [0; 7]
            {
                return None;
            }
        }
        HwKeyProtector::V3(p) => {
            if p.header.version != vmgs::HW_KEY_PROTECTOR_VERSION_3
                || p.header.length as usize != vmgs::HW_KEY_PROTECTOR_V3_SIZE
                || p.header.mix_measurement > 1
                || p.header._reserved != [0; 3]
            {
                return None;
            }
            if p.header.tee_type == vmgs::HW_KEY_PROTECTOR_TEE_TYPE_SNP
                && p.header.svn[8..].iter().any(|&byte| byte != 0)
            {
                return None;
            }
        }
    }
    protector.key_derivation_policy()
}

/// Independently derive keys for a parsed, policy-checked candidate. No report
/// or parsing is repeated here, and seal-time keys are never reused.
fn unseal_matches(
    hardware: &dyn TeeCallGetDerivedKey,
    config: &AttestationVmConfig,
    protector: &HwKeyProtector,
    policy: KeyDerivationPolicy,
    dek: &[u8; 32],
) -> Result<bool, Error> {
    let keys = HardwareDerivedKeys::derive_key(hardware, config, policy)
        .map_err(|err| Error(ErrorInner::Derive(err)))?;
    let unsealed = match protector.unseal_key(&keys) {
        Ok(unsealed) => unsealed,
        Err(HardwareKeySealingError::HardwareKeyProtectorHmacVerificationFailed) => {
            return Ok(false);
        }
        Err(err) => return Err(Error(ErrorInner::Unseal(err))),
    };
    Ok(constant_time_eq::constant_time_eq_32(&unsealed, dek))
}

/// Seal with the already observed SVN, without fetching another report. The
/// runtime caller must have committed any SNP increase before entering here.
fn create_protector_with_svn(
    hardware: &dyn TeeCallGetDerivedKey,
    config: &AttestationVmConfig,
    svn: KeyDerivationSvn,
    mix_measurement: bool,
    dek: &[u8; 32],
) -> Result<Vec<u8>, Error> {
    let keys = HardwareDerivedKeys::derive_key(
        hardware,
        config,
        KeyDerivationPolicy {
            svn,
            mix_measurement,
        },
    )
    .map_err(|err| Error(ErrorInner::Derive(err)))?;
    let protector = seal_key(&keys, dek).map_err(|err| Error(ErrorInner::Seal(err)))?;
    Ok(protector.as_bytes().to_vec())
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod state_tests;

// SVN metadata is not secret key material.
fn svn_equal(left: KeyDerivationSvn, right: KeyDerivationSvn) -> bool {
    match (left, right) {
        (
            KeyDerivationSvn::Snp { tcb_version: left },
            KeyDerivationSvn::Snp { tcb_version: right },
        ) => left == right,
        (
            KeyDerivationSvn::Tdx {
                tee_tcb_svn: left_tee,
                cpu_svn: left_cpu,
            },
            KeyDerivationSvn::Tdx {
                tee_tcb_svn: right_tee,
                cpu_svn: right_cpu,
            },
        ) => left_tee == right_tee && left_cpu == right_cpu,
        _ => false,
    }
}
