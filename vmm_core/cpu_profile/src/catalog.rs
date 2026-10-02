// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU profiles pinned in this OpenVMM, profile selection, and the
//! profile checks of cold boot and restore.

use crate::canonical;
use crate::derive::KnownGeneration;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::pinned::PinnedRecord;
use crate::pinned_data::PINNED;
use crate::profile::CpuProfile;
use crate::signature::HostCpuSignature;
use crate::signature::decode_signature;
use std::sync::OnceLock;

/// The `--cpu-profile` value that selects the profile of the host's
/// generation.
pub const AUTO: &str = "auto";

/// Each pinned profile, built from its static data on first use, so that a
/// cold boot or a restore builds only its own.
static PROFILES: [OnceLock<CpuProfile>; PINNED.len()] = [const { OnceLock::new() }; PINNED.len()];

/// Returns the pinned profile at `index` in [`PINNED`], building it from its
/// constants on first use: no parsing, no validation, and no hashing, which
/// the tests do for every pinned profile instead.
fn load(index: usize) -> &'static CpuProfile {
    PROFILES[index].get_or_init(|| PINNED[index].to_profile())
}

/// Returns the index in [`PINNED`] of the profile `id`.
fn position(id: &str) -> Option<usize> {
    PINNED.iter().position(|pinned| pinned.id == id)
}

/// Returns every pinned profile. Selecting by ID with [`pinned`] or
/// [`pinned_for_restore`] builds only that profile.
pub fn pinned_profiles() -> &'static [CpuProfile] {
    static ALL: OnceLock<Vec<CpuProfile>> = OnceLock::new();
    ALL.get_or_init(|| (0..PINNED.len()).map(|index| load(index).clone()).collect())
}

/// Returns the pinned profile `id`.
pub fn pinned(id: &str) -> Option<&'static CpuProfile> {
    position(id).map(load)
}

/// Returns the precomputed record of the pinned profile `id`: its canonical
/// encoding and digest, for a snapshot's CPU profile record.
pub fn pinned_record(id: &str) -> Option<PinnedRecord> {
    position(id).map(|index| PINNED[index].record())
}

/// Returns the name of the generation of the host CPU, if a pinned profile
/// serves it: the generation of the profile [`select_auto`] selects.
pub fn generation_of(host: &HostCpuSignature) -> Option<&'static str> {
    select_auto(host)
        .ok()
        .map(|profile| profile.generation().name.as_str())
}

/// Selects the profile of the host CPU's generation: its latest pinned
/// revision. Only the profiles whose generation covers the host are built.
///
/// Fails with `E_PROFILE_HOST_UNKNOWN` when no pinned profile serves the
/// host, or when profiles of more than one generation do; there is no host
/// CPUID passthrough.
pub fn select_auto(host: &HostCpuSignature) -> Result<&'static CpuProfile, ProfileError> {
    let candidates = (0..PINNED.len())
        .filter(|&index| covers(PINNED[index].generation, host))
        .map(load)
        .collect::<Vec<_>>();
    select_auto_in(&candidates, host, pinned_ids)
}

/// Returns whether `generation` covers the host CPU, as
/// [`Generation::contains`](crate::Generation::contains) does for the
/// generation of a parsed profile.
fn covers(generation: &KnownGeneration, host: &HostCpuSignature) -> bool {
    let vendor = generation.vendor.as_bytes();
    let (family, model, stepping) = decode_signature(vendor, host.signature());
    host.vendor().as_slice() == vendor
        && generation
            .cpus
            .iter()
            .any(|&(cpu_family, cpu_model, [first, last])| {
                (cpu_family, cpu_model) == (family, model) && (first..=last).contains(&stepping)
            })
}

/// Selects among `profiles`, as [`select_auto`] does; `ids` lists every
/// pinned profile for the failure messages.
fn select_auto_in<'a>(
    profiles: &[&'a CpuProfile],
    host: &HostCpuSignature,
    ids: impl Fn() -> String,
) -> Result<&'a CpuProfile, ProfileError> {
    let matches = profiles
        .iter()
        .copied()
        .filter(|profile| profile.generation().contains(profile.vendor(), host))
        .collect::<Vec<_>>();
    let mut generations = matches
        .iter()
        .map(|profile| profile.generation().name.as_str())
        .collect::<Vec<_>>();
    generations.sort_unstable();
    generations.dedup();
    if generations.len() > 1 {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileHostUnknown,
            format!(
                "the host CPU, {host}, is in more than one CPU generation ({}); pinned profiles: {}",
                generations.join(", "),
                ids()
            ),
        ));
    }
    matches
        .into_iter()
        .max_by_key(|profile| revision(profile.id()))
        .ok_or_else(|| {
            ProfileError::new(
                ProfileErrorCode::ProfileHostUnknown,
                format!(
                    "no pinned CPU profile serves the host CPU, {host}; pinned profiles: {}",
                    ids()
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
/// with `E_PROFILE_DIGEST` when its pinned profile has another digest. The
/// pinned profile's digest is a constant, which the catalog's tests check, so
/// a restore encodes and hashes nothing here.
pub fn pinned_for_restore(id: &str, sha256: &[u8]) -> Result<&'static CpuProfile, ProfileError> {
    let index = position(id).ok_or_else(|| unknown(id))?;
    let golden = &PINNED[index].digest;
    if sha256 != golden {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileDigest,
            format!(
                "the snapshot's CPU profile {id} differs from the pinned profile of the same ID ({})",
                canonical::format_digest(golden)
            ),
        ));
    }
    Ok(load(index))
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
    PINNED
        .iter()
        .map(|pinned| pinned.id)
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

    /// The pinned JSON files, the source of the static data, and their golden
    /// digests. Released profiles are immutable: changing a golden digest is
    /// a deliberate act, here.
    const FILES: [(&str, &str, &str); 3] = [
        (
            "intel.skylake-sp.v1",
            include_str!("../profiles/intel.skylake-sp.v1.json"),
            "sha256:b36ef861ffec67350a28faff8c446d5a19f78d6d58862378fa9d2282dad87492",
        ),
        (
            "intel.icelake-sp.v1",
            include_str!("../profiles/intel.icelake-sp.v1.json"),
            "sha256:a35f3bb9bc30337b8b76af4939d5914f878ac7dc2a2d6d9d6bf4d8278b91f301",
        ),
        (
            "intel.emeraldrapids.v1",
            include_str!("../profiles/intel.emeraldrapids.v1.json"),
            "sha256:73c084783f26df29871c72200ea34470e4afec6187bf96de6ac051dbaed6727d",
        ),
    ];

    /// The checks that an OpenVMM start skips: every pinned file is a valid
    /// profile in pretty canonical form with its golden digest, and the static
    /// data generated from it holds the same profile, canonical encoding,
    /// digest, and generation.
    #[test]
    fn the_static_data_is_the_pinned_files() {
        assert_eq!(PINNED.len(), FILES.len());
        for (index, (pinned, (id, json, golden))) in PINNED.iter().zip(FILES).enumerate() {
            let profile = CpuProfile::from_pretty_json(json).unwrap();
            assert_eq!(profile.id(), id);
            assert_eq!(profile.digest_string(), golden, "{id}");
            assert_eq!(pinned.id, id);
            assert_eq!(&pinned.to_profile(), &profile, "{id}");
            assert_eq!(load(index), &profile, "{id}");
            assert_eq!(pinned.encoding.as_bytes(), profile.encode(), "{id}");
            assert_eq!(canonical::format_digest(&pinned.digest), golden, "{id}");
            assert_eq!(
                canonical::sha256(pinned.encoding.as_bytes()),
                pinned.digest,
                "{id}"
            );
            assert_eq!(profile.vendor(), pinned.generation.vendor);
            assert_eq!(profile.generation(), &pinned.generation.generation());
            assert_eq!(
                pinned_record(id),
                Some(PinnedRecord {
                    id: pinned.id,
                    encoding: pinned.encoding.as_bytes(),
                    digest: profile.digest(),
                })
            );
            assert!(std::ptr::eq(
                pinned_for_restore(id, &profile.digest()).unwrap(),
                load(index)
            ));
        }
        assert_eq!(pinned_record("intel.icelake-sp.v2"), None);
        assert_eq!(
            pinned_profiles()
                .iter()
                .map(CpuProfile::id)
                .collect::<Vec<_>>(),
            FILES.map(|(id, ..)| id)
        );
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
    fn auto_takes_the_latest_revision_and_rejects_ambiguous_generations() {
        let json = profile("intel.skylake-sp.v1").to_pretty_json();
        let variant =
            |from: &str, to: &str| CpuProfile::from_pretty_json(&json.replace(from, to)).unwrap();
        let host = intel(SKYLAKE);
        let ids = || "the test's".to_owned();
        let revisions = [
            variant("intel.skylake-sp.v1", "intel.skylake-sp.v2"),
            variant("intel.skylake-sp.v1", "intel.skylake-sp.v10"),
            CpuProfile::from_pretty_json(&json).unwrap(),
        ];
        assert_eq!(
            select_auto_in(&revisions.iter().collect::<Vec<_>>(), &host, ids)
                .unwrap()
                .id(),
            "intel.skylake-sp.v10"
        );

        // Two generations that both cover the host.
        let ambiguous = [
            CpuProfile::from_pretty_json(&json).unwrap(),
            variant("skylake-sp", "skylake-x"),
        ];
        let ambiguous = ambiguous.iter().collect::<Vec<_>>();
        let error = select_auto_in(&ambiguous, &host, ids).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileHostUnknown);
        assert!(
            error.message.contains(
                "is in more than one CPU generation (skylake-sp, skylake-x); \
                 pinned profiles: the test's"
            ),
            "{error}"
        );
        assert_eq!(
            code(select_auto_in(&ambiguous, &intel(ICELAKE), ids)),
            ProfileErrorCode::ProfileHostUnknown
        );
    }

    /// Selection matches the host against each pinned entry's generation, to
    /// parse only the profile it selects; that agrees with the profiles' own
    /// generations.
    #[test]
    fn the_static_generations_cover_what_the_profiles_do() {
        for (index, pinned) in PINNED.iter().enumerate() {
            let profile = load(index);
            for vendor in [*b"GenuineIntel", *b"AuthenticAMD"] {
                for (family, model) in [(6u32, 85u32), (6, 106), (6, 143), (6, 207), (25, 17)] {
                    for stepping in 0..16 {
                        // Encode the display family and model as CPUID.1:EAX does.
                        let extended_family = family.saturating_sub(15);
                        let base_family = family - extended_family;
                        let signature = extended_family << 20
                            | (model >> 4) << 16
                            | base_family << 8
                            | (model & 0xf) << 4
                            | stepping;
                        let host = HostCpuSignature::new(vendor, signature);
                        assert_eq!(
                            covers(pinned.generation, &host),
                            profile.generation().contains(profile.vendor(), &host),
                            "{} on {host}",
                            pinned.id
                        );
                    }
                }
            }
        }
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
