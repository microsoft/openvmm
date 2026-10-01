// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checking a host fingerprint against the profile of its generation, the
//! CPU part of host qualification.

use crate::catalog;
use crate::error::ProfileError;
use crate::fingerprint::CpuFingerprint;
use crate::profile::CpuProfile;
use crate::signature::HostCpuSignature;
use crate::surface::HostCpuSurface;
use crate::surface::verify_support;

/// The prefix of the summary line of a fingerprint check.
pub const SUMMARY_PREFIX: &str = "NVX-CPU-PROFILE:";

/// The outcome of checking a fingerprint against the profile that `auto`
/// selects for its host.
#[derive(Debug)]
pub struct FingerprintCheck {
    /// The host CPU.
    pub host: HostCpuSignature,
    /// The profile that `auto` selects, if any.
    pub profile: Option<&'static CpuProfile>,
    /// Whether the backend supports the profile, or why not.
    pub result: Result<(), ProfileError>,
}

impl FingerprintCheck {
    /// Returns the one-line summary of the check, for logs and CI:
    ///
    /// ```text
    /// NVX-CPU-PROFILE: status=pass backend=kvm generation=icelake-sp profile=intel.icelake-sp.v1 profile_digest=sha256:… surface_digest=sha256:… host_invariant_tsc=yes
    /// NVX-CPU-PROFILE: status=fail backend=whp generation=none profile=none surface_digest=sha256:… host_invariant_tsc=yes code=E_PROFILE_HOST_UNKNOWN detail="…"
    /// ```
    ///
    /// `host_invariant_tsc` reports the host OS's view, which host
    /// qualification evaluates; it does not affect `status`.
    pub fn summary_line(&self, fingerprint: &CpuFingerprint) -> String {
        let mut line = format!(
            "{SUMMARY_PREFIX} status={} backend={} generation={} profile={}",
            if self.result.is_ok() { "pass" } else { "fail" },
            fingerprint.backend.name,
            self.profile
                .map_or("none", |profile| profile.generation().name.as_str()),
            self.profile.map_or("none", |profile| profile.id()),
        );
        if let Some(profile) = self.profile {
            line.push_str(&format!(" profile_digest={}", profile.digest_string()));
        }
        line.push_str(&format!(
            " surface_digest={} host_invariant_tsc={}",
            fingerprint.surface_digest,
            if fingerprint.host.cpu.invariant_tsc {
                "yes"
            } else {
                "no"
            }
        ));
        if let Err(error) = &self.result {
            line.push_str(&format!(" code={} detail={:?}", error.code, error.message));
        }
        line
    }
}

/// Checks a host fingerprint: selects the profile of the host's generation
/// (`E_PROFILE_HOST_UNKNOWN`) and verifies that the backend supports it
/// (`E_PROFILE_UNSUPPORTED`).
pub fn check_fingerprint(fingerprint: &CpuFingerprint) -> FingerprintCheck {
    let cpu = &fingerprint.host.cpu;
    let mut vendor = [0; 12];
    if cpu.vendor.len() == vendor.len() {
        vendor.copy_from_slice(cpu.vendor.as_bytes());
    }
    let host = HostCpuSignature::new(vendor, cpu.signature.0);
    match catalog::select_auto(&host) {
        Ok(profile) => FingerprintCheck {
            host,
            profile: Some(profile),
            result: verify_support(
                profile,
                &HostCpuSurface::from_fingerprint(&fingerprint.backend),
            ),
        },
        Err(error) => FingerprintCheck {
            host,
            profile: None,
            result: Err(error),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hex32;
    use crate::ProfileErrorCode;
    use crate::cpuid::CpuidEntry;
    use crate::test_support::fingerprint;
    use crate::test_support::fingerprint_with;
    use crate::test_support::profile;
    use crate::test_support::profile_entries;
    use test_with_tracing::test;

    #[test]
    fn passes_a_host_that_supports_its_generation() {
        let profile = profile("intel.icelake-sp.v1");
        let fingerprint = fingerprint(profile, "kvm");
        let check = check_fingerprint(&fingerprint);
        check.result.as_ref().unwrap();
        assert_eq!(
            check.profile.map(CpuProfile::id),
            Some("intel.icelake-sp.v1")
        );
        assert_eq!(
            check.summary_line(&fingerprint),
            format!(
                "NVX-CPU-PROFILE: status=pass backend=kvm generation=icelake-sp \
                 profile=intel.icelake-sp.v1 profile_digest={} surface_digest={} \
                 host_invariant_tsc=yes",
                profile.digest_string(),
                fingerprint.surface_digest
            )
        );
    }

    #[test]
    fn fails_unknown_hosts_and_unsupported_profiles() {
        let profile = profile("intel.icelake-sp.v1");
        let mut alder_lake = fingerprint(profile, "whp");
        alder_lake.host.cpu.signature = Hex32(0x0009_06a3);
        alder_lake.host.cpu.invariant_tsc = false;
        let check = check_fingerprint(&alder_lake);
        assert_eq!(
            check.result.as_ref().unwrap_err().code,
            ProfileErrorCode::ProfileHostUnknown
        );
        let line = check.summary_line(&alder_lake);
        assert!(
            line.starts_with("NVX-CPU-PROFILE: status=fail backend=whp generation=none profile=none surface_digest="),
            "{line}"
        );
        assert!(
            line.contains(
                " host_invariant_tsc=no code=E_PROFILE_HOST_UNKNOWN detail=\"no pinned CPU profile"
            ),
            "{line}"
        );

        let mut entries = profile_entries(profile);
        let entry = entries
            .iter_mut()
            .find(|entry| entry.key() == (7, Some(0)))
            .unwrap();
        let [eax, ebx, ecx, edx] = entry.registers();
        *entry = CpuidEntry::new(7, Some(0), [eax, ebx & !(1 << 16), ecx, edx]);
        let no_avx512 = fingerprint_with(profile, "kvm", entries);
        let check = check_fingerprint(&no_avx512);
        assert_eq!(
            check.result.as_ref().unwrap_err().code,
            ProfileErrorCode::ProfileUnsupported
        );
        assert!(
            check
                .summary_line(&no_avx512)
                .contains("code=E_PROFILE_UNSUPPORTED detail=\"the backend does not support CPU profile intel.icelake-sp.v1: CPUID 0x7.0 EBX bit 16 is not supported\""),
            "{}",
            check.summary_line(&no_avx512)
        );
    }
}
