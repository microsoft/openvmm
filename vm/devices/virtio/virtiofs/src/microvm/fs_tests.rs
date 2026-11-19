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
use super::saved_state::MAX_INODES;
use super::saved_state::MAX_PATH_BYTES;
use super::state::encode_relative_path;
use super::state::fuse_negotiation_from_session;
use super::state::reopen_flags;
use super::state::validate_microvm_state;
use super::state::validate_relative_path;
use crate::InodeMap;
use crate::VirtioFs;
use crate::inode::VirtioFsInode;
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

/// Returns a microVM filesystem of `root_path` whose root holds the regular
/// file `file`, which the guest has looked up, with the file's inode and node
/// ID.
fn microvm_with_a_tracked_file(root_path: &Path) -> (VirtioFs, Arc<VirtioFsInode>, u64) {
    std::fs::write(root_path.join("file"), b"data").unwrap();
    let fs = VirtioFs::new_microvm(root_path, profile(root_path)).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let (file, _) = root.lookup_child(lx::LxStr::from_bytes(b"file")).unwrap();
    let (file, node_id) = fs.insert_inode(file).unwrap();
    (fs, file, node_id)
}

/// Returns an inode for a directory at `path` with the host inode number of
/// `file`, as a host reports once it reuses the number of a removed file.
fn directory_with_the_inode_number_of(file: &VirtioFsInode, path: &str) -> VirtioFsInode {
    let mut stat = file.volume().lstat(file.clone_path()).unwrap();
    stat.mode = lx::S_IFDIR | 0o755;
    let directory = VirtioFsInode::with_attr(file.volume(), PathBuf::from(path), &stat);
    assert!(directory.dedup_key() == file.dedup_key());
    directory
}

/// Tracks regular files under dedup keys of their own, as lookups of other
/// host files do, until `fs` tracks `MAX_INODES` inodes.
fn track_inodes_up_to_the_limit(fs: &VirtioFs, file: &VirtioFsInode) {
    let mut stat = file.volume().lstat(file.clone_path()).unwrap();
    let mut inodes = fs.inner.inodes.write();
    let mut inode_nr = u64::MAX;
    while inodes.inodes_by_node_id.values.len() < MAX_INODES {
        stat.inode_nr = inode_nr;
        let filler = VirtioFsInode::with_attr(
            file.volume(),
            PathBuf::from(format!("filler-{inode_nr}")),
            &stat,
        );
        if !inodes.inodes_by_key.contains_key(&filler.dedup_key()) {
            inodes.insert(filler).unwrap();
        }
        inode_nr -= 1;
    }
}

/// Returns the node ID that `inodes` deduplicates the host object of `inode`
/// to, if any.
fn deduplicated_node_id(inodes: &InodeMap, inode: &VirtioFsInode) -> Option<u64> {
    inodes
        .inodes_by_key
        .get(&inode.dedup_key())
        .map(|&(_, node_id)| node_id)
}

#[test]
fn retyped_object_is_refused_at_the_inode_limit() {
    // A host object of another file type under a tracked inode's dedup key,
    // such as a directory that reuses the inode number of a removed file,
    // takes a node of its own (see `InodeMap::dedup_entry`), at the tracked
    // inode's path or another, so the inode limit applies to it.
    for path in ["file", "other"] {
        let temporary_directory = tempdir().unwrap();
        let (fs, file, node_id) = microvm_with_a_tracked_file(temporary_directory.path());
        track_inodes_up_to_the_limit(&fs, &file);

        let directory = directory_with_the_inode_number_of(&file, path);
        assert_eq!(
            fs.preflight_inode_insert(&directory).unwrap_err(),
            lx::Error::ENOSPC
        );
        assert!(matches!(
            fs.insert_inode(directory),
            Err(error) if error == lx::Error::ENOSPC
        ));
        let reservation = fs.reserve_inode().unwrap();
        assert!(matches!(
            reservation.insert(directory_with_the_inode_number_of(&file, path)),
            Err(error) if error == lx::Error::ENOSPC
        ));
        let inodes = fs.inner.inodes.read();
        assert_eq!(inodes.inodes_by_node_id.values.len(), MAX_INODES);
        assert_eq!(deduplicated_node_id(&inodes, &file), Some(node_id));
    }
}

#[test]
fn retyped_object_is_refused_without_a_free_node_id() {
    // Such an object needs a free node ID too, and its refusal leaves the
    // tracked inode under its dedup key.
    for path in ["file", "other"] {
        let temporary_directory = tempdir().unwrap();
        let (fs, file, node_id) = microvm_with_a_tracked_file(temporary_directory.path());
        fs.inner.inodes.write().inodes_by_node_id.next_handle = 0;

        let directory = directory_with_the_inode_number_of(&file, path);
        assert_eq!(
            fs.preflight_inode_insert(&directory).unwrap_err(),
            lx::Error::ENOSPC
        );
        assert!(matches!(
            fs.insert_inode(directory),
            Err(error) if error == lx::Error::ENOSPC
        ));
        assert_eq!(
            deduplicated_node_id(&fs.inner.inodes.read(), &file),
            Some(node_id)
        );
    }
}

#[test]
fn retyped_object_is_not_bound_by_the_alias_limit_of_the_tracked_inode() {
    // Such an object adds no alias to the tracked inode, so the tracked
    // inode's full alias set does not refuse it.
    for path in ["file", "other"] {
        let temporary_directory = tempdir().unwrap();
        let (fs, file, node_id) = microvm_with_a_tracked_file(temporary_directory.path());
        for index in 1..MAX_ALIASES_PER_INODE {
            file.add_alias(PathBuf::from(format!("alias-{index}")));
        }

        let directory = directory_with_the_inode_number_of(&file, path);
        fs.preflight_inode_insert(&directory).unwrap();
        let (directory, directory_node_id) = fs.insert_inode(directory).unwrap();
        assert_ne!(directory_node_id, node_id);
        assert_eq!(directory.aliases(), [PathBuf::from(path)]);
        assert_eq!(file.aliases().len(), MAX_ALIASES_PER_INODE);
        let inodes = fs.inner.inodes.read();
        assert!(Arc::ptr_eq(&inodes.get(node_id).unwrap(), &file));
        assert_eq!(
            deduplicated_node_id(&inodes, &file),
            Some(directory_node_id)
        );
    }
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
        .insert_file(Arc::clone(&root).open(lx::O_RDONLY).unwrap())
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
        .insert_file(Arc::clone(&root).open(lx::O_RDONLY).unwrap())
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

/// Objects with one inode number on two devices, as `/proc` and `/sys` are,
/// save as two inodes and restore as two.
#[cfg(target_os = "linux")]
#[test]
fn one_inode_number_on_two_devices_restores_as_two_inodes() {
    crate::test_helpers::assert_proc_and_sys_share_an_inode_number();
    let root_path = Path::new("/");
    let profile = profile(root_path);
    let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let root = source.get_inode(FUSE_ROOT_ID).unwrap();
    let names = ["proc", "sys"];
    let node_ids = names.map(|name| {
        source
            .lookup_helper(&root, lx::LxStr::from_bytes(name.as_bytes()))
            .unwrap()
            .nodeid
    });
    assert_ne!(node_ids[0], node_ids[1]);
    let state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();

    let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap();
    for (name, node_id) in names.into_iter().zip(node_ids) {
        assert_eq!(
            destination.get_inode(node_id).unwrap().clone_path(),
            Path::new(name)
        );
    }
}

#[test]
fn relative_path_validation_rejects_platform_escapes() {
    assert!(validate_relative_path(Path::new("../outside"), true).is_err());
    assert!(validate_relative_path(Path::new("/outside"), true).is_err());
    assert!(validate_relative_path(Path::new("alternate:name"), true).is_err());
}

#[test]
fn restore_rejects_a_guest_inode_number_of_another_object() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::write(root_path.join("file"), b"data").unwrap();
    let profile = profile(root_path);
    let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let root = source.get_inode(FUSE_ROOT_ID).unwrap();
    let file = source
        .lookup_helper(&root, lx::LxStr::from_bytes(b"file"))
        .unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    let saved = state
        .inodes
        .iter_mut()
        .find(|inode| inode.node_id == file.nodeid)
        .unwrap();
    assert_eq!(saved.guest_inode_id, file.attr.ino);
    saved.guest_inode_id ^= 1;

    let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    let error = destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("saved guest inode ID does not match the attachment"),
        "{error:#}"
    );
}

/// A release that numbered objects by inode number alone saved the objects of
/// host devices other than the share's root, such as below a mount, and the
/// listings of their directories, without the devices' keys. Its snapshot
/// must restore with those numbers, and the share must then keep numbering
/// every object so, as the guest knows them, so that the listings stay valid.
/// A snapshot cannot number objects both ways, and one with keys restores
/// with keys.
#[cfg(target_os = "linux")]
#[test]
fn snapshot_without_device_keys_restores_its_numbers() {
    use std::collections::BTreeMap;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::DirEntryExt;
    use std::os::unix::fs::MetadataExt;

    // `/` reaches sysfs and procfs, which are separate host devices.
    crate::test_helpers::assert_proc_and_sys_share_an_inode_number();
    let root_path = Path::new("/");
    let profile = profile(root_path);
    let host = |path: &str| std::fs::symlink_metadata(path).unwrap();
    let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let root = source.get_inode(FUSE_ROOT_ID).unwrap();
    let sys = source
        .lookup_helper(&root, lx::LxStr::from_bytes(b"sys"))
        .unwrap();
    let sys_inode = source.get_inode(sys.nodeid).unwrap();
    let kernel = source
        .lookup_helper(&sys_inode, lx::LxStr::from_bytes(b"kernel"))
        .unwrap();
    let handle = source
        .insert_file(sys_inode.open(lx::O_RDONLY | lx::O_DIRECTORY).unwrap())
        .unwrap();
    source
        .get_file(handle)
        .unwrap()
        .read_dir(&source, 0, 4096, false)
        .unwrap();
    // The entries of `/sys` with their host inode numbers, as a release
    // without device keys listed them.
    let mut listed: BTreeMap<_, _> = std::fs::read_dir("/sys")
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name().into_vec(), entry.ino())
        })
        .collect();
    listed.insert(b".".to_vec(), host("/sys").ino());
    listed.insert(b"..".to_vec(), host("/sys").ino());
    // Saves the share with the given sysfs nodes, and the listing of `/sys`,
    // numbered without their device's key.
    let save_unkeyed = |node_ids: &[u64]| {
        let mut state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        for inode in &mut state.inodes {
            if node_ids.contains(&inode.node_id) {
                assert_ne!(inode.guest_inode_id, inode.object_identity.inode_id);
                inode.guest_inode_id = inode.object_identity.inode_id;
            }
        }
        let saved = state
            .handles
            .iter_mut()
            .find(|saved| saved.handle_id == handle)
            .unwrap();
        assert_eq!(saved.directory_entries.len(), listed.len());
        for entry in &mut saved.directory_entries {
            entry.guest_inode_id = listed[&entry.name];
        }
        state
    };
    let restore = |state| {
        let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let session = Session::new(destination.clone());
        destination
            .restore_microvm_state(&profile, state, &session)
            .map(|()| destination)
    };

    let error = restore(save_unkeyed(&[sys.nodeid])).err().unwrap();
    assert!(
        format!("{error:#}").contains("both with and without device keys"),
        "{error:#}"
    );

    let destination = restore(save_unkeyed(&[sys.nodeid, kernel.nodeid])).unwrap();
    for (node_id, path) in [(sys.nodeid, "/sys"), (kernel.nodeid, "/sys/kernel")] {
        let inode = destination.get_inode(node_id).unwrap();
        assert_eq!(inode.get_attr().unwrap().ino, host(path).ino());
    }
    // READDIRPLUS looks the listed entries up again, numbered as listed,
    // except for a mount point, whose lookup reaches another device.
    let entries = crate::tests::read_directory(
        &destination,
        &destination.get_file(handle).unwrap(),
        0,
        4096,
        true,
    );
    assert_eq!(entries.len(), listed.len());
    let sysfs = host("/sys").dev();
    for (name, (ino, _)) in entries {
        let listed_ino = listed[name.as_bytes()];
        if name == "."
            || name == ".."
            || std::fs::symlink_metadata(Path::new("/sys").join(&name))
                .unwrap()
                .dev()
                == sysfs
        {
            assert_eq!(ino, listed_ino, "{name}");
        }
    }
    // Objects of devices that the snapshot does not hold get no keys either.
    let root = destination.get_inode(FUSE_ROOT_ID).unwrap();
    let proc = destination
        .lookup_helper(&root, lx::LxStr::from_bytes(b"proc"))
        .unwrap();
    assert_eq!(proc.attr.ino, host("/proc").ino());

    let destination = restore(
        source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap(),
    )
    .unwrap();
    let root = destination.get_inode(FUSE_ROOT_ID).unwrap();
    let proc = destination
        .lookup_helper(&root, lx::LxStr::from_bytes(b"proc"))
        .unwrap();
    assert_ne!(proc.attr.ino, host("/proc").ino());
}

/// The host lists a mount point, such as `/proc` in `/`, by the inode number
/// of the directory that the mount covers, while a lookup reaches the mounted
/// root on another device. READDIRPLUS must still list it, with the number
/// that the lookup reports, which tells mounted roots apart.
#[cfg(target_os = "linux")]
#[test]
fn readdirplus_lists_mount_points() {
    crate::test_helpers::assert_proc_and_sys_share_an_inode_number();
    let root_path = Path::new("/");
    let fs = VirtioFs::new_microvm(root_path, profile(root_path)).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let handle = fs
        .insert_file(
            Arc::clone(&root)
                .open(lx::O_RDONLY | lx::O_DIRECTORY)
                .unwrap(),
        )
        .unwrap();
    let entries = crate::tests::read_directory(&fs, &fs.get_file(handle).unwrap(), 0, 4096, true);
    for name in ["proc", "sys"] {
        let lookup = fs
            .lookup_helper(&root, lx::LxStr::from_bytes(name.as_bytes()))
            .unwrap();
        assert_eq!(entries[name].0, lookup.attr.ino, "{name}");
    }
    assert_ne!(entries["proc"].0, entries["sys"].0);
}
