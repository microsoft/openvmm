// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed servicing DTOs, not evidence that a TCB was observed or accepted.
//!
//! Only transport these through an authenticated, VM-bound, fresh servicing
//! channel. The caller must establish authentication, VM binding, and freshness;
//! these messages provide none of them. Never restore them from VMGS, protector
//! headers, or other host-controlled metadata. They contain no keys or secrets.

use mesh::payload::Protobuf;

/// Initialized runtime sealing state transferred through trusted servicing.
#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "underhill_attestation.runtime_sealing")]
pub struct SavedRuntimeSealingState {
    /// Schema version. Must be 1; absent (zero) and unknown versions fail restore.
    #[mesh(1)]
    pub version: u32,
    /// Required on restore. Optional on the wire to detect an absent variant.
    #[mesh(2)]
    pub tee: Option<SavedRuntimeSealingTee>,
}

/// SNP floor metadata or initialized TDX state, without keys or a report.
#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "underhill_attestation.runtime_sealing")]
pub enum SavedRuntimeSealingTee {
    /// Preserve every raw TCB byte, including reserved bytes and unknown domains.
    #[mesh(1)]
    Snp {
        /// Complete raw reported TCB, with no component normalization.
        #[mesh(1)]
        tcb_version: u64,
        /// Exact report version, including equality-only unknown versions.
        #[mesh(2)]
        report_version: u32,
        /// Absent below report version 3; exactly [family, model] otherwise.
        #[mesh(3)]
        cpuid: Option<Vec<u8>>,
    },
    /// TDX retains no historical SVN; its TCB policy belongs to admission.
    #[mesh(2)]
    Tdx,
}
