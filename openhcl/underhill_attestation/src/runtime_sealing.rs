// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local hardware protector verification and recreation for runtime recovery.
//!
//! These operations neither access VMGS nor make GET/SKR callouts. The caller
//! supplies the active DEK and is responsible for persisting a replacement
//! protector. Hardware-derived keys are freshly obtained on every operation;
//! they must not be cached across migration, even when the SVN is unchanged.

use crate::hardware_key_sealing::HardwareDerivedKeys;
use crate::hardware_key_sealing::HardwareDerivedKeysError;
use crate::hardware_key_sealing::HardwareKeySealingError;
use crate::hardware_key_sealing::HwKeyProtector;
use crate::hardware_key_sealing::seal_key;
use crate::vmgs::parse_hardware_key_protector;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::HardwareSealingPolicy;
use openhcl_attestation_protocol::vmgs;
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
    #[error("failed to freshly derive hardware sealing keys")]
    Derive(#[source] HardwareDerivedKeysError),
    #[error("failed to unseal the hardware protector")]
    Unseal(#[source] HardwareKeySealingError),
    #[error("failed to seal the active DEK")]
    Seal(#[source] HardwareKeySealingError),
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

/// Check whether the stored protector authenticates and unseals to the active
/// DEK on the current hardware, using its **stored** policy and SVN.
///
/// Returns `Ok(false)` for malformed, incompatible, policy-mismatched, or
/// unauthenticated protectors, and for a different DEK. Hardware derivation and
/// crypto operational failures return `Err`, not a mismatch. An unavailable
/// source SVN is a derivation error: the caller may explicitly retry by creating
/// a protector with the destination's current SVN. No derived keys are cached.
pub fn protector_matches(
    tee: &dyn TeeCall,
    config: &AttestationVmConfig,
    protector_bytes: &[u8],
    dek: &[u8; 32],
) -> Result<bool, Error> {
    let (hardware, mix_measurement) = sealing_context(tee, config)?;
    let Ok(protector) = parse_hardware_key_protector(protector_bytes) else {
        return Ok(false);
    };
    let Some(policy) = validated_policy(&protector) else {
        return Ok(false);
    };
    if policy.mix_measurement != mix_measurement || !svn_matches_tee(policy.svn, tee.tee_type()) {
        return Ok(false);
    }

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

/// Seal the unchanged active DEK into a new v3 protector using a fresh local
/// report's SVN and the configured policy. This does not rotate the DEK.
///
/// Disabled sealing, unsupported TEEs, TDX signer policy, missing or incompatible
/// report SVN, and report/derivation/sealing failures return `Err`. No fallback
/// policy or remote key release is attempted.
pub fn create_protector(
    tee: &dyn TeeCall,
    config: &AttestationVmConfig,
    dek: &[u8; 32],
) -> Result<Vec<u8>, Error> {
    let (hardware, mix_measurement) = sealing_context(tee, config)?;
    let report = tee
        .get_attestation_report(&[0; REPORT_DATA_SIZE])
        .map_err(|err| Error(ErrorInner::Report(err)))?;
    let svn = report
        .key_derivation_svn
        .ok_or(Error(ErrorInner::MissingKeyDerivationSvn))?;
    if !svn_matches_tee(svn, tee.tee_type()) {
        return Err(Error(ErrorInner::ReportSvnMismatch));
    }
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
mod tests;
