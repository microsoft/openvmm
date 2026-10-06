// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The pinned profiles as compile-time static data.
//!
//! `pinned_data.rs` holds every pinned profile as Rust constants: its values,
//! its canonical encoding, and its golden digest. The `generate_pinned`
//! example generates the file from `profiles/<id>.json`. An OpenVMM start
//! therefore neither parses JSON nor hashes: the first use of a profile
//! copies its constants into a [`CpuProfile`], and snapshot records use the
//! precomputed encoding and digest. The tests check every constant against
//! the JSON files and the golden digests.

use crate::Hex32;
use crate::Hex64;
use crate::cpuid::XsaveComponent;
use crate::derive::KnownGeneration;
use crate::profile::CpuProfile;
use crate::profile::CpuidLeafValue;
use crate::profile::PinnedMsr;
use crate::profile::Provenance;
use crate::profile::ProvenanceSource;
use crate::profile::SCHEMA;

/// A pinned profile as static data.
pub(crate) struct PinnedProfile {
    /// The profile ID.
    pub(crate) id: &'static str,
    /// The golden digest: the SHA-256 of `encoding`.
    pub(crate) digest: [u8; 32],
    /// The canonical encoding, compact canonical JSON.
    pub(crate) encoding: &'static str,
    /// The human-readable description.
    pub(crate) description: &'static str,
    /// The generation, which also gives the vendor.
    pub(crate) generation: &'static KnownGeneration,
    /// The CPUID table, sorted by leaf and subleaf.
    pub(crate) cpuid: &'static [CpuidLeafValue],
    /// The XCR0 bits the guest may enable.
    pub(crate) xcr0: u64,
    /// The IA32_XSS bits the guest may enable.
    pub(crate) xss: u64,
    /// The layout of every enabled XSAVE state component beyond x87 and SSE.
    pub(crate) xsave_components: &'static [XsaveComponent],
    /// The guest physical address width in bits.
    pub(crate) physical_address_width: u32,
    /// The pinned MSR values, sorted by index.
    pub(crate) msrs: &'static [PinnedMsr],
    /// How the profile was derived.
    pub(crate) provenance_method: &'static str,
    /// The host fingerprints it was derived from, by backend.
    pub(crate) provenance_sources: &'static [PinnedSource],
}

/// The fingerprints of one backend that a pinned profile was derived from.
pub(crate) struct PinnedSource {
    backend: &'static str,
    hosts: u32,
    surface_digests: &'static [&'static str],
}

impl PinnedProfile {
    /// Returns the profile, copied from the constants.
    pub(crate) fn to_profile(&self) -> CpuProfile {
        CpuProfile {
            cpuid: self.cpuid.to_vec(),
            description: self.description.to_owned(),
            generation: self.generation.generation(),
            id: self.id.to_owned(),
            msrs: self.msrs.to_vec(),
            physical_address_width: self.physical_address_width,
            provenance: Provenance {
                method: self.provenance_method.to_owned(),
                sources: self
                    .provenance_sources
                    .iter()
                    .map(|source| ProvenanceSource {
                        backend: source.backend.to_owned(),
                        hosts: source.hosts,
                        surface_digests: source
                            .surface_digests
                            .iter()
                            .map(|&digest| digest.to_owned())
                            .collect(),
                    })
                    .collect(),
            },
            schema: SCHEMA.to_owned(),
            vendor: self.generation.vendor.cpuid_vendor().to_owned(),
            xcr0: Hex64(self.xcr0),
            xsave_components: self.xsave_components.to_vec(),
            xss: Hex64(self.xss),
        }
    }

    /// Returns the profile's record.
    pub(crate) fn record(&self) -> PinnedRecord<'static> {
        PinnedRecord {
            id: self.id,
            encoding: self.encoding.as_bytes(),
            digest: self.digest,
        }
    }
}

/// The CPU profile record that a snapshot copies: the profile ID, the
/// canonical encoding that the snapshot embeds, and its SHA-256, the profile
/// digest. A pinned profile's record is precomputed at build time
/// ([`pinned_record`](crate::pinned_record)), so capture can record it, and
/// restore can compare a recorded profile with it, without encoding or
/// hashing. A host profile's is computed once, when the VM worker selects the
/// profile ([`PartitionProfile::record`](crate::PartitionProfile::record)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedRecord<'a> {
    /// The profile ID.
    pub id: &'a str,
    /// The canonical encoding, [`CpuProfile::encode`] of the profile.
    pub encoding: &'a [u8],
    /// The profile digest, the SHA-256 of `encoding`.
    pub digest: [u8; 32],
}

/// Returns a CPUID entry of a pinned profile.
pub(crate) const fn leaf(
    leaf: u32,
    subleaf: Option<u32>,
    value: [u32; 4],
    mask: [u32; 4],
) -> CpuidLeafValue {
    CpuidLeafValue {
        leaf: Hex32(leaf),
        mask: [
            Hex32(mask[0]),
            Hex32(mask[1]),
            Hex32(mask[2]),
            Hex32(mask[3]),
        ],
        subleaf: match subleaf {
            Some(subleaf) => Some(Hex32(subleaf)),
            None => None,
        },
        value: [
            Hex32(value[0]),
            Hex32(value[1]),
            Hex32(value[2]),
            Hex32(value[3]),
        ],
    }
}

/// Returns an XSAVE state component of a pinned profile.
pub(crate) const fn xsave(
    index: u32,
    size: u32,
    offset: u32,
    supervisor: bool,
    align64: bool,
    xfd: bool,
) -> XsaveComponent {
    XsaveComponent {
        align64,
        index,
        offset,
        size,
        supervisor,
        xfd,
    }
}

/// Returns a pinned MSR value of a pinned profile.
pub(crate) const fn msr(index: u32, value: u64, mask: u64) -> PinnedMsr {
    PinnedMsr {
        index: Hex32(index),
        mask: Hex64(mask),
        value: Hex64(value),
    }
}

/// Returns a provenance source of a pinned profile.
pub(crate) const fn source(
    backend: &'static str,
    hosts: u32,
    surface_digests: &'static [&'static str],
) -> PinnedSource {
    PinnedSource {
        backend,
        hosts,
        surface_digests,
    }
}

/// Returns the digest that `hex`, 64 lowercase hex digits, spells. Malformed
/// input fails the build.
pub(crate) const fn digest(hex: &str) -> [u8; 32] {
    const fn nibble(digit: u8) -> u8 {
        match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            _ => panic!("a digest is lowercase hex"),
        }
    }
    let hex = hex.as_bytes();
    assert!(hex.len() == 64, "a digest has 64 hex digits");
    let mut bytes = [0; 32];
    let mut i = 0;
    while i < 32 {
        bytes[i] = nibble(hex[2 * i]) << 4 | nibble(hex[2 * i + 1]);
        i += 1;
    }
    bytes
}
