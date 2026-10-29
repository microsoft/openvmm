// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPU profiles pinned in this OpenVMM, profile selection, and the
//! profile checks of cold boot and restore.

use crate::canonical;
use crate::derive::HOST_GENERATION;
use crate::derive::KnownGeneration;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::pinned::PinnedRecord;
use crate::pinned_data::PINNED;
use crate::profile::CpuProfile;
use crate::profile::is_id_component;
use crate::profile::is_revision;
use crate::signature::HostCpuSignature;
use crate::signature::decode_signature;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::OnceLock;

/// The `--cpu-profile` value that selects the profile of the host's
/// generation.
pub const AUTO: &str = "auto";

/// The `--cpu-profile` value that selects a host profile: a profile that the
/// VM worker derives from the backend's fingerprint of this host
/// ([`derive_host_profile`](crate::derive::derive_host_profile)) and that
/// only its partition holds ([`PartitionProfile::host`]). It is for
/// development hosts that no pinned profile serves. [`select`] never selects
/// it, but the VM worker falls back to it from [`AUTO`] on a CPU that no
/// pinned profile serves ([`pinned_profile_serves`]) and that host profiles
/// serve ([`supports_host_profiles`](crate::derive::supports_host_profiles)),
/// and warns.
pub const HOST: &str = "host";

/// Each pinned profile, built from its static data on first use, so that a
/// cold boot or a restore builds only the profiles that it considers: the one
/// that it names, or those that cover its host.
static PROFILES: [OnceLock<CpuProfile>; PINNED.len()] = [const { OnceLock::new() }; PINNED.len()];

/// Whether each pinned profile is valid, verified on its first use through
/// [`load`], so that a process verifies each profile that it uses once.
static VERIFIED: [OnceLock<Result<(), ProfileError>>; PINNED.len()] =
    [const { OnceLock::new() }; PINNED.len()];

/// Returns the pinned profile at `index` in [`PINNED`], copied from its
/// constants on first use, without verifying it. Only [`select_auto`] reads
/// it, for the ID and generation by which it selects among the revisions
/// that cover the host, and it then [`load`]s the one that it selects; like
/// [`covers`], it takes them from the constants either way.
fn build(index: usize) -> &'static CpuProfile {
    PROFILES[index].get_or_init(|| PINNED[index].to_profile())
}

/// Returns the pinned profile at `index` in [`PINNED`], built from its
/// constants and verified on first use ([`PinnedProfile::verify`]): no JSON
/// parsing and no hashing, but an encoding, once per profile and process, so
/// that a stale or malformed `pinned_data.rs` fails with `E_PROFILE_DIGEST`
/// before any partition uses the profile.
///
/// [`PinnedProfile::verify`]: crate::pinned::PinnedProfile::verify
fn load(index: usize) -> Result<&'static CpuProfile, ProfileError> {
    let profile = build(index);
    VERIFIED[index]
        .get_or_init(|| PINNED[index].verify(profile))
        .clone()?;
    Ok(profile)
}

/// Returns the index in [`PINNED`] of the profile `id`.
fn position(id: &str) -> Option<usize> {
    PINNED.iter().position(|pinned| pinned.id == id)
}

/// Returns every pinned profile. Selecting by ID with [`pinned`] or
/// [`pinned_for_restore`] builds and verifies only that profile.
///
/// Offline only: this builds and verifies every profile, for tools and tests,
/// and panics if one is invalid. A cold boot or restore selects its one
/// profile with [`select`] or [`pinned_for_restore`].
pub fn pinned_profiles() -> &'static [CpuProfile] {
    static ALL: OnceLock<Vec<CpuProfile>> = OnceLock::new();
    ALL.get_or_init(|| {
        (0..PINNED.len())
            .map(|index| match load(index) {
                Ok(profile) => profile.clone(),
                Err(error) => panic!("{error}"),
            })
            .collect()
    })
}

/// Returns the pinned profile `id`, verified.
///
/// Fails with `E_PROFILE_UNKNOWN` when this OpenVMM does not pin `id`, and
/// with `E_PROFILE_DIGEST` when its pinned profile is invalid.
pub fn pinned(id: &str) -> Result<&'static CpuProfile, ProfileError> {
    load(position(id).ok_or_else(|| unknown(id))?)
}

/// Returns the precomputed record of the pinned profile `id`: its canonical
/// encoding and digest, constants that a snapshot's CPU profile record
/// copies at capture and that the restore preflight compares byte for byte.
/// It does not verify the profile, which [`select`] and
/// [`pinned_for_restore`] do first.
pub fn pinned_record(id: &str) -> Option<PinnedRecord<'static>> {
    position(id).map(|index| PINNED[index].record())
}

/// A host profile with its record, computed once, when the VM worker selects
/// the profile.
struct HostProfile {
    profile: CpuProfile,
    encoding: Vec<u8>,
    digest: [u8; 32],
}

/// The CPU profile of a partition: a pinned profile, or a host profile.
///
/// A host profile has no constants in this OpenVMM. The VM worker derives it
/// for `--cpu-profile host`
/// ([`derive_host_profile`](crate::derive::derive_host_profile)) or decodes
/// it from the snapshot that it restores ([`host_profile_for_restore`]), and
/// only the partitions that use it hold it, so each VM of a process can have
/// its own. Core hands the partition's profile to the backend in its time ABI
/// configuration, and capture copies its [`record`](Self::record). Clones
/// share the profile.
#[derive(Clone)]
pub struct PartitionProfile(Source);

#[derive(Clone)]
enum Source {
    /// The pinned profile at this index in [`PINNED`], which [`load`]
    /// verified.
    Pinned(usize, &'static CpuProfile),
    /// A host profile.
    Host(Arc<HostProfile>),
}

impl PartitionProfile {
    /// Returns the partition profile of the pinned profile `id`, verified.
    ///
    /// Fails with `E_PROFILE_UNKNOWN` when this OpenVMM does not pin `id`, and
    /// with `E_PROFILE_DIGEST` when its pinned profile is invalid.
    pub fn pinned(id: &str) -> Result<Self, ProfileError> {
        let index = position(id).ok_or_else(|| unknown(id))?;
        Ok(Self(Source::Pinned(index, load(index)?)))
    }

    /// Returns the partition profile of `profile`, a host profile, and
    /// computes its record.
    ///
    /// Fails with `E_PROFILE_UNKNOWN` for a profile that is not a host
    /// profile.
    pub fn host(profile: CpuProfile) -> Result<Self, ProfileError> {
        if profile.generation().name != HOST_GENERATION || !is_host_profile_id(profile.id()) {
            return Err(ProfileError::new(
                ProfileErrorCode::ProfileUnknown,
                format!("CPU profile {} is not a host profile", profile.id()),
            ));
        }
        let encoding = profile.encode();
        let digest = canonical::sha256(&encoding);
        Ok(Self(Source::Host(Arc::new(HostProfile {
            profile,
            encoding,
            digest,
        }))))
    }

    /// Returns whether the profile is a host profile.
    pub fn is_host(&self) -> bool {
        matches!(self.0, Source::Host(_))
    }

    /// Returns the profile's record, which capture copies into a snapshot:
    /// the pinned profile's precomputed constants, or the host profile's
    /// encoding and digest, computed when it was selected.
    pub fn record(&self) -> PinnedRecord<'_> {
        match &self.0 {
            Source::Pinned(index, _) => PINNED[*index].record(),
            Source::Host(host) => PinnedRecord {
                id: host.profile.id(),
                encoding: &host.encoding,
                digest: host.digest,
            },
        }
    }
}

impl Deref for PartitionProfile {
    type Target = CpuProfile;

    fn deref(&self) -> &CpuProfile {
        match &self.0 {
            Source::Pinned(_, profile) => profile,
            Source::Host(host) => &host.profile,
        }
    }
}

impl fmt::Debug for PartitionProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartitionProfile")
            .field("id", &self.id())
            .field("host", &self.is_host())
            .finish()
    }
}

/// Returns whether `id` names a host profile, `<vendor>.host.v<revision>`,
/// with the syntax of every profile ID: the vendor is lowercase ASCII
/// letters, digits, and hyphens, and the revision a decimal number without a
/// leading zero. No pinned profile has such an ID.
pub fn is_host_profile_id(id: &str) -> bool {
    let mut components = id.split('.');
    matches!(
        (components.next(), components.next(), components.next(), components.next()),
        (Some(vendor), Some(HOST_GENERATION), Some(revision), None)
            if is_id_component(vendor) && revision.strip_prefix('v').is_some_and(is_revision)
    )
}

/// Returns the host profile that a snapshot recorded by ID, SHA-256, and
/// canonical document, for a restore, after checking that the document is
/// the canonical encoding of a valid host profile with that ID and digest
/// (`E_PROFILE_DIGEST`). An ID that does not name a host profile fails with
/// `E_PROFILE_UNKNOWN`.
///
/// Unlike [`pinned_for_restore`], this decodes and hashes the document: no
/// constant exists to compare it with. The restore preflight then checks
/// the destination host with [`check_generation`], and the VM worker
/// selects the profile for the partition ([`PartitionProfile::host`]).
pub fn host_profile_for_restore(
    id: &str,
    sha256: &[u8],
    document: &[u8],
) -> Result<CpuProfile, ProfileError> {
    if !is_host_profile_id(id) {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileUnknown,
            format!("CPU profile {id:?} is not a host profile"),
        ));
    }
    if canonical::sha256(document).as_slice() != sha256 {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileDigest,
            format!("the snapshot's host CPU profile {id} does not match its recorded digest"),
        ));
    }
    let profile = CpuProfile::decode(document)?;
    if profile.id() != id || profile.generation().name != HOST_GENERATION {
        return Err(ProfileError::new(
            ProfileErrorCode::ProfileDigest,
            format!(
                "the snapshot records CPU profile {id}, but its embedded document is {}",
                profile.id()
            ),
        ));
    }
    Ok(profile)
}

/// Returns the name of the generation of the host CPU, if a pinned profile
/// serves it: the generation of the profile [`select_auto`] selects.
pub fn generation_of(host: &HostCpuSignature) -> Option<&'static str> {
    select_auto(host)
        .ok()
        .map(|profile| profile.generation().name.as_str())
}

/// Returns whether a pinned profile serves the host CPU: whether the
/// generation of any pinned profile covers it. Unlike [`generation_of`], it
/// holds for a CPU that profiles of more than one generation cover, which
/// [`select_auto`] rejects as a catalog defect. The VM worker falls back from
/// [`AUTO`] to a host profile only on a CPU that no pinned profile serves.
/// Unlike [`select_auto`], which builds the profiles that cover the host, it
/// builds no profile: it reads only the pinned entries' generations.
pub fn pinned_profile_serves(host: &HostCpuSignature) -> bool {
    PINNED.iter().any(|pinned| covers(pinned.generation, host))
}

/// Selects the profile of the host CPU's generation: its latest pinned
/// revision. Only the profiles whose generation covers the host are built,
/// and only the selected one is verified ([`pinned`]).
///
/// Fails with `E_PROFILE_HOST_UNKNOWN` when no pinned profile serves the
/// host, or when profiles of more than one generation do; there is no host
/// CPUID passthrough. Fails with `E_PROFILE_DIGEST` when the selected
/// profile is invalid.
pub fn select_auto(host: &HostCpuSignature) -> Result<&'static CpuProfile, ProfileError> {
    let candidates = (0..PINNED.len())
        .filter(|&index| covers(PINNED[index].generation, host))
        .map(build)
        .collect::<Vec<_>>();
    pinned(select_auto_in(&candidates, host, pinned_ids)?.id())
}

/// Returns whether `generation` covers the host CPU, as
/// [`Generation::contains`](crate::Generation::contains) does for the
/// generation of a parsed profile.
fn covers(generation: &KnownGeneration, host: &HostCpuSignature) -> bool {
    let vendor = generation.vendor.cpuid_vendor().as_bytes();
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

/// Selects the pinned profile for `--cpu-profile spec`, [`AUTO`] or a pinned
/// ID, verifies it, and checks that the host CPU is in its generation. A host
/// profile has no ID to select it by: the VM worker derives it for [`HOST`],
/// and for [`AUTO`] on a CPU that no pinned profile serves, or takes a
/// restored snapshot's ([`PartitionProfile::host`]).
///
/// Fails with `E_PROFILE_HOST_UNKNOWN`, `E_PROFILE_UNKNOWN` for an ID that is
/// not pinned, `E_PROFILE_DIGEST` for a pinned profile that is invalid, or
/// `E_CPU_GENERATION`.
pub fn select(spec: &str, host: &HostCpuSignature) -> Result<PartitionProfile, ProfileError> {
    let id = if spec == AUTO {
        select_auto(host)?.id()
    } else {
        spec
    };
    let profile = PartitionProfile::pinned(id)?;
    check_generation(&profile, host)?;
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

/// Returns the pinned profile that a snapshot recorded by ID and SHA-256,
/// verified.
///
/// Fails with `E_PROFILE_UNKNOWN` when this OpenVMM does not pin the ID, and
/// with `E_PROFILE_DIGEST` when its pinned profile is invalid or has another
/// digest. The pinned profile's digest is a constant, which the catalog's
/// tests check, so a restore hashes nothing here. The restore preflight also
/// compares the recorded document with [`pinned_record`]'s encoding, which
/// the profile's verification checks, and checks the destination host with
/// [`check_generation`].
pub fn pinned_for_restore(id: &str, sha256: &[u8]) -> Result<&'static CpuProfile, ProfileError> {
    let index = position(id).ok_or_else(|| unknown(id))?;
    let profile = load(index)?;
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
    use crate::CpuidLeafValue;
    use crate::derive;
    use crate::pinned::PinnedProfile;
    use crate::test_support::fingerprint;
    use crate::test_support::profile;
    use test_with_tracing::test;

    const SKYLAKE: u32 = 0x0005_0654;
    const CASCADE_LAKE: u32 = 0x0005_0657;
    const ICELAKE: u32 = 0x0006_06a6;
    const EMERALDRAPIDS: u32 = 0x000c_06f2;
    const ALDER_LAKE: u32 = 0x0009_06a3;
    const ALDER_LAKE_S: u32 = 0x0009_0672;
    const TIGER_LAKE: u32 = 0x0008_06c1;
    const MILAN: u32 = 0x00a0_0f11;
    const MILAN_X: u32 = 0x00a0_0f12;
    const GENOA: u32 = 0x00a1_0f11;
    const GENOA_STEPPING_2: u32 = 0x00a1_0f12;
    const RAPHAEL: u32 = 0x00a6_0f12;
    const TURIN: u32 = 0x00b0_0f21;
    const TURIN_DENSE: u32 = 0x00b1_0f10;

    fn intel(signature: u32) -> HostCpuSignature {
        HostCpuSignature::new(*b"GenuineIntel", signature)
    }

    fn amd(signature: u32) -> HostCpuSignature {
        HostCpuSignature::new(*b"AuthenticAMD", signature)
    }

    fn code<T: fmt::Debug>(result: Result<T, ProfileError>) -> ProfileErrorCode {
        result.unwrap_err().code
    }

    /// The pinned JSON files, the source of the static data, and their golden
    /// digests. Released profiles are immutable: changing a golden digest is
    /// a deliberate act, here.
    const FILES: [(&str, &str, &str); 8] = [
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
        (
            "intel.alderlake.v1",
            include_str!("../profiles/intel.alderlake.v1.json"),
            "sha256:2dfb6ccfd5a195b1614e6d963615c3372f40806b8f19b0d8425142f8c18a0c2e",
        ),
        (
            "amd.milan.v1",
            include_str!("../profiles/amd.milan.v1.json"),
            "sha256:463ee0368dc01bf56a3ab1157d62b21b51d11b3dcccb46c804864748ef696543",
        ),
        (
            "amd.genoa.v1",
            include_str!("../profiles/amd.genoa.v1.json"),
            "sha256:894ac647399039242eb0f0e6582e91691234446ebc9362576d7c5613f34b966b",
        ),
        (
            "amd.genoa.v2",
            include_str!("../profiles/amd.genoa.v2.json"),
            "sha256:8ee00e9ee8fd5defe3eebdc343733195d477f5f3449defa403f616d530a84839",
        ),
        (
            "amd.turin.v1",
            include_str!("../profiles/amd.turin.v1.json"),
            "sha256:0d9d044922eae3f27442178249f0b60feb0db07efe446e6641a1fcbfbb3a04ff",
        ),
    ];

    /// The checks that an OpenVMM start leaves to the tests: every pinned
    /// file is a valid profile in pretty canonical form with its golden
    /// digest, and the static data generated from it holds the same profile,
    /// canonical encoding, digest, and generation. A start verifies only that
    /// the static data is a valid profile in its own canonical encoding.
    #[test]
    fn the_static_data_is_the_pinned_files() {
        assert_eq!(PINNED.len(), FILES.len());
        for (index, (pinned, (id, json, golden))) in PINNED.iter().zip(FILES).enumerate() {
            let profile = CpuProfile::from_pretty_json(json).unwrap();
            assert_eq!(profile.id(), id);
            assert_eq!(profile.digest_string(), golden, "{id}");
            assert_eq!(pinned.id, id);
            assert_eq!(&pinned.to_profile(), &profile, "{id}");
            pinned.verify(&profile).unwrap();
            assert_eq!(load(index).unwrap(), &profile, "{id}");
            assert_eq!(VERIFIED[index].get(), Some(&Ok(())), "{id}");
            assert_eq!(pinned.encoding.as_bytes(), profile.encode(), "{id}");
            assert_eq!(canonical::format_digest(&pinned.digest), golden, "{id}");
            assert_eq!(
                canonical::sha256(pinned.encoding.as_bytes()),
                pinned.digest,
                "{id}"
            );
            assert_eq!(profile.vendor(), pinned.generation.vendor.cpuid_vendor());
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
                load(index).unwrap()
            ));
        }
        assert_eq!(
            code(pinned("intel.icelake-sp.v2")),
            ProfileErrorCode::ProfileUnknown
        );
        assert_eq!(pinned_record("intel.icelake-sp.v2"), None);
        assert_eq!(
            pinned_profiles()
                .iter()
                .map(CpuProfile::id)
                .collect::<Vec<_>>(),
            FILES.map(|(id, ..)| id)
        );
    }

    /// A start refuses static data that is not a valid profile in its own
    /// canonical encoding, as a stale or malformed `pinned_data.rs` would
    /// hold, and names what is wrong.
    #[test]
    fn verification_rejects_stale_or_malformed_static_data() {
        fn edited(
            pinned: &PinnedProfile,
            edit: impl FnOnce(&mut Vec<CpuidLeafValue>),
        ) -> &'static [CpuidLeafValue] {
            let mut cpuid = pinned.cpuid.to_vec();
            edit(&mut cpuid);
            cpuid.leak()
        }
        fn leaf(cpuid: &mut [CpuidLeafValue], key: (u32, Option<u32>)) -> &mut CpuidLeafValue {
            cpuid.iter_mut().find(|entry| entry.key() == key).unwrap()
        }
        fn refusal(edited: PinnedProfile) -> String {
            let error = edited.verify(&edited.to_profile()).unwrap_err();
            assert_eq!(error.code, ProfileErrorCode::ProfileDigest, "{error}");
            error.message
        }

        let pinned = &PINNED[position("intel.icelake-sp.v1").unwrap()];
        pinned.verify(&pinned.to_profile()).unwrap();
        let invalid = "pinned CPU profile intel.icelake-sp.v1 is invalid: ";

        // Values that violate the CPU time bits: a TSC-deadline timer.
        let cpuid = edited(pinned, |cpuid| leaf(cpuid, (1, None)).value[2].0 |= 1 << 24);
        assert_eq!(
            refusal(PinnedProfile { cpuid, ..*pinned }),
            format!("{invalid}it violates the CPU time bits: CPUID 0x1 ECX bit 24 is set")
        );
        // A missing leaf.
        let cpuid = edited(pinned, |cpuid| cpuid.retain(|entry| entry.leaf.0 != 3));
        assert_eq!(
            refusal(PinnedProfile { cpuid, ..*pinned }),
            format!("{invalid}CPUID 0x3 lists subleaves [], expected [None]")
        );
        // A VM-owned field that the profile pins.
        let cpuid = edited(pinned, |cpuid| leaf(cpuid, (1, None)).mask[1].0 = !0);
        let message = refusal(PinnedProfile { cpuid, ..*pinned });
        assert!(
            message.starts_with(&format!("{invalid}CPUID 0x1 has mask")),
            "{message}"
        );
        // Another generation.
        let generation = derive::known_generation("skylake-sp").unwrap();
        assert_eq!(
            refusal(PinnedProfile {
                generation,
                ..*pinned
            }),
            format!("{invalid}ID \"intel.icelake-sp.v1\" is not intel.skylake-sp.v<revision>")
        );

        // Valid values that the encoding does not hold, and another profile's
        // encoding.
        let stale = "pinned CPU profile intel.icelake-sp.v1 differs from its constant \
                     canonical encoding";
        assert_eq!(
            refusal(PinnedProfile {
                description: "Another description",
                ..*pinned
            }),
            stale
        );
        assert_eq!(
            refusal(PinnedProfile {
                encoding: PINNED[position("intel.skylake-sp.v1").unwrap()].encoding,
                ..*pinned
            }),
            stale
        );

        // A digest that is not the encoding's: a start hashes nothing, and
        // leaves the digest to the tests, which check it against the golden
        // digest.
        let mut digest = pinned.digest;
        digest[0] ^= 1;
        let stale_digest = PinnedProfile { digest, ..*pinned };
        stale_digest.verify(&stale_digest.to_profile()).unwrap();
    }

    #[test]
    fn generations_are_known_and_disjoint() {
        for (i, profile) in pinned_profiles().iter().enumerate() {
            let generation = profile.generation();
            let known = derive::known_generation(&generation.name).unwrap();
            assert_eq!(profile.vendor(), known.vendor.cpuid_vendor());
            assert_eq!(profile.cpu_vendor(), known.vendor);
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
        for (host, id, generation) in [
            (intel(SKYLAKE), "intel.skylake-sp.v1", "skylake-sp"),
            (intel(ICELAKE), "intel.icelake-sp.v1", "icelake-sp"),
            (
                intel(EMERALDRAPIDS),
                "intel.emeraldrapids.v1",
                "emeraldrapids",
            ),
            (intel(ALDER_LAKE), "intel.alderlake.v1", "alderlake"),
            (intel(ALDER_LAKE_S), "intel.alderlake.v1", "alderlake"),
            (amd(MILAN), "amd.milan.v1", "milan"),
            (amd(MILAN_X), "amd.milan.v1", "milan"),
            (amd(GENOA), "amd.genoa.v2", "genoa"),
            (amd(GENOA_STEPPING_2), "amd.genoa.v2", "genoa"),
            (amd(TURIN), "amd.turin.v1", "turin"),
        ] {
            assert_eq!(select_auto(&host).unwrap().id(), id);
            assert_eq!(select(AUTO, &host).unwrap().id(), id);
            assert_eq!(generation_of(&host), Some(generation));
            assert!(pinned_profile_serves(&host), "{host}");
        }
        for host in [
            intel(CASCADE_LAKE),
            intel(TIGER_LAKE),
            amd(RAPHAEL),
            amd(TURIN_DENSE),
            // The vendor decides too: no Intel CPU has Milan's signature.
            intel(MILAN),
            HostCpuSignature::new(*b"HygonGenuine", MILAN),
        ] {
            assert_eq!(
                code(select_auto(&host)),
                ProfileErrorCode::ProfileHostUnknown
            );
            assert_eq!(generation_of(&host), None);
            assert!(!pinned_profile_serves(&host), "{host}");
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
            let profile = load(index).unwrap();
            for vendor in [*b"GenuineIntel", *b"AuthenticAMD"] {
                for (family, model) in [
                    (6u32, 85u32),
                    (6, 106),
                    (6, 140),
                    (6, 143),
                    (6, 151),
                    (6, 154),
                    (6, 207),
                    (25, 1),
                    (25, 17),
                    (25, 97),
                    (26, 2),
                    (26, 17),
                ] {
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

        // Another vendor's generation.
        assert_eq!(
            select("amd.milan.v1", &amd(MILAN)).unwrap().id(),
            "amd.milan.v1"
        );
        assert_eq!(
            code(select("intel.skylake-sp.v1", &amd(MILAN))),
            ProfileErrorCode::CpuGeneration
        );
        assert_eq!(
            code(select("amd.milan.v1", &host)),
            ProfileErrorCode::CpuGeneration
        );

        // Another generation of the same vendor.
        assert_eq!(
            select("amd.turin.v1", &amd(TURIN)).unwrap().id(),
            "amd.turin.v1"
        );
        assert_eq!(
            code(select("amd.genoa.v1", &amd(TURIN))),
            ProfileErrorCode::CpuGeneration
        );
        assert_eq!(
            code(select("amd.milan.v1", &amd(GENOA))),
            ProfileErrorCode::CpuGeneration
        );

        // An earlier revision of the host's generation, which auto no longer
        // selects.
        assert_eq!(
            select("amd.genoa.v1", &amd(GENOA)).unwrap().id(),
            "amd.genoa.v1"
        );
    }

    #[test]
    fn restore_checks_the_pinned_record_and_the_host() {
        let profile = profile("intel.icelake-sp.v1");
        let digest = profile.digest();
        let host = intel(ICELAKE);
        assert_eq!(pinned_for_restore(profile.id(), &digest).unwrap(), profile);
        check_generation(profile, &host).unwrap();
        // The preflight compares the recorded document with the pinned
        // encoding, which is the profile's.
        assert_eq!(
            pinned_record(profile.id()).unwrap().encoding,
            profile.encode().as_slice()
        );

        // A corrupted, short, or another profile's digest.
        let mut corrupted = digest;
        corrupted[10] ^= 1;
        assert_eq!(
            code(pinned_for_restore(profile.id(), &corrupted)),
            ProfileErrorCode::ProfileDigest
        );
        assert_eq!(
            code(pinned_for_restore(profile.id(), &digest[..31])),
            ProfileErrorCode::ProfileDigest
        );
        assert_eq!(
            code(pinned_for_restore("intel.skylake-sp.v1", &digest)),
            ProfileErrorCode::ProfileDigest
        );

        // A profile this OpenVMM does not pin.
        let mut unknown = profile.clone();
        unknown.id = "intel.icelake-sp.v9".to_owned();
        assert_eq!(
            code(pinned_for_restore(unknown.id(), &unknown.digest())),
            ProfileErrorCode::ProfileUnknown
        );

        // A pinned ID whose content differs.
        let mut changed = profile.clone();
        changed.description.push('!');
        assert_eq!(
            code(pinned_for_restore(changed.id(), &changed.digest())),
            ProfileErrorCode::ProfileDigest
        );

        // Another generation on the destination.
        assert_eq!(
            code(check_generation(profile, &intel(EMERALDRAPIDS))),
            ProfileErrorCode::CpuGeneration
        );
    }

    /// The host profile of this crate's test fixture: Alder Lake's pinned
    /// surface, fingerprinted on WHP.
    fn host_profile() -> CpuProfile {
        derive::derive_host_profile(&fingerprint(profile("intel.alderlake.v1"), "whp")).unwrap()
    }

    #[test]
    fn host_profile_ids_have_the_host_generation() {
        for (id, host) in [
            ("intel.host.v1", true),
            ("intel.host.v12", true),
            ("amd.host.v1", true),
            ("intel.icelake-sp.v1", false),
            ("interim.host.kvm.v1", false),
            ("intel.host", false),
            (".host.v1", false),
            ("Intel.host.v1", false),
            ("intel.host.v", false),
            ("intel.host.v0", false),
            ("intel.host.v01", false),
            ("intel.host.vfoo", false),
            ("intel.host.1", false),
            ("host", false),
            (HOST, false),
            (AUTO, false),
        ] {
            assert_eq!(is_host_profile_id(id), host, "{id}");
        }
        assert!(is_host_profile_id(host_profile().id()));
        assert!(pinned_profiles().iter().all(|profile| {
            !is_host_profile_id(profile.id()) && profile.generation().name != HOST_GENERATION
        }));
    }

    #[test]
    fn partition_profiles_carry_their_records() {
        let pinned = PartitionProfile::pinned("intel.icelake-sp.v1").unwrap();
        assert!(!pinned.is_host());
        assert!(std::ptr::eq(
            &*pinned,
            super::pinned("intel.icelake-sp.v1").unwrap()
        ));
        assert_eq!(
            pinned.record(),
            pinned_record("intel.icelake-sp.v1").unwrap()
        );
        assert_eq!(
            code(PartitionProfile::pinned("intel.host.v1")),
            ProfileErrorCode::ProfileUnknown
        );

        let host = host_profile();
        let selected = PartitionProfile::host(host.clone()).unwrap();
        assert!(selected.is_host());
        assert_eq!(*selected, host);
        let record = selected.record();
        assert_eq!(record.id, host.id());
        assert_eq!(record.encoding, host.encode().as_slice());
        assert_eq!(record.digest, host.digest());
        // Clones share the profile.
        assert!(std::ptr::eq(&*selected.clone(), &*selected));

        // A pinned profile is not a host profile.
        assert_eq!(
            code(PartitionProfile::host(
                profile("intel.alderlake.v1").clone()
            )),
            ProfileErrorCode::ProfileUnknown
        );
    }

    /// Every VM of a process holds its own host profile: two host profiles
    /// with the same ID, here derived from the fingerprints of two backends,
    /// coexist, as a later VM's restore of another host's snapshot needs.
    #[test]
    fn host_profiles_of_the_same_id_coexist() {
        let whp = PartitionProfile::host(host_profile()).unwrap();
        let kvm = PartitionProfile::host(
            derive::derive_host_profile(&fingerprint(profile("intel.alderlake.v1"), "kvm"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(whp.id(), kvm.id());
        assert_ne!(whp.record().digest, kvm.record().digest);
        assert_eq!(whp.record().digest, host_profile().digest());
    }

    #[test]
    fn selection_takes_pinned_profiles_only() {
        // A host profile has no ID to select it by, and `host` is not an ID.
        let host = host_profile();
        for spec in [host.id(), HOST] {
            assert_eq!(
                code(select(spec, &intel(ALDER_LAKE))),
                ProfileErrorCode::ProfileUnknown,
                "{spec}"
            );
        }
        let selected = select(AUTO, &intel(ALDER_LAKE)).unwrap();
        assert!(!selected.is_host());
        assert_eq!(selected.id(), "intel.alderlake.v1");
        assert_eq!(
            selected.record(),
            pinned_record("intel.alderlake.v1").unwrap()
        );
    }

    #[test]
    fn restore_decodes_and_checks_a_host_profile() {
        let host = host_profile();
        let encoding = host.encode();
        let digest = host.digest();
        assert_eq!(
            host_profile_for_restore(host.id(), &digest, &encoding).unwrap(),
            host
        );

        // A corrupted document or digest.
        let mut corrupted = encoding.clone();
        let last = corrupted.len() - 3;
        corrupted[last] ^= 1;
        assert_eq!(
            code(host_profile_for_restore(host.id(), &digest, &corrupted)),
            ProfileErrorCode::ProfileDigest
        );
        assert_eq!(
            code(host_profile_for_restore(
                host.id(),
                &digest[..31],
                &encoding
            )),
            ProfileErrorCode::ProfileDigest
        );
        // A document that is not canonical, with its own digest.
        let pretty = host.to_pretty_json().into_bytes();
        assert_eq!(
            code(host_profile_for_restore(
                host.id(),
                &canonical::sha256(&pretty),
                &pretty
            )),
            ProfileErrorCode::ProfileDigest
        );
        // A canonical document, with its own digest, of a profile that
        // violates the CPU time bits: a TSC-deadline timer.
        let mut deadline = host.clone();
        let leaf1 = deadline
            .cpuid
            .iter_mut()
            .find(|entry| entry.key() == (1, None))
            .unwrap();
        leaf1.value[2].0 |= 1 << 24;
        let encoding_with_deadline = deadline.encode();
        let error = host_profile_for_restore(
            host.id(),
            &canonical::sha256(&encoding_with_deadline),
            &encoding_with_deadline,
        )
        .unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileDigest);
        assert!(
            error
                .message
                .contains("violates the CPU time bits: CPUID 0x1 ECX bit 24 is set"),
            "{error}"
        );
        // A pinned profile's document under a host profile's ID.
        let pinned = pinned_record("intel.icelake-sp.v1").unwrap();
        assert_eq!(
            code(host_profile_for_restore(
                host.id(),
                &pinned.digest,
                pinned.encoding
            )),
            ProfileErrorCode::ProfileDigest
        );
        // A pinned profile's ID is not a host profile's.
        assert_eq!(
            code(host_profile_for_restore(
                pinned.id,
                &pinned.digest,
                pinned.encoding
            )),
            ProfileErrorCode::ProfileUnknown
        );
        // Nor is an ID with a malformed revision, whatever the document.
        for id in ["intel.host.v", "intel.host.v0", "intel.host.vfoo"] {
            assert_eq!(
                code(host_profile_for_restore(id, &digest, &encoding)),
                ProfileErrorCode::ProfileUnknown,
                "{id}"
            );
        }
    }
}
