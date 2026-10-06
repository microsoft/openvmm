// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86 CPU profiles for the NVX time ABI, and the host CPU fingerprints they
//! are derived from.
//!
//! A [`CpuProfile`] is the complete guest-visible CPU surface of one CPU
//! generation, shared by every hypervisor backend: every CPUID leaf a guest
//! can observe, the XSAVE features and layout, the guest physical address
//! width, and pinned MSR values. Profiles are pinned in this crate
//! ([`pinned`]), immutable once released, and identified by ID and SHA-256
//! digest. A VM's effective CPUID ([`CpuProfile::effective_cpuid`]) is a pure
//! function of its profile, its topology, and the time ABI's identity
//! leaves; OpenVMM builds it in `virt::time_abi::cpuid::effective_cpuid`.
//!
//! - **Selection:** [`select`] maps `--cpu-profile <id|auto>` and the host
//!   CPU ([`HostCpuSignature`]) to a pinned profile. `--cpu-profile host`
//!   ([`HOST`]) is the opt-in exception, for development hosts that no
//!   pinned profile serves ([`supports_host_profiles`]): the VM worker
//!   derives a host profile from the backend's fingerprint of this host
//!   ([`derive_host_profile`]). Either way, the partition holds its profile
//!   in a [`PartitionProfile`], which core hands to the backend, so each VM
//!   of a process can have its own host profile.
//! - **Verification:** each backend reports the CPU surface it supports
//!   ([`HostCpuSurface`]), and [`verify_support`] checks that it covers the
//!   profile. No CPUID entry outside the profile's tables carries host data:
//!   a pass-through backend (MSHV, WHP) must present zero there
//!   ([`check_unlisted_cpuid`]), and KVM answers those entries from the
//!   effective CPUID itself ([`CpuidPresentation`]).
//! - **Restore:** [`pinned_for_restore`] checks a snapshot's recorded profile
//!   digest against the pinned constant, core compares the recorded document
//!   with [`pinned_record`]'s encoding, and [`check_generation`] checks the
//!   destination host. Core compares the effective CPUID record with the
//!   recomputed [`EffectiveCpuid`]. A snapshot of a host profile carries the
//!   only copy of its document, which [`host_profile_for_restore`] decodes
//!   and checks against the recorded digest, and from which the restoring
//!   VM worker's [`PartitionProfile`] takes it.
//! - **Offline only:** the codecs and digests
//!   ([`CpuProfile::encode`], [`CpuProfile::digest`],
//!   [`CpuProfile::digest_string`], [`CpuProfile::decode`],
//!   [`CpuProfile::from_pretty_json`], [`CpuProfile::to_pretty_json`],
//!   [`EffectiveCpuid::encode`], [`EffectiveCpuid::digest`],
//!   [`EffectiveCpuid::decode`], and [`EffectiveCpuid::decode_verified`])
//!   and [`pinned_profiles`] parse, encode, or hash. They serve tools, tests,
//!   and `--cpu-fingerprint`, never a cold boot or restore of a pinned
//!   profile, whose profile work uses only constants. Only a host profile,
//!   which has no constants, encodes and hashes at cold boot and decodes at
//!   restore.
//!
//! Failures carry the stable codes of the time ABI specification
//! ([`ProfileError`]).
//!
//! A [`CpuFingerprint`](fingerprint::CpuFingerprint) records the CPU surface
//! that one backend supports on one host, with the host's identity, as
//! deterministic canonical JSON with SHA-256 digests. The
//! [`derive`](mod@derive) module intersects the fingerprints of a
//! generation's hosts into its profile, following the dump, strip, and verify
//! workflow of Firecracker's `cpu-template-helper`.

pub mod cpuid;
pub mod derive;
pub mod fingerprint;
pub mod host;
pub mod hv_banks;

mod canonical;
mod catalog;
mod check;
mod effective;
mod error;
mod hex;
mod pinned;
mod pinned_data;
mod profile;
mod signature;
mod surface;
#[cfg(test)]
mod test_support;
mod unlisted;
mod vendor;

pub use catalog::AUTO;
pub use catalog::HOST;
pub use catalog::PartitionProfile;
pub use catalog::check_generation;
pub use catalog::generation_of;
pub use catalog::host_profile_for_restore;
pub use catalog::is_host_profile_id;
pub use catalog::pinned;
pub use catalog::pinned_for_restore;
pub use catalog::pinned_profiles;
pub use catalog::pinned_record;
pub use catalog::select;
pub use catalog::select_auto;
pub use check::FingerprintCheck;
pub use check::SUMMARY_PREFIX;
pub use check::check_fingerprint;
pub use check::check_fingerprint_with;
pub use derive::derive_host_profile;
pub use derive::supports_host_profiles;
pub use effective::CpuidResult;
pub use effective::EFFECTIVE_CPUID_SCHEMA;
pub use effective::EffectiveCpuid;
pub use effective::x2apic_cpuid;
pub use error::ProfileError;
pub use error::ProfileErrorCode;
pub use hex::Hex32;
pub use hex::Hex64;
pub use hex::ParseHexError;
pub use pinned::PinnedRecord;
pub use profile::CpuProfile;
pub use profile::CpuidLeafValue;
pub use profile::Generation;
pub use profile::GenerationCpu;
pub use profile::PinnedMsr;
pub use profile::Provenance;
pub use profile::ProvenanceSource;
pub use profile::SCHEMA as PROFILE_SCHEMA;
pub use profile::VM_OWNED_LEAVES;
pub use profile::pinned_mask;
pub use profile::runtime_owned_bits;
pub use profile::vm_owned_bits;
pub use signature::HostCpuSignature;
pub use surface::CpuidPresentation;
pub use surface::HostCpuSurface;
pub use surface::SupportedMsr;
pub use surface::TIME_POLICY_BITS;
pub use surface::support_violations;
pub use surface::verify_support;
pub use surface::verify_support_with_unlisted;
pub use unlisted::check_unlisted_cpuid;
pub use unlisted::unlisted_cpuid_candidates;
pub use unlisted::unlisted_cpuid_violations;
pub use vendor::CpuVendor;
