// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conservative comparison of trusted local report snapshots, never VMGS data.
//!
//! # Reading the flow
//!
//! An **observation** is the SVN and comparison domain extracted from one trusted
//! local report. A **floor** is the last accepted observation: later runtime
//! operations must meet or exceed every ordered component of that floor.
//! **Ratchet** means replacing the floor with an accepted observation, never
//! rolling it back if subsequent sealing or persistence fails.
//!
//! There are two entry paths:
//! - **Boot:** `BootTcbFloor::observe` consumes an already acquired report (no
//!   hardware I/O). It initializes or advances the prospective floor. A bad
//!   observation permanently disables export for that boot, without failing boot
//!   unsealing. The caller also requires successful boot sealing before enrollment.
//! - **Runtime:** `observe_and_ratchet` calls `Snapshot::observe` to fetch a fresh
//!   report, then calls `check_successor` to validate it against the floor. Only
//!   after that check succeeds does it replace the floor and return the accepted
//!   SVN for sealing or candidate verification. A rejected observation leaves the
//!   existing floor intact so a later attempt can retry.
//!
//! `check_successor` is a pure compatibility/minimum check: it neither fetches a
//! report nor changes state. "Successor" includes an equal TCB, not only an
//! upgrade. It does not authenticate a protector or appraise vendor TCB status.
//!
//! SNP layouts follow `TCB_VERSION` and `ATTESTATION_REPORT` in the
//! [SEV-SNP Firmware ABI specification (AMD 56860)](https://docs.amd.com/v/u/en-US/56860_PUB_SEV_SNP).
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
use x86defs::snp::SNP_CPUID_FAMILY_MILAN_GENOA;
use x86defs::snp::SNP_CPUID_FAMILY_TURIN;
use x86defs::snp::SNP_CPUID_MODELS_MILAN_GENOA;
use x86defs::snp::SNP_CPUID_MODELS_TURIN_90_AF;
use x86defs::snp::SNP_CPUID_MODELS_TURIN_C0_CF;
use x86defs::snp::SnpReport;
use x86defs::snp::SnpTcbVersionLegacy;
use x86defs::snp::SnpTcbVersionTurin;
use x86defs::tdx::TeeTcbSvn;
use zerocopy::FromBytes;

// OpenHCL policy, not an AMD CPU-generation mapping: only report versions
// 3 through 5 currently support component ordering here. V2 lacks CPUID fields;
// v6 adds extended TCB semantics this floor does not model. Other versions
// remain equality-only, even when the CPU model has a known TCB layout.
const SNP_REPORT_VERSIONS_WITH_COMPONENT_ORDERING: core::ops::RangeInclusive<u32> = 3..=5;

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
/// 0x00..=0x1f) and Turin (family 0x1a, models 0x90..=0xaf and 0xc0..=0xcf)
/// use distinct layouts;
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
/// A valid collection alone does not authorize runtime enrollment: the caller
/// must also require successful sealing and persistence of the current active
/// DEK in the successful unlock attempt.
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

    /// Record an existing boot report; this method makes no hardware calls.
    ///
    /// The first valid observation initializes the prospective floor. Subsequent
    /// ones must pass `check_successor` before replacing it. A parsing or policy
    /// error latches `invalid`: later reports cannot re-enable export in this
    /// boot. Disabled or already-invalid collectors ignore further observations.
    /// This deliberately returns no error to the boot-unlock flow.
    ///
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

    /// Transfer the collected floor if at least one valid report was observed
    /// and collection was never invalidated. This alone does not prove sealing
    /// succeeded; the boot caller separately gates runtime enrollment on that.
    pub(crate) fn finish(self) -> Option<RuntimeTcbFloor> {
        if self.enabled && !self.invalid {
            self.floor
        } else {
            None
        }
    }
}

/// Compact metadata from one local report, not a persisted VMGS snapshot.
/// Used both for a new observation and for the currently accepted floor.
#[derive(Debug)]
struct Snapshot {
    svn: KeyDerivationSvn,
    snp_domain: Option<SnpDomain>,
}

/// The interpretation domain of an SNP report's raw TCB bytes.
///
/// SVN bytes alone cannot tell us which security components they describe.
/// AMD 56860 rev. 1.59, section 2.3, tables 4/5 define different TCB_VERSION
/// layouts for pre-Turin and Turin CPUs: e.g. byte 0 is bootloader on the
/// former but FMC on the latter. CPU family/model selects that layout, not
/// report version. The report version separately gates the semantics we support
/// (including CPUID availability); future reports can add unmodeled TCB fields.
/// See the [AMD firmware ABI specification].
///
/// Require exact domain equality before even accepting equal raw SVNs. This
/// avoids comparing unrelated components across CPUs or report formats. It is
/// deliberately stricter than merely having the same layout: no cross-domain
/// ordering is defined here. Unknown domains allow unchanged SVNs only; their
/// metadata must not be discarded or interpreted using a guessed layout.
///
/// [AMD firmware ABI specification]: https://docs.amd.com/v/u/en-US/56860_PUB_SEV_SNP
#[derive(Debug, PartialEq, Eq)]
struct SnpDomain {
    version: u32,
    // Unknown v3+ layouts retain these bytes only as opaque discriminators;
    // they never authorize component ordering. V2 does not have CPUID fields.
    cpuid: Option<[u8; 2]>,
}

#[derive(Debug, PartialEq, Eq)]
enum SnpTcbLayout {
    Legacy,
    Turin,
}

impl SnpDomain {
    /// Select a supported layout, separately from comparing its components.
    /// Family alone is insufficient: each family includes other CPU models.
    fn tcb_layout(&self) -> Option<SnpTcbLayout> {
        if !SNP_REPORT_VERSIONS_WITH_COMPONENT_ORDERING.contains(&self.version) {
            return None;
        }
        let [family, model] = self.cpuid?;
        match family {
            SNP_CPUID_FAMILY_MILAN_GENOA if SNP_CPUID_MODELS_MILAN_GENOA.contains(&model) => {
                Some(SnpTcbLayout::Legacy)
            }
            SNP_CPUID_FAMILY_TURIN
                if SNP_CPUID_MODELS_TURIN_90_AF.contains(&model)
                    || SNP_CPUID_MODELS_TURIN_C0_CF.contains(&model) =>
            {
                Some(SnpTcbLayout::Turin)
            }
            _ => None,
        }
    }
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

    /// Fetch, check, and commit a runtime observation, in that order.
    ///
    /// Report acquisition/parsing or `check_successor` failure leaves the floor
    /// unchanged. On success, return the SVN of the newly accepted floor. The
    /// update intentionally precedes derivation, protector verification, and
    /// disk I/O: none of those later failures may undo a trusted TCB increase.
    ///
    /// For example, accepting components (2, 4) after (2, 3) retains (2, 4)
    /// even if the ensuing write fails; a retry at (2, 3) must then be rejected.
    /// These are component vectors, not packed integers or lexicographic values.
    fn observe_and_ratchet(&mut self, tee: &dyn TeeCall) -> Result<KeyDerivationSvn, Error> {
        let observed = Snapshot::observe(tee)?;
        self.snapshot.check_successor(&observed)?;
        // Commit before any key derivation, crypto, or untrusted header parsing.
        self.snapshot = observed;
        Ok(self.snapshot.svn)
    }
}

impl Snapshot {
    /// Fetch one fresh local report and extract its SVN/comparison domain.
    /// Unlike `BootTcbFloor::observe`, this performs hardware I/O. It does not
    /// compare against or mutate a floor, derive keys, or inspect VMGS.
    fn observe(tee: &dyn TeeCall) -> Result<Self, Error> {
        let report = tee
            .get_attestation_report(&[0; REPORT_DATA_SIZE])
            .map_err(|err| Error(ErrorInner::Report(err)))?;
        Self::from_report(tee, &report)
    }

    /// Extract a snapshot without I/O or comparison against an earlier TCB.
    /// Check the SVN's TEE variant and, for SNP, the report layout and agreement
    /// between its raw reported TCB and the adapter's extracted SVN.
    ///
    /// Trust comes from the local hardware interface, not this parser: this is
    /// not signature verification of an arbitrary report. Never pass report
    /// bytes supplied by the host or a VMGS header.
    fn from_report(tee: &dyn TeeCall, report: &GetAttestationReportResult) -> Result<Self, Error> {
        let svn = report
            .key_derivation_svn
            .ok_or(Error(ErrorInner::MissingKeyDerivationSvn))?;
        if !svn_matches_tee(svn, tee.tee_type()) {
            return Err(Error(ErrorInner::ReportSvnMismatch));
        }
        let snp_domain = match svn {
            KeyDerivationSvn::Snp { tcb_version } => {
                // Read an owned value: the byte buffer need not satisfy the
                // report's alignment. Preserve acceptance of trailing bytes.
                let (snp_report, _) = SnpReport::read_from_prefix(&report.report)
                    .map_err(|_| Error(ErrorInner::MalformedReport))?;
                let version = snp_report.version;
                if snp_report.reported_tcb != tcb_version {
                    return Err(Error(ErrorInner::ReportTcbMismatch));
                }
                let cpuid = if version >= 3 {
                    Some([snp_report.cpuid_fam_id, snp_report.cpuid_mod_id])
                } else {
                    None
                };
                Some(SnpDomain { version, cpuid })
            }
            KeyDerivationSvn::Tdx { .. } => None,
        };
        Ok(Self { svn, snp_domain })
    }

    /// Can `observed` replace `self` as the minimum accepted runtime TCB?
    /// `self` is the existing floor; `observed` is a candidate local observation.
    /// Neither argument is modified, and this method performs no hardware I/O.
    ///
    /// - Require the same SNP comparison domain (report version/family/model).
    /// - Within a compatible domain, identical SVN values are accepted, even
    ///   when there is no supported component-ordering rule.
    /// - For changed SNP SVNs, require a supported layout, equal reserved bytes,
    ///   and non-decreasing named components.
    /// - For TDX, require the same module identity and reserved bytes, with
    ///   non-decreasing minor SVN, SE_SVN, and CPU SVN components. Different
    ///   TEE types are incompatible.
    ///
    /// A higher component cannot compensate for a lower one. Known component
    /// regressions fail with `TcbLowered`; incompatible domains, identities,
    /// reserved bytes, or unsupported ordering fail with `TcbIncompatible`.
    /// Acceptance is only a minimum-TCB decision, not proof that a candidate
    /// protector can be unsealed or that the TCB has a particular vendor status.
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
                // AMD 56860 rev. 1.59 section 2.3, tables 4 and 5. The
                // report version gates supported semantics; family/model
                // chooses the encoding. Preserve raw SVN for key derivation.
                let floor = floor.to_le_bytes();
                let next = next.to_le_bytes();
                let layout = domain
                    .tcb_layout()
                    .ok_or(Error(ErrorInner::TcbIncompatible))?;
                let meets_floor = match layout {
                    SnpTcbLayout::Legacy => {
                        let floor = SnpTcbVersionLegacy::read_from_bytes(&floor)
                            .map_err(|_| Error(ErrorInner::MalformedReport))?;
                        let next = SnpTcbVersionLegacy::read_from_bytes(&next)
                            .map_err(|_| Error(ErrorInner::MalformedReport))?;
                        if next.reserved != floor.reserved {
                            return Err(Error(ErrorInner::TcbIncompatible));
                        }
                        next.bootloader >= floor.bootloader
                            && next.tee >= floor.tee
                            && next.snp >= floor.snp
                            && next.microcode >= floor.microcode
                    }
                    SnpTcbLayout::Turin => {
                        let floor = SnpTcbVersionTurin::read_from_bytes(&floor)
                            .map_err(|_| Error(ErrorInner::MalformedReport))?;
                        let next = SnpTcbVersionTurin::read_from_bytes(&next)
                            .map_err(|_| Error(ErrorInner::MalformedReport))?;
                        if next.reserved != floor.reserved {
                            return Err(Error(ErrorInner::TcbIncompatible));
                        }
                        next.fmc >= floor.fmc
                            && next.bootloader >= floor.bootloader
                            && next.tee >= floor.tee
                            && next.snp >= floor.snp
                            && next.microcode >= floor.microcode
                    }
                };
                if !meets_floor {
                    return Err(Error(ErrorInner::TcbLowered));
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
                // Parse the ABI layout without changing the raw derivation SVN.
                let floor_tee = TeeTcbSvn::read_from_bytes(&floor_tee)
                    .map_err(|_| Error(ErrorInner::MalformedReport))?;
                let next_tee = TeeTcbSvn::read_from_bytes(&next_tee)
                    .map_err(|_| Error(ErrorInner::MalformedReport))?;
                // Major SVN selects the module identity, not an ordered SVN.
                // This rejects both legacy/TD-preserving transitions and
                // transitions between distinct TD-preserving identities. A
                // change in reserved metadata has no supported ordering.
                if next_tee.tdx_module_svn_major != floor_tee.tdx_module_svn_major
                    || next_tee._reserved != floor_tee._reserved
                {
                    return Err(Error(ErrorInner::TcbIncompatible));
                }
                // CPU SVN is kept as 16 bytes: apply the local per-position
                // floor rule documented on components_meet, not Rust array Ord
                // (lexicographic) or a packed-integer comparison. TEE SVN has
                // its own named-field rules and must not use that helper.
                if !components_meet(&next_cpu, &floor_cpu)
                    || next_tee.tdx_module_svn_minor < floor_tee.tdx_module_svn_minor
                    || next_tee.seam_last_patch_svn < floor_tee.seam_last_patch_svn
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

/// Inclusive component-wise minimum: every unsigned byte must meet the same
/// position in the floor. For CPU SVN, N is 16; there is no endian conversion,
/// sorting, or carry between components. With other bytes equal, (2, 4) meets
/// (2, 3), but (3, 2) does not: growth cannot compensate for a regression.
///
/// Official context: Intel PCS [Get TDX TCB Info V4], step 3.a, requires all
/// 16 PCK certificate TCB component SVNs to meet their corresponding TCB Info
/// values; [Appendix A] describes the 16 components and `tcbType` comparison
/// metadata. This motivates component-wise rather than scalar ordering.
///
/// This function applies OpenHCL's local no-decrease policy to two raw CPU SVN
/// observations. It does NOT implement PCS appraisal, map raw bytes to PCK
/// component identities, or establish equivalence across platform families.
/// Do not infer `UpToDate`/revocation status from it: that requires authenticated
/// platform-specific collateral, including FMSPC and TCB Info.
///
/// [Get TDX TCB Info V4]: https://api.portal.trustedservices.intel.com/content/documentation.html#pcs-tcb-info-tdx-v4
/// [Appendix A]: https://api.portal.trustedservices.intel.com/content/documentation.html#pcs-tcb-info-model-v3
fn components_meet<const N: usize>(actual: &[u8; N], minimum: &[u8; N]) -> bool {
    actual
        .iter()
        .zip(minimum)
        .all(|(actual, minimum)| actual >= minimum)
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
