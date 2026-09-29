// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Typed servicing DTOs, not evidence that a TCB was observed or accepted.
//!
//! Only transport these through an authenticated, VM-bound, fresh servicing
//! channel. The caller must establish authentication, VM binding, and freshness;
//! these messages provide none of them. Never restore them from VMGS, protector
//! headers, or other host-controlled metadata. They contain no keys or secrets.

use mesh::payload::Protobuf;

/// An accepted runtime floor transferred through trusted servicing state.
#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "underhill_attestation.runtime_sealing")]
pub struct SavedRuntimeTcbFloor {
    /// Schema version. Must be 1; absent (zero) and unknown versions fail restore.
    #[mesh(1)]
    pub version: u32,
    /// Required on restore. Optional on the wire to detect an absent variant.
    #[mesh(2)]
    pub svn: Option<SavedRuntimeTcbFloorSvn>,
}

/// Raw SVN and its exact comparison domain, without keys or a hardware report.
#[derive(Debug, Clone, PartialEq, Eq, Protobuf)]
#[mesh(package = "underhill_attestation.runtime_sealing")]
pub enum SavedRuntimeTcbFloorSvn {
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
    /// Preserve both 16-byte SVN arrays exactly, including reserved bytes.
    #[mesh(2)]
    Tdx {
        /// Exact 16-byte TEE TCB SVN, including module identity and reserved bytes.
        #[mesh(1)]
        tee_tcb_svn: Vec<u8>,
        /// Exact 16-byte CPU SVN.
        #[mesh(2)]
        cpu_svn: Vec<u8>,
    },
}
