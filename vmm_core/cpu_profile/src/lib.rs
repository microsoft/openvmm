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
//! leaves.
//!
//! - **Selection:** [`select`] maps `--cpu-profile <id|auto>` and the host
//!   CPU ([`HostCpuSignature`]) to a pinned profile.
//! - **Verification:** each backend reports the CPU surface it supports
//!   ([`HostCpuSurface`]), and [`verify_support`] checks that it covers the
//!   profile. A pass-through backend (MSHV, WHP) must also present zero at
//!   every CPUID entry outside the profile's tables
//!   ([`check_unlisted_cpuid`]), which KVM does by construction
//!   ([`CpuidPresentation`]).
//! - **Restore:** [`restore_profile`] checks a snapshot's profile record,
//!   and [`EffectiveCpuid::check_matches`] its effective CPUID.
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
mod profile;
mod signature;
mod surface;
#[cfg(test)]
mod test_support;
mod unlisted;

pub use catalog::AUTO;
pub use catalog::check_generation;
pub use catalog::generation_of;
pub use catalog::pinned;
pub use catalog::pinned_for_restore;
pub use catalog::pinned_profiles;
pub use catalog::restore_profile;
pub use catalog::select;
pub use catalog::select_auto;
pub use catalog::verify_profile_record;
pub use check::FingerprintCheck;
pub use check::SUMMARY_PREFIX;
pub use check::check_fingerprint;
pub use effective::CpuidResult;
pub use effective::EFFECTIVE_CPUID_SCHEMA;
pub use effective::EffectiveCpuid;
pub use effective::x2apic_cpuid;
pub use error::ProfileError;
pub use error::ProfileErrorCode;
pub use hex::Hex32;
pub use hex::Hex64;
pub use hex::ParseHexError;
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
pub use unlisted::check_unlisted_cpuid;
pub use unlisted::unlisted_cpuid_candidates;
pub use unlisted::unlisted_cpuid_violations;
