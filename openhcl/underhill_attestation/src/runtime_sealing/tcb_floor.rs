// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conservative comparison of trusted local report snapshots, never VMGS data.
//!
//! SNP layout references: AMD ABI 56860, TCB_VERSION and SNP attestation report;
//! [VirTEE report offsets](https://docs.rs/sev/8.0.0/src/sev/firmware/guest/types/snp.rs.html)
//! and [TCB component order](https://docs.rs/sev/8.0.0/src/sev/firmware/host/types/snp.rs.html).
//! Raw packed integer ordering is not a component-wise security ordering, and
//! Turin moves components relative to Milan/Genoa.

use super::Error;
use super::ErrorInner;
use super::create_protector_with_svn;
use super::protector_matches;
use super::sealing_context;
use super::svn_matches_tee;
use super::validated_policy;
use crate::vmgs::parse_hardware_key_protector;
use cvm_tracing::CVM_ALLOWED;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use tee_call::GetAttestationReportResult;
use tee_call::KeyDerivationSvn;
use tee_call::REPORT_DATA_SIZE;
use tee_call::TeeCall;

const SNP_REPORT_SIZE: usize = 1184;
const SNP_REPORTED_TCB_OFFSET: usize = 0x180;
const SNP_CPUID_OFFSET: usize = 0x188;

/// A resident, runtime-only TCB floor derived exclusively from local hardware.
///
/// Obtain once from platform initialization (reusing boot reports), or use
/// [`Self::new`] **before accepting worker events**, then
/// retain the same instance across attempts and errors. Blocking hardware jobs
/// must hold the caller's shared mutex for each complete method call. Never
/// recreate the floor on retry or initialize it from a VMGS protector header.
/// This is not a persisted anti-rollback counter and does not protect a new
/// runtime from rollback across restart. It deliberately has no `Clone`, `Copy`,
/// `Default`, deserializer, or public constructor from bytes or an SVN.
///
/// SNP ordering is component-wise only for report versions 3, 4, and 5 within
/// the exact same version/family/model. Milan/Genoa (family 0x19, models
/// 0x00..=0x1f) and Turin (family 0x1a, models 0x00..=0x0f) use distinct layouts;
/// reserved TCB bytes must remain equal. All other SNP domains require exact
/// SVN equality. Version 2 has no trusted CPUID and is also equality-only.
/// TDX uses a structural minimum-TCB policy: CPU SVN bytes must individually
/// meet the floor. TEE SVN byte 1 must match, including legacy identity 0.
/// For TD-preserving modules byte 0 is module ISVSVN and bytes 2..16 are platform
/// components; these bytes must individually meet the floor in both layouts.
/// This is not quote appraisal
/// or an Intel TCB status decision, which requires authenticated TCB Info.
#[derive(Debug)]
pub struct RuntimeTcbFloor {
    snapshot: Snapshot,
}

/// Collect a runtime floor from reports already obtained during boot. No I/O.
///
/// Keep one collector across all unlock retries, including failed SKR attempts.
/// Only comparable, nondecreasing observations ratchet the accepted floor.
/// ANY malformed, incompatible, or lowered observation permanently disables
/// export for this boot, even if a later report is good. This is deliberately
/// stricter than runtime retry handling: boot must never export an ambiguous
/// initial floor or silently bootstrap later after a worker event.
///
/// Collection never affects boot unlocking, including cached lower-SVN unseal
/// policies, SKR fallback, and retry/skip-hardware-unsealing decisions.
pub(crate) struct BootTcbFloor {
    enabled: bool,
    floor: Option<RuntimeTcbFloor>,
    invalid: bool,
}

impl BootTcbFloor {
    pub(crate) fn new(tee: Option<&dyn TeeCall>, config: &AttestationVmConfig) -> Self {
        Self {
            enabled: tee.is_some_and(|tee| sealing_context(tee, config).is_ok()),
            floor: None,
            invalid: false,
        }
    }

    /// Only pass the result directly from the trusted local `TeeCall`, before
    /// host callouts or key unwrap can fail. Never pass host or VMGS report bytes.
    pub(crate) fn observe(&mut self, tee: &dyn TeeCall, report: &GetAttestationReportResult) {
        if !self.enabled || self.invalid {
            return;
        }
        let observed = Snapshot::from_report(tee, report).and_then(|observed| {
            if let Some(floor) = &self.floor {
                floor.snapshot.check_successor(&observed)?;
            }
            Ok(observed)
        });
        match observed {
            Ok(snapshot) => self.floor = Some(RuntimeTcbFloor { snapshot }),
            Err(err) => {
                self.invalid = true;
                tracelimit::warn_ratelimited!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "Boot report cannot establish runtime TCB floor; runtime hardware resealing disabled"
                );
            }
        }
    }

    pub(crate) fn finish(self) -> Option<RuntimeTcbFloor> {
        if self.enabled && !self.invalid {
            self.floor
        } else {
            None
        }
    }
}

#[derive(Debug)]
struct Snapshot {
    svn: KeyDerivationSvn,
    snp_domain: Option<SnpDomain>,
}

#[derive(Debug, PartialEq, Eq)]
struct SnpDomain {
    version: u32,
    // Unknown v3+ layouts retain these bytes only as opaque discriminators;
    // they never authorize component ordering. V2 does not have CPUID fields.
    cpuid: Option<[u8; 2]>,
}

impl RuntimeTcbFloor {
    /// Initialize from a fresh trusted local report, without deriving any keys.
    /// The `tee` must be the trusted local hardware interface, not report bytes
    /// supplied by the root or by a persisted protector.
    pub fn new(tee: &dyn TeeCall, config: &AttestationVmConfig) -> Result<Self, Error> {
        sealing_context(tee, config)?;
        Ok(Self {
            snapshot: Snapshot::observe(tee)?,
        })
    }

    /// Validate and ratchet from a fresh report, then seal the active DEK using
    /// that same report's SVN. Derivation/sealing errors cannot undo the ratchet.
    /// The caller performs candidate pre-write verification separately.
    pub fn create_protector(
        &mut self,
        tee: &dyn TeeCall,
        config: &AttestationVmConfig,
        dek: &[u8; 32],
    ) -> Result<Vec<u8>, Error> {
        let (hardware, mix_measurement) = sealing_context(tee, config)?;
        let svn = self.observe_and_ratchet(tee)?;
        create_protector_with_svn(hardware, config, svn, mix_measurement, dek)
    }

    /// Validate and ratchet from a fresh report before inspecting the candidate.
    /// Return `Ok(false)` if its header SVN differs from the observed SVN, even
    /// if hardware could still derive a key for that older candidate. Otherwise
    /// freshly derive hardware keys and authenticate/unseal via the existing
    /// stateless verifier. Neither mismatches nor errors undo a ratchet.
    ///
    /// This does not make the report and derivation atomic with migration; the
    /// caller still needs pre-write and post-flush verification and retry logic.
    pub fn verify_protector(
        &mut self,
        tee: &dyn TeeCall,
        config: &AttestationVmConfig,
        protector: &[u8],
        dek: &[u8; 32],
    ) -> Result<bool, Error> {
        sealing_context(tee, config)?;
        let svn = self.observe_and_ratchet(tee)?;
        let Ok(candidate) = parse_hardware_key_protector(protector) else {
            return Ok(false);
        };
        let Some(policy) = validated_policy(&candidate) else {
            return Ok(false);
        };
        if !svn_equal(policy.svn, svn) {
            return Ok(false);
        }
        protector_matches(tee, config, protector, dek)
    }

    fn observe_and_ratchet(&mut self, tee: &dyn TeeCall) -> Result<KeyDerivationSvn, Error> {
        let observed = Snapshot::observe(tee)?;
        self.snapshot.check_successor(&observed)?;
        // Commit before any key derivation, crypto, or untrusted header parsing.
        self.snapshot = observed;
        Ok(self.snapshot.svn)
    }
}

impl Snapshot {
    fn observe(tee: &dyn TeeCall) -> Result<Self, Error> {
        let report = tee
            .get_attestation_report(&[0; REPORT_DATA_SIZE])
            .map_err(|err| Error(ErrorInner::Report(err)))?;
        Self::from_report(tee, &report)
    }

    // This parser is private: the result must originate from trusted local
    // hardware, not attestation bytes supplied by the host or a VMGS header.
    fn from_report(tee: &dyn TeeCall, report: &GetAttestationReportResult) -> Result<Self, Error> {
        let svn = report
            .key_derivation_svn
            .ok_or(Error(ErrorInner::MissingKeyDerivationSvn))?;
        if !svn_matches_tee(svn, tee.tee_type()) {
            return Err(Error(ErrorInner::ReportSvnMismatch));
        }
        let snp_domain = match svn {
            KeyDerivationSvn::Snp { tcb_version } => {
                if report.report.len() < SNP_REPORT_SIZE {
                    return Err(Error(ErrorInner::MalformedReport));
                }
                let version = u32::from_le_bytes(report_field(&report.report, 0)?);
                let reported_tcb =
                    u64::from_le_bytes(report_field(&report.report, SNP_REPORTED_TCB_OFFSET)?);
                if reported_tcb != tcb_version {
                    return Err(Error(ErrorInner::ReportTcbMismatch));
                }
                let cpuid = if version >= 3 {
                    Some(report_field(&report.report, SNP_CPUID_OFFSET)?)
                } else {
                    None
                };
                Some(SnpDomain { version, cpuid })
            }
            KeyDerivationSvn::Tdx { .. } => None,
        };
        Ok(Self { svn, snp_domain })
    }

    fn check_successor(&self, observed: &Self) -> Result<(), Error> {
        if self.snp_domain != observed.snp_domain {
            return Err(Error(ErrorInner::TcbIncompatible));
        }
        if svn_equal(self.svn, observed.svn) {
            return Ok(());
        }
        match (self.svn, observed.svn, self.snp_domain.as_ref()) {
            (
                KeyDerivationSvn::Snp { tcb_version: floor },
                KeyDerivationSvn::Snp { tcb_version: next },
                Some(domain),
            ) => {
                // true means an ordered component; false means reserved and
                // therefore equality-only. No unknown layout gets a mask.
                let components = match (domain.version, domain.cpuid) {
                    (3..=5, Some([0x19, 0x00..=0x1f])) => {
                        [true, true, false, false, false, false, true, true]
                    }
                    (3..=5, Some([0x1a, 0x00..=0x0f])) => {
                        [true, true, true, true, false, false, false, true]
                    }
                    _ => return Err(Error(ErrorInner::TcbIncompatible)),
                };
                for ((floor, next), component) in floor
                    .to_le_bytes()
                    .into_iter()
                    .zip(next.to_le_bytes())
                    .zip(components)
                {
                    if component && next < floor {
                        return Err(Error(ErrorInner::TcbLowered));
                    }
                    if !component && next != floor {
                        return Err(Error(ErrorInner::TcbIncompatible));
                    }
                }
                Ok(())
            }
            (
                KeyDerivationSvn::Tdx {
                    tee_tcb_svn: floor_tee,
                    cpu_svn: floor_cpu,
                },
                KeyDerivationSvn::Tdx {
                    tee_tcb_svn: next_tee,
                    cpu_svn: next_cpu,
                },
                None,
            ) => {
                // Byte 1 selects the module identity, not an ordered SVN.
                // This rejects both legacy/TD-preserving transitions and
                // transitions between distinct TD-preserving identities.
                if next_tee[1] != floor_tee[1] {
                    return Err(Error(ErrorInner::TcbIncompatible));
                }
                // With the identity equal (including 0 for legacy modules),
                // comparing all remaining bytes implements both layouts.
                if !components_meet(&next_cpu, &floor_cpu)
                    || !components_meet(&next_tee, &floor_tee)
                {
                    return Err(Error(ErrorInner::TcbLowered));
                }
                Ok(())
            }
            // Mixed TEE snapshots have no ordering contract.
            _ => Err(Error(ErrorInner::TcbIncompatible)),
        }
    }
}

fn components_meet<const N: usize>(actual: &[u8; N], minimum: &[u8; N]) -> bool {
    actual
        .iter()
        .zip(minimum)
        .all(|(actual, minimum)| actual >= minimum)
}

fn report_field<const N: usize>(report: &[u8], offset: usize) -> Result<[u8; N], Error> {
    report
        .get(offset..)
        .and_then(|tail| tail.get(..N))
        .and_then(|field| field.try_into().ok())
        .ok_or(Error(ErrorInner::MalformedReport))
}

// Keep equality local rather than imposing a public ordering/equality contract
// on TeeCall's types. These values are SVN metadata, not secret key material.
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

#[cfg(test)]
mod tests;
