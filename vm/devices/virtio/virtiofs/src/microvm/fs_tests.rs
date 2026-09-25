// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for microVM filesystem persistence and limits.

use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MICROVM_FUSE_MAJOR;
use super::profile::MICROVM_FUSE_MAX_WRITE;
use super::profile::MICROVM_FUSE_MIN_MINOR;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use super::saved_state::MAX_ALIASES_PER_INODE;
use super::saved_state::MAX_PATH_BYTES;
use super::state::encode_relative_path;
use super::state::fuse_negotiation_from_session;
use super::state::reopen_flags;
use super::state::validate_microvm_state;
use super::state::validate_relative_path;
use crate::VirtioFs;
use fuse::Session;
use fuse::SessionInfoState;
use fuse::SessionState;
use fuse::protocol::FUSE_ASYNC_READ;
use fuse::protocol::FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2;
use fuse::protocol::FUSE_INIT_EXT;
use fuse::protocol::FUSE_ROOT_ID;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;
use test_with_tracing::test;

fn profile(root_path: &Path) -> MicroVmVirtioFsProfile {
    MicroVmVirtioFsProfile::from_attachment(
        MICROVM_ATTACHMENT_ID.to_owned(),
        microvm_root_identity(root_path).unwrap(),
        true,
        Vec::new(),
    )
    .unwrap()
}

#[test]
fn microvm_state_restores_against_the_same_attachment() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    let destination = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let session = Session::new(destination.clone());

    destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap();
    assert!(destination.get_inode(FUSE_ROOT_ID).is_ok());
}

#[test]
fn initialized_microvm_negotiation_restores() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let session_state = SessionState {
        initialized: true,
        info: SessionInfoState {
            major: MICROVM_FUSE_MAJOR,
            minor: MICROVM_FUSE_MIN_MINOR,
            capable: FUSE_ASYNC_READ,
            want: FUSE_ASYNC_READ,
            max_write: MICROVM_FUSE_MAX_WRITE,
            time_gran: 1,
            ..Default::default()
        },
    };
    *source.inner.negotiation.write() = fuse_negotiation_from_session(session_state);
    let state = source.save_microvm_state(&profile, session_state).unwrap();

    let destination = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap();
    assert_eq!(session.save_state(), session_state);
    assert!(session.is_initialized());
}

#[test]
fn malformed_allocation_state_is_rejected() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    state.next_node_id = FUSE_ROOT_ID;

    assert!(validate_microvm_state(&state, &profile).is_err());
}

#[test]
fn saved_request_size_mismatch_is_rejected() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    state.maximum_request_size = 0;

    assert!(validate_microvm_state(&state, &profile).is_err());
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
fn microvm_denied_subtree_is_not_lookupable() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::create_dir(root_path.join("allowed")).unwrap();
    std::fs::create_dir(root_path.join("secrets")).unwrap();
    std::fs::write(root_path.join("secrets").join("token"), b"secret").unwrap();
    let profile = MicroVmVirtioFsProfile::from_attachment(
        MICROVM_ATTACHMENT_ID.to_owned(),
        microvm_root_identity(root_path).unwrap(),
        false,
        vec!["secrets".to_owned()],
    )
    .unwrap();
    let fs = VirtioFs::new_microvm(root_path, profile).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();

    let denied = match root.lookup_child(lx::LxStr::from_bytes(b"secrets")) {
        Err(error) => error,
        Ok(_) => panic!("denied subtree was lookupable"),
    };
    assert_eq!(denied, lx::Error::EACCES);
    root.lookup_child(lx::LxStr::from_bytes(b"allowed"))
        .unwrap();
    assert_eq!(
        root.child_path(lx::LxStr::from_bytes(b"..")).unwrap_err(),
        lx::Error::EINVAL
    );
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
fn saved_negotiation_must_match_the_exact_microvm_policy() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let session_state = SessionState {
        initialized: true,
        info: SessionInfoState {
            major: MICROVM_FUSE_MAJOR,
            minor: MICROVM_FUSE_MIN_MINOR,
            capable: FUSE_ASYNC_READ | FUSE_INIT_EXT,
            capable2: FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
            want: FUSE_ASYNC_READ | FUSE_INIT_EXT,
            want2: FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
            max_write: MICROVM_FUSE_MAX_WRITE,
            time_gran: 1,
            ..Default::default()
        },
    };
    *source.inner.negotiation.write() = fuse_negotiation_from_session(session_state);
    let mut state = source.save_microvm_state(&profile, session_state).unwrap();

    state.negotiation.want2 = 0;
    assert!(validate_microvm_state(&state, &profile).is_err());
    state.negotiation.want2 = FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2;
    state.negotiation.max_background = 1;
    assert!(validate_microvm_state(&state, &profile).is_err());
}

#[test]
fn reopen_flags_preserve_safe_status_bits_and_reject_effects() {
    const O_NONBLOCK: u32 = 0x800;
    const O_DSYNC: u32 = 0x1000;
    const O_CLOEXEC: u32 = 0x80000;
    const O_TMPFILE: u32 = 0x410000;

    let saved = lx::O_RDWR as u32
        | lx::O_APPEND as u32
        | O_NONBLOCK
        | O_DSYNC
        | O_CLOEXEC
        | lx::O_CREAT as u32
        | lx::O_EXCL as u32
        | lx::O_TRUNC as u32;
    let reopened = reopen_flags(saved).unwrap();
    assert_eq!(
        reopened,
        saved & !(lx::O_CREAT as u32 | lx::O_EXCL as u32 | lx::O_TRUNC as u32)
    );
    assert!(reopen_flags(O_TMPFILE).is_err());
    assert!(reopen_flags(0x80000000).is_err());
}

#[test]
fn readonly_profile_rejects_a_writable_saved_handle() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let root = source.get_inode(FUSE_ROOT_ID).unwrap();
    source
        .insert_file(Arc::clone(&root).open(lx::O_RDONLY as u32).unwrap())
        .unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    state.handles[0].open_flags = lx::O_RDWR as u32 | lx::O_NOFOLLOW as u32;

    assert!(validate_microvm_state(&state, &profile).is_err());
}

#[test]
fn saved_parent_alias_is_rejected_before_reopen() {
    let temporary_directory = tempdir().unwrap();
    let profile = profile(temporary_directory.path());
    let source = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    state.inodes[0].relative_aliases = vec![b"..".to_vec()];
    let destination = VirtioFs::new_microvm(temporary_directory.path(), profile.clone()).unwrap();
    let session = Session::new(destination.clone());

    assert!(
        destination
            .restore_microvm_state(&profile, state, &session)
            .is_err()
    );
}

#[test]
fn directory_snapshot_survives_host_mutation_after_restore() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::write(root_path.join("alpha"), b"alpha").unwrap();
    std::fs::write(root_path.join("beta"), b"beta").unwrap();
    let profile = profile(root_path);
    let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let root = source.get_inode(FUSE_ROOT_ID).unwrap();
    let handle = source
        .insert_file(Arc::clone(&root).open(lx::O_RDONLY as u32).unwrap())
        .unwrap();
    let file = source.get_file(handle).unwrap();

    let first_page = file.read_dir(&source, 0, 32, false).unwrap();
    assert!(!first_page.is_empty());
    let snapshot = file.directory_entries();
    assert!(snapshot.len() > 1);
    let continuation_offset = snapshot[0].next_cookie;
    assert_ne!(continuation_offset, 0);
    let expected_continuation = file
        .read_dir(&source, continuation_offset, 4096, false)
        .unwrap();
    let state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();

    std::fs::rename(root_path.join("beta"), root_path.join("00-beta")).unwrap();
    std::fs::write(root_path.join("later"), b"later").unwrap();

    let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap();
    let continuation = destination
        .get_file(handle)
        .unwrap()
        .read_dir(&destination, continuation_offset, 4096, false)
        .unwrap();
    assert_eq!(continuation, expected_continuation);
}

#[test]
fn hard_link_aliases_restore_and_reject_missing_or_replaced_names() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::write(root_path.join("first"), b"data").unwrap();
    std::fs::hard_link(root_path.join("first"), root_path.join("second")).unwrap();
    let profile = profile(root_path);
    let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let root = source.get_inode(FUSE_ROOT_ID).unwrap();
    let (_, first_node_id) = source
        .insert_inode(
            root.lookup_child(lx::LxStr::from_bytes(b"first"))
                .unwrap()
                .0,
        )
        .unwrap();
    let (_, second_node_id) = source
        .insert_inode(
            root.lookup_child(lx::LxStr::from_bytes(b"second"))
                .unwrap()
                .0,
        )
        .unwrap();
    assert_eq!(first_node_id, second_node_id);

    let missing_state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    let inode = missing_state
        .inodes
        .iter()
        .find(|inode| inode.node_id == first_node_id)
        .unwrap();
    assert_eq!(
        inode.relative_aliases,
        vec![
            encode_relative_path(Path::new("first")).unwrap(),
            encode_relative_path(Path::new("second")).unwrap(),
        ]
    );
    let replaced_state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();

    std::fs::remove_file(root_path.join("second")).unwrap();
    let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    assert!(
        destination
            .restore_microvm_state(&profile, missing_state, &session)
            .is_err()
    );

    std::fs::write(root_path.join("second"), b"replacement").unwrap();
    let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    assert!(
        destination
            .restore_microvm_state(&profile, replaced_state, &session)
            .is_err()
    );
}

#[test]
fn relative_path_validation_rejects_platform_escapes() {
    assert!(validate_relative_path(Path::new("../outside"), true).is_err());
    assert!(validate_relative_path(Path::new("/outside"), true).is_err());
    assert!(validate_relative_path(Path::new("alternate:name"), true).is_err());
}
