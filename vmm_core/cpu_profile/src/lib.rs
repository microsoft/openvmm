// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86 CPU fingerprints, the input to pinned guest CPU profiles.
//!
//! A [`CpuFingerprint`](fingerprint::CpuFingerprint) records the guest CPU
//! surface that one hypervisor backend supports on one host: every CPUID leaf
//! and subleaf that a guest could be given, the XSAVE features and layout, the
//! feature MSRs, and the time capabilities that the NVX time ABI depends on,
//! together with the identity of the host CPU and OS.
//!
//! Fingerprints are emitted as deterministic, canonical JSON with SHA-256
//! digests, so that the fingerprints of a fleet of hosts can be compared and
//! intersected into per-generation CPU profiles. This follows the dump, strip,
//! and verify workflow of Firecracker's `cpu-template-helper`.

pub mod cpuid;
pub mod fingerprint;
pub mod host;

mod canonical;
mod hex;

pub use hex::Hex32;
pub use hex::Hex64;
pub use hex::ParseHexError;
