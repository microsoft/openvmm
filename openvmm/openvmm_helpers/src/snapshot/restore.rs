// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reading VM snapshots from a directory.

use super::SnapshotManifest;
use anyhow::Context;
use std::path::Path;

/// Read a snapshot from the given directory.
///
/// Returns the decoded manifest and the raw saved-state bytes.
/// The caller is responsible for opening `memory.bin` separately.
pub fn read_snapshot(dir: &Path) -> anyhow::Result<(SnapshotManifest, Vec<u8>)> {
    let manifest_bytes =
        fs_err::read(dir.join("manifest.bin")).context("failed to read manifest.bin")?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(&manifest_bytes).context("failed to decode snapshot manifest")?;

    let state_bytes = fs_err::read(dir.join("state.bin")).context("failed to read state.bin")?;

    Ok((manifest, state_bytes))
}
