// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Non-Windows stub for the [`soft_lp`](super::soft_lp) module.
//!
//! Soft large pages are a Windows-only optimization (see the Windows module for
//! the full scheme), so on every other target [`SoftLp`] is uninhabited:
//! [`SoftLp::new`] always returns `None`, so the value is named in the VA
//! mapper's per-mapping properties (`Option<SoftLp>`) but never constructed, and
//! every method is statically unreachable. Providing this stub — rather than
//! `cfg`-ing each use site — keeps the parent module free of platform gates.

use super::super::manager::MemoryPolicy;
use inspect::Inspect;
use memory_range::MemoryRange;
use sparse_mmap::SparseMapping;

/// Uninhabited stand-in for the Windows `SoftLp`: never constructed off Windows.
#[derive(Debug)]
pub(super) enum SoftLp {}

impl SoftLp {
    /// Soft large pages never apply off Windows, so this always returns `None`.
    pub(super) fn new(
        _range: MemoryRange,
        _policy: &MemoryPolicy,
        _writable: bool,
        _primary: bool,
    ) -> Option<Self> {
        None
    }

    /// Unreachable: `SoftLp` is never constructed off Windows.
    pub(super) fn deferred_protect(&self) -> bool {
        match *self {}
    }

    /// Unreachable: `SoftLp` is never constructed off Windows.
    pub(super) fn resolve(
        &self,
        _mapping: &SparseMapping,
        _fault: MemoryRange,
        _write: bool,
        _start: u64,
        _end: u64,
    ) -> Result<MemoryRange, RaiseError> {
        match *self {}
    }
}

impl Inspect for SoftLp {
    fn inspect(&self, _req: inspect::Request<'_>) {
        match *self {}
    }
}

/// Uninhabited error mirror: the stub's [`SoftLp::resolve`] is unreachable, so a
/// `RaiseError` is never produced, but the VA mapper's fault path names the type
/// in its error mapping.
#[derive(Debug)]
pub(super) enum RaiseError {}

impl std::fmt::Display for RaiseError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

impl std::error::Error for RaiseError {}
