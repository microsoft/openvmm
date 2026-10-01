// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU profiles pinned in this OpenVMM, profile selection, and the
//! profile checks of cold boot and restore.

use crate::canonical;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::profile::CpuProfile;
use crate::signature::HostCpuSignature;
use std::sync::OnceLock;

/// The `--cpu-profile` value that selects the profile of the host's
/// generation.
pub const AUTO: &str = "auto";

/// A pinned profile: its pretty canonical JSON and its golden digest.
struct Pinned {
    json: &'static str,
    digest: &'static str,
}

/// The pinned profiles. Released profiles are immutable: a change is a new
/// revision with a new ID, and a test checks every golden digest.
const PINNED: &[Pinned] = &[
    Pinned {
        json: include_str!("../profiles/intel.skylake-sp.v1.json"),
        digest: "sha256:e1c8e7965ec04693b2e3bd97eb93da55a2dc518118e48b563241772190d4b0a8",
    },
    Pinned {
        json: include_str!("../profiles/intel.icelake-sp.v1.json"),
        digest: "sha256:5dbabb30bef86017f5d0858c2f3c056d345bfd33698e2f69877150a9cc66f812",
    },
    Pinned {
        json: include_str!("../profiles/intel.emeraldrapids.v1.json"),
        digest: "sha256:84106dd216e6dc858c1e8f7813eff29b0ee61ab7968b676c74efb268f9ed700f",
    },
];

/// Returns every pinned profile.
pub fn pinned_profiles() -> &'static [CpuProfile] {
    static CATALOG: OnceLock<Vec<CpuProfile>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        PINNED
            .iter()
            .map(|pinned| {
                let profile = CpuProfile::from_pretty_json(pinned.json)
                    .unwrap_or_else(|error| panic!("invalid pinned CPU profile: {error}"));
                assert_eq!(
                    profile.digest_string(),
                    pinned.digest,
                    "pinned CPU profile {} changed, but released profiles are immutable",
                    profile.id()
                );
                profile
            })
            .collect()
    })
}

/// Returns the pinned profile `id`.
pub fn pinned(id: &str) -> Option<&'static CpuProfile> {
    pinned_profiles().iter().find(|profile| profile.id() == id)
}

/// Returns the name of the generation of the host CPU, if a pinned profile
/// serves it.
pub fn generation_of(host: &HostCpuSignature) -> Option<&'static str> {
    pinned_profiles()
        .iter()
        .find(|profile| profile.generation().contains(profile.vendor(), host))
        .map(|profile| profile.generation().name.as_str())
}

/// Selects the profile of the host CPU's generation: its latest pinned
/// revision.
///
/// Fails with `E_PROFILE_HOST_UNKNOWN` when no pinned profile serves the
/// host; there is no host CPUID passthrough.
pub fn select_auto(host: &HostCpuSignature) -> Result<&'static CpuProfile, ProfileError> {
    pinned_profiles()
        .iter()
        .filter(|profile| profile.generation().contains(profile.vendor(), host))
        .max_by_key(|profile| revision(profile.id()))
        .ok_or_else(|| {
            ProfileError::new(
                ProfileErrorCode::ProfileHostUnknown,
                format!(
                    "no pinned CPU profile serves the host CPU, {host}; pinned profiles: {}",
                    pinned_ids()
                ),
            )
        })
}

/// Selects the profile for `--cpu-profile spec`, [`AUTO`] or a pinned ID,
/// and checks that the host CPU is in its generation.
///
/// Fails with `E_PROFILE_HOST_UNKNOWN`, `E_PROFILE_UNKNOWN` for an ID that is
/// not pinned, or `E_CPU_GENERATION`.
pub fn select(spec: &str, host: &HostCpuSignature) -> Result<&'static CpuProfile, ProfileError> {
    if spec == AUTO {
        return select_auto(host);
    }
    let profile = pinned(spec).ok_or_else(|| unknown(spec))?;
    check_generation(profile, host)?;
    Ok(profile)
}

/// Checks that the host CPU is in the profile's generation
/// (`E_CPU_GENERATION`).
pub fn check_generation(profile: &CpuProfile, host: &HostCpuSignature) -> Result<(), ProfileError> {
    if profile.generation().contains(profile.vendor(), host) {
        Ok(())
    } else {
        Err(ProfileError::new(
            ProfileErrorCode::CpuGeneration,
            format!(
                "the host CPU, {host}, is not in generation {} of CPU profile {}",
                profile.generation().name,
                profile.id()
            ),
        ))
    }
}

/// Verifies the CPU profile record of a snapshot: the embedded canonical
/// profile matches its recorded SHA-256 and recorded ID, and decodes
/// (`E_PROFILE_DIGEST`).
pub fn verify_profile_record(
    id: &str,
    sha256: &[u8],
    embedded: &[u8],
) -> Result<CpuProfile, ProfileError> {
    if canonical::sha256(embedded).as_slice() != sha256 {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileDigest,
            format!("the embedded CPU profile {id} does not match its recorded digest"),
        ));
    }
    let profile = CpuProfile::decode(embedded)?;
    if profile.id() != id {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileDigest,
            format!(
                "the embedded CPU profile is {}, but the record names {id}",
                profile.id()
            ),
        ));
    }
    Ok(profile)
}

/// Returns the pinned profile that a snapshot recorded by ID and SHA-256.
///
/// Fails with `E_PROFILE_UNKNOWN` when this OpenVMM does not pin the ID, and
/// with `E_PROFILE_DIGEST` when its pinned profile has another digest.
pub fn pinned_for_restore(id: &str, sha256: &[u8]) -> Result<&'static CpuProfile, ProfileError> {
    let profile = pinned(id).ok_or_else(|| unknown(id))?;
    if profile.digest().as_slice() != sha256 {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileDigest,
            format!(
                "the snapshot's CPU profile {id} differs from the pinned profile of the same ID ({})",
                profile.digest_string()
            ),
        ));
    }
    Ok(profile)
}

/// Runs the profile checks of a restore: [`verify_profile_record`],
/// [`pinned_for_restore`], and [`check_generation`] for the destination
/// host.
pub fn restore_profile(
    id: &str,
    sha256: &[u8],
    embedded: &[u8],
    host: &HostCpuSignature,
) -> Result<&'static CpuProfile, ProfileError> {
    verify_profile_record(id, sha256, embedded)?;
    let profile = pinned_for_restore(id, sha256)?;
    check_generation(profile, host)?;
    Ok(profile)
}

fn unknown(id: &str) -> ProfileError {
    ProfileError::new(
        ProfileErrorCode::ProfileUnknown,
        format!(
            "CPU profile {id:?} is not pinned in this OpenVMM; pinned profiles: {}",
            pinned_ids()
        ),
    )
}

fn pinned_ids() -> String {
    pinned_profiles()
        .iter()
        .map(|profile| profile.id())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns the revision of a valid profile ID.
fn revision(id: &str) -> u32 {
    id.rsplit_once(".v")
        .and_then(|(_, revision)| revision.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::derive;
    use crate::test_support::profile;
    use test_with_tracing::test;

    const SKYLAKE: u32 = 0x0005_0654;
    const CASCADE_LAKE: u32 = 0x0005_0657;
    const ICELAKE: u32 = 0x0006_06a6;
    const EMERALDRAPIDS: u32 = 0x000c_06f2;
    const ALDER_LAKE: u32 = 0x0009_06a3;

    fn intel(signature: u32) -> HostCpuSignature {
        HostCpuSignature::new(*b"GenuineIntel", signature)
    }

    fn code<T: std::fmt::Debug>(result: Result<T, ProfileError>) -> ProfileErrorCode {
        result.unwrap_err().code
    }

    #[test]
    fn pins_the_three_generations_with_their_golden_digests() {
        let pinned = pinned_profiles()
            .iter()
            .map(|profile| (profile.id(), profile.digest_string()))
            .collect::<Vec<_>>();
        let golden = PINNED
            .iter()
            .zip([
                "intel.skylake-sp.v1",
                "intel.icelake-sp.v1",
                "intel.emeraldrapids.v1",
            ])
            .map(|(pinned, id)| (id, pinned.digest.to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(pinned, golden);
    }

    #[test]
    fn generations_are_known_and_disjoint() {
        for (i, profile) in pinned_profiles().iter().enumerate() {
            let generation = profile.generation();
            let known = derive::known_generation(&generation.name).unwrap();
            assert_eq!(profile.vendor(), known.vendor);
            assert_eq!(
                generation
                    .cpus
                    .iter()
                    .map(|cpu| (cpu.family, cpu.model, cpu.steppings))
                    .collect::<Vec<_>>(),
                known.cpus
            );
            for other in &pinned_profiles()[i + 1..] {
                assert_ne!(other.id(), profile.id());
                if other.generation().name != generation.name {
                    for cpu in &generation.cpus {
                        assert!(!other.generation().cpus.iter().any(|other| {
                            (other.family, other.model) == (cpu.family, cpu.model)
                                && other.steppings[0] <= cpu.steppings[1]
                                && cpu.steppings[0] <= other.steppings[1]
                        }));
                    }
                }
            }
        }
    }

    #[test]
    fn auto_selects_the_host_generation_or_fails() {
        for (signature, id, generation) in [
            (SKYLAKE, "intel.skylake-sp.v1", "skylake-sp"),
            (ICELAKE, "intel.icelake-sp.v1", "icelake-sp"),
            (EMERALDRAPIDS, "intel.emeraldrapids.v1", "emeraldrapids"),
        ] {
            assert_eq!(select_auto(&intel(signature)).unwrap().id(), id);
            assert_eq!(select(AUTO, &intel(signature)).unwrap().id(), id);
            assert_eq!(generation_of(&intel(signature)), Some(generation));
        }
        for host in [
            intel(CASCADE_LAKE),
            intel(ALDER_LAKE),
            HostCpuSignature::new(*b"AuthenticAMD", 0x00a1_0f11),
        ] {
            assert_eq!(
                code(select_auto(&host)),
                ProfileErrorCode::ProfileHostUnknown
            );
            assert_eq!(generation_of(&host), None);
        }
        let error = select_auto(&intel(CASCADE_LAKE)).unwrap_err();
        assert!(
            error.to_string().starts_with("[E_PROFILE_HOST_UNKNOWN] "),
            "{error}"
        );
        assert!(error.message.contains("model 85 stepping 7"), "{error}");
    }

    #[test]
    fn explicit_selection_checks_the_id_and_the_generation() {
        let host = intel(ICELAKE);
        assert_eq!(
            select("intel.icelake-sp.v1", &host).unwrap().id(),
            "intel.icelake-sp.v1"
        );
        assert_eq!(
            code(select("intel.skylake-sp.v1", &host)),
            ProfileErrorCode::CpuGeneration
        );
        assert_eq!(
            code(select("intel.icelake-sp.v2", &host)),
            ProfileErrorCode::ProfileUnknown
        );
        assert_eq!(code(select("", &host)), ProfileErrorCode::ProfileUnknown);
    }

    #[test]
    fn restore_checks_the_record_the_pinned_profile_and_the_host() {
        let profile = profile("intel.icelake-sp.v1");
        let bytes = profile.encode();
        let digest = profile.digest();
        let host = intel(ICELAKE);
        assert_eq!(
            restore_profile(profile.id(), &digest, &bytes, &host).unwrap(),
            profile
        );

        // A corrupted record.
        let mut corrupted = bytes.clone();
        corrupted[10] ^= 1;
        assert_eq!(
            code(restore_profile(profile.id(), &digest, &corrupted, &host)),
            ProfileErrorCode::ProfileDigest
        );
        assert_eq!(
            code(restore_profile(profile.id(), &digest[..31], &bytes, &host)),
            ProfileErrorCode::ProfileDigest
        );
        assert_eq!(
            code(restore_profile(
                "intel.skylake-sp.v1",
                &digest,
                &bytes,
                &host
            )),
            ProfileErrorCode::ProfileDigest
        );

        // A profile this OpenVMM does not pin.
        let mut unknown = profile.clone();
        unknown.id = "intel.icelake-sp.v9".to_owned();
        let unknown_bytes = unknown.encode();
        assert_eq!(
            code(restore_profile(
                unknown.id(),
                &unknown.digest(),
                &unknown_bytes,
                &host
            )),
            ProfileErrorCode::ProfileUnknown
        );

        // A pinned ID whose content differs.
        let mut changed = profile.clone();
        changed.description.push('!');
        assert_eq!(
            code(restore_profile(
                changed.id(),
                &changed.digest(),
                &changed.encode(),
                &host
            )),
            ProfileErrorCode::ProfileDigest
        );

        // Another generation on the destination.
        assert_eq!(
            code(restore_profile(
                profile.id(),
                &digest,
                &bytes,
                &intel(EMERALDRAPIDS)
            )),
            ProfileErrorCode::CpuGeneration
        );
    }
}
