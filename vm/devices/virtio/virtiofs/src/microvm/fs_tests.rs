// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for microVM filesystem limits.

use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use super::saved_state::MAX_ALIASES_PER_INODE;
use super::saved_state::MAX_PATH_BYTES;
use super::state::validate_relative_path;
use crate::VirtioFs;
use fuse::protocol::FUSE_ROOT_ID;
use std::path::Path;
use std::path::PathBuf;
use tempfile::tempdir;
use test_with_tracing::test;

fn profile(root_path: &Path) -> MicroVmVirtioFsProfile {
    MicroVmVirtioFsProfile::from_attachment(
        MICROVM_ATTACHMENT_ID.to_owned(),
        microvm_root_identity(root_path).unwrap(),
        true,
    )
    .unwrap()
}

#[test]
fn microvm_runtime_path_and_map_limits_are_enforced() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let fs = VirtioFs::new_microvm(temporary_directory.path(), profile).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let oversized_name = vec![b'x'; MAX_PATH_BYTES + 1];
    assert_eq!(
        root.lookup_child(lx::LxStr::from_bytes(&oversized_name))
            .err()
            .unwrap(),
        lx::Error::E2BIG
    );

    fs.inner.inodes.write().inodes_by_node_id.next_handle = 0;
    assert_eq!(
        fs.preflight_new_inode_path(Path::new("new-entry"))
            .unwrap_err(),
        lx::Error::ENOSPC
    );
    fs.inner.files.write().next_handle = 0;
    assert_eq!(fs.preflight_file_insert().unwrap_err(), lx::Error::ENOSPC);
}

#[test]
fn microvm_alias_limit_is_enforced_before_linking() {
    let temporary_directory = tempdir().unwrap();
    std::fs::write(temporary_directory.path().join("entry"), b"data").unwrap();
    let profile = profile(temporary_directory.path());
    let fs = VirtioFs::new_microvm(temporary_directory.path(), profile).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let (inode, _) = fs
        .insert_inode(
            root.lookup_child(lx::LxStr::from_bytes(b"entry"))
                .unwrap()
                .0,
        )
        .unwrap();
    for index in 1..MAX_ALIASES_PER_INODE {
        inode.add_alias(PathBuf::from(format!("alias-{index}")));
    }
    assert_eq!(
        fs.preflight_alias_add(&inode, Path::new("alias-overflow"))
            .unwrap_err(),
        lx::Error::ENOSPC
    );
}

#[test]
fn relative_path_validation_rejects_platform_escapes() {
    assert!(validate_relative_path(Path::new("../outside"), true).is_err());
    assert!(validate_relative_path(Path::new("/outside"), true).is_err());
    assert!(validate_relative_path(Path::new("alternate:name"), true).is_err());
}
