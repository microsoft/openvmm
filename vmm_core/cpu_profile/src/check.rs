// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checking a host fingerprint against the profile of its generation, the
//! CPU part of host qualification.

use crate::catalog;
use crate::cpuid::CpuidEntry;
use crate::error::ProfileError;
use crate::fingerprint::CpuFingerprint;
use crate::profile::CpuProfile;
use crate::signature::HostCpuSignature;
use crate::surface::CpuidPresentation;
use crate::surface::HostCpuSurface;
use crate::surface::support_violations;
use crate::surface::verify_support;
use crate::surface::verify_support_with_unlisted;

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
/// (`E_PROFILE_UNSUPPORTED`) and, for MSHV and WHP, whose fingerprints record
/// a probe partition's view, that it presents zero at every CPUID entry
/// outside the profile's tables (`E_CPU_UNLISTED`).
pub fn check_fingerprint(fingerprint: &CpuFingerprint) -> FingerprintCheck {
    check_fingerprint_with(fingerprint, |_| Ok(None))
}

/// Checks a host fingerprint as [`check_fingerprint`] does, but for MSHV and
/// WHP takes the CPUID entries outside the selected profile's tables from
/// `configured`, as a cold boot checks them: `configured` returns what VP 0
/// of a partition configured from the profile reads at the host's
/// [`unlisted_cpuid_candidates`](crate::unlisted_cpuid_candidates), or
/// `None` to check the fingerprint's own probe partition instead.
///
/// The fingerprint's probe partition enables every feature the backend
/// offers, so it can present entries that a partition of the profile does
/// not, such as the XSAVE components of CET. `configured` runs only when the
/// backend supports the profile otherwise; its failure fails the check.
pub fn check_fingerprint_with(
    fingerprint: &CpuFingerprint,
    configured: impl FnOnce(&CpuProfile) -> Result<Option<Vec<CpuidEntry>>, ProfileError>,
) -> FingerprintCheck {
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
            result: check_support(profile, fingerprint, configured),
        },
        Err(error) => FingerprintCheck {
            host,
            profile: None,
            result: Err(error),
        },
    }
}

/// Checks that the backend of `fingerprint` supports `profile`, reading the
/// entries outside the profile's tables from `configured` if it returns
/// them.
fn check_support(
    profile: &CpuProfile,
    fingerprint: &CpuFingerprint,
    configured: impl FnOnce(&CpuProfile) -> Result<Option<Vec<CpuidEntry>>, ProfileError>,
) -> Result<(), ProfileError> {
    let surface = HostCpuSurface::from_fingerprint(&fingerprint.backend);
    if surface.presentation != CpuidPresentation::PassThroughGuestView
        || !support_violations(profile, &surface).is_empty()
    {
        return verify_support(profile, &surface);
    }
    match configured(profile)? {
        Some(presented) => verify_support_with_unlisted(profile, &surface, &presented),
        None => verify_support(profile, &surface),
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
    use crate::test_support::host_fingerprint;
    use crate::test_support::milan_whp_entries;
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
        let mut tiger_lake = fingerprint(profile, "whp");
        tiger_lake.host.cpu.signature = Hex32(0x0008_06c1);
        tiger_lake.host.cpu.invariant_tsc = false;
        let check = check_fingerprint(&tiger_lake);
        assert_eq!(
            check.result.as_ref().unwrap_err().code,
            ProfileErrorCode::ProfileHostUnknown
        );
        let line = check.summary_line(&tiger_lake);
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

    #[test]
    fn fails_pass_through_hosts_that_present_unlisted_entries() {
        let profile = profile("intel.skylake-sp.v1");
        // The RDT monitoring subleaf, which the profile does not list.
        let mut entries = profile_entries(profile);
        entries.push(CpuidEntry::new(0xf, Some(1), [0, 0xb, 0x6f, 0x7]));
        entries.sort_by_key(CpuidEntry::key);
        for backend in ["mshv", "whp"] {
            let fingerprint = fingerprint_with(profile, backend, entries.clone());
            let check = check_fingerprint(&fingerprint);
            assert_eq!(
                check.result.as_ref().unwrap_err().code,
                ProfileErrorCode::CpuUnlisted
            );
            let line = check.summary_line(&fingerprint);
            assert!(
                line.starts_with(&format!(
                    "NVX-CPU-PROFILE: status=fail backend={backend} generation=skylake-sp \
                     profile=intel.skylake-sp.v1 profile_digest="
                )),
                "{line}"
            );
            assert!(
                line.ends_with(
                    " code=E_CPU_UNLISTED detail=\"the backend presents CPUID entries \
                     outside CPU profile intel.skylake-sp.v1: CPUID 0xf.1 is outside the profile \
                     and reads EAX 0x0, EBX 0xb, ECX 0x6f, EDX 0x7\""
                ),
                "{line}"
            );
        }

        // KVM answers unlisted entries from its table, so the check does not
        // apply.
        let fingerprint = fingerprint_with(profile, "kvm", entries);
        check_fingerprint(&fingerprint).result.unwrap();
    }

    /// A WHP host whose probe partition, with every available feature,
    /// presents CET's XSAVE components, which the profile does not enable,
    /// as the root partition of a 12th generation Intel Core host reports
    /// them.
    fn cet_host() -> CpuFingerprint {
        let profile = profile("intel.alderlake.v1");
        let mut entries = profile_entries(profile);
        entries.push(CpuidEntry::new(0xd, Some(11), [0x10, 0, 1, 0]));
        entries.push(CpuidEntry::new(0xd, Some(12), [0x18, 0, 1, 0]));
        entries.sort_by_key(CpuidEntry::key);
        fingerprint_with(profile, "whp", entries)
    }

    #[test]
    fn checks_unlisted_entries_on_a_partition_configured_from_the_profile() {
        let fingerprint = cet_host();
        // The probe partition's own view fails, though a cold boot passes.
        let check = check_fingerprint(&fingerprint);
        let error = check.result.unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::CpuUnlisted);
        assert!(
            error.message.ends_with(
                "CPUID 0xd.11 is outside the profile and reads EAX 0x10, EBX 0x0, ECX 0x1, \
                 EDX 0x0; CPUID 0xd.12 is outside the profile and reads EAX 0x18, EBX 0x0, \
                 ECX 0x1, EDX 0x0"
            ),
            "{error}"
        );

        // A partition configured from the profile reads zero there.
        let zeros = [
            (0xd, Some(11)),
            (0xd, Some(12)),
            (0x14, Some(1)),
            (0x20, None),
        ]
        .map(|(leaf, subleaf)| CpuidEntry::new(leaf, subleaf, [0; 4]));
        let mut asked = None;
        let check = check_fingerprint_with(&fingerprint, |profile| {
            asked = Some(profile.id().to_owned());
            Ok(Some(zeros.to_vec()))
        });
        check.result.as_ref().unwrap();
        assert_eq!(asked.as_deref(), Some("intel.alderlake.v1"));
        assert!(
            check
                .summary_line(&fingerprint)
                .starts_with("NVX-CPU-PROFILE: status=pass backend=whp generation=alderlake "),
            "{}",
            check.summary_line(&fingerprint)
        );

        // A non-zero entry of the configured partition fails.
        let mut presented = zeros.to_vec();
        presented[2] = CpuidEntry::new(0x14, Some(1), [0x0249_0002, 0x003f_003f, 0, 0]);
        let error = check_fingerprint_with(&fingerprint, |_| Ok(Some(presented)))
            .result
            .unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::CpuUnlisted);
        assert!(
            error.message.ends_with(
                "CPUID 0x14.1 is outside the profile and reads EAX 0x2490002, EBX 0x3f003f, \
                 ECX 0x0, EDX 0x0"
            ),
            "{error}"
        );

        // Without a configured partition, the probe's own view counts.
        assert_eq!(
            check_fingerprint_with(&fingerprint, |_| Ok(None))
                .result
                .unwrap_err()
                .code,
            ProfileErrorCode::CpuUnlisted
        );
        // A backend that cannot configure the partition fails the check.
        let error = check_fingerprint_with(&fingerprint, |_| {
            Err(ProfileError::new(
                ProfileErrorCode::ProfileUnsupported,
                "WHP rejects the features",
            ))
        })
        .result
        .unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileUnsupported);
    }

    /// The host that `amd.milan.v1` was derived from supports it: an AMD EPYC
    /// 7763 that WHP serves on an Azure host. Its probe partition presents
    /// CET's XSAVE components, which a partition configured from the profile
    /// does not, as on the 12th generation Intel Core host.
    #[test]
    fn the_milan_host_supports_its_profile() {
        let fingerprint = host_fingerprint("whp", milan_whp_entries());
        let error = check_fingerprint(&fingerprint).result.unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::CpuUnlisted);
        assert!(
            error
                .message
                .contains("CPUID 0xd.11 is outside the profile"),
            "{error}"
        );
        let check = check_fingerprint_with(&fingerprint, |profile| {
            assert_eq!(profile.id(), "amd.milan.v1");
            Ok(Some(vec![
                CpuidEntry::new(0xd, Some(11), [0; 4]),
                CpuidEntry::new(0xd, Some(12), [0; 4]),
            ]))
        });
        check.result.as_ref().unwrap();
        assert!(
            check.summary_line(&fingerprint).starts_with(
                "NVX-CPU-PROFILE: status=pass backend=whp generation=milan \
                 profile=amd.milan.v1 profile_digest=sha256:463ee036"
            ),
            "{}",
            check.summary_line(&fingerprint)
        );
    }

    #[test]
    fn configures_a_partition_only_when_it_decides_the_check() {
        let unused = |_: &CpuProfile| -> Result<Option<Vec<CpuidEntry>>, ProfileError> {
            panic!("the check configured a partition")
        };
        // KVM presents no host data outside the profile.
        let profile = profile("intel.alderlake.v1");
        check_fingerprint_with(&fingerprint(profile, "kvm"), unused)
            .result
            .unwrap();
        // An unsupported profile fails first, naming every violation.
        let mut entries = profile_entries(profile);
        let entry = entries
            .iter_mut()
            .find(|entry| entry.key() == (7, Some(0)))
            .unwrap();
        let [eax, ebx, ecx, edx] = entry.registers();
        *entry = CpuidEntry::new(7, Some(0), [eax, ebx & !(1 << 5), ecx, edx]);
        entries.push(CpuidEntry::new(0xd, Some(11), [0x10, 0, 1, 0]));
        entries.sort_by_key(CpuidEntry::key);
        let error = check_fingerprint_with(&fingerprint_with(profile, "whp", entries), unused)
            .result
            .unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileUnsupported);
        assert!(
            error
                .message
                .contains("CPUID 0x7.0 EBX bit 5 is not supported")
        );
        assert!(
            error
                .message
                .contains("CPUID 0xd.11 is outside the profile")
        );
        // An unknown host has no profile to configure.
        let mut unknown = cet_host();
        unknown.host.cpu.signature = Hex32(0x0008_06c1);
        assert_eq!(
            check_fingerprint_with(&unknown, unused)
                .result
                .unwrap_err()
                .code,
            ProfileErrorCode::ProfileHostUnknown
        );
    }
}
