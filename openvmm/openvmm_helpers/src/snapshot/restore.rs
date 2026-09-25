// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reading VM snapshots from a directory.

use super::SnapshotManifest;
use super::format::MAX_MANIFEST_SIZE_BYTES;
use super::format::MAX_SAVED_STATE_SIZE_BYTES;
use super::format::validate_manifest_header;
use anyhow::Context;
use std::io::Read;
use std::path::Path;

/// Read a snapshot from the given directory.
///
/// Returns the decoded manifest and the raw saved-state bytes. The manifest
/// and `state.bin` are read with bounded sizes, and `state.bin` must have the
/// length that the manifest records. The caller is responsible for validating
/// the manifest against the VM and for opening `memory.bin` separately.
pub fn read_snapshot(dir: &Path) -> anyhow::Result<(SnapshotManifest, Vec<u8>)> {
    let manifest_bytes = read_bounded(
        &dir.join("manifest.bin"),
        MAX_MANIFEST_SIZE_BYTES,
        "manifest.bin",
    )?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(&manifest_bytes).context("failed to decode snapshot manifest")?;
    validate_manifest_header(&manifest)?;
    anyhow::ensure!(
        manifest.state_size_bytes <= MAX_SAVED_STATE_SIZE_BYTES,
        "state.bin exceeds the maximum size of {MAX_SAVED_STATE_SIZE_BYTES} bytes"
    );

    let state_bytes = read_bounded(
        &dir.join("state.bin"),
        manifest.state_size_bytes,
        "state.bin",
    )?;
    anyhow::ensure!(
        state_bytes.len() as u64 == manifest.state_size_bytes,
        "state.bin length {} does not match the manifest length {}",
        state_bytes.len(),
        manifest.state_size_bytes,
    );

    Ok((manifest, state_bytes))
}

/// Reads at most `limit` bytes of the file at `path`, failing if it is longer.
fn read_bounded(path: &Path, limit: u64, description: &str) -> anyhow::Result<Vec<u8>> {
    let file = fs_err::File::open(path).with_context(|| format!("failed to open {description}"))?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {description}"))?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "{description} exceeds the maximum size of {limit} bytes"
    );
    Ok(bytes)
}
