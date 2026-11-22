// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for microVM filesystem persistence and limits.

use super::policy_tests::name;
use super::policy_tests::request;
use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MICROVM_FUSE_MAJOR;
use super::profile::MICROVM_FUSE_MAX_WRITE;
use super::profile::MICROVM_FUSE_MIN_MINOR;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use super::saved_state::MAX_ALIASES_PER_INODE;
use super::saved_state::MAX_DIRECTORY_ENTRIES;
use super::saved_state::MAX_INODES;
use super::saved_state::MAX_PATH_BYTES;
use super::state::encode_relative_path;
use super::state::fuse_negotiation_from_session;
use super::state::reopen_flags;
use super::state::validate_microvm_state;
use super::state::validate_relative_path;
use crate::InodeMap;
use crate::VirtioFs;
use crate::file::VirtioFsFile;
use crate::inode::VirtioFsInode;
use crate::test_helpers::decode_dir_entries;
use fuse::Fuse;
use fuse::Session;
use fuse::SessionInfoState;
use fuse::SessionState;
use fuse::protocol::FUSE_ASYNC_READ;
use fuse::protocol::FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2;
use fuse::protocol::FUSE_INIT_EXT;
use fuse::protocol::FUSE_ROOT_ID;
use fuse::protocol::fuse_dirent_align;
use fuse::protocol::fuse_direntplus;
use fuse::protocol::fuse_read_in;
use fuse::protocol::fuse_release_in;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::tempdir;
use test_with_tracing::test;
use zerocopy::FromZeros;

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

/// A saved state may hold the largest lookup count. Each further reference
/// that the guest gets for the node, from a lookup or a hard link, must leave
/// the count there rather than wrap it to zero, so that a forget cannot drop
/// the node while the guest still references it.
#[test]
fn largest_restored_lookup_count_saturates() {
    for link in [false, true] {
        let temporary_directory = tempdir().unwrap();
        let root_path = temporary_directory.path();
        std::fs::write(root_path.join("file"), b"data").unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(root_path).unwrap(),
            false,
            Vec::new(),
        )
        .unwrap();
        let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let node_id = source
            .lookup(&request(FUSE_ROOT_ID), name("file"))
            .unwrap()
            .nodeid;
        let mut state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        state
            .inodes
            .iter_mut()
            .find(|inode| inode.node_id == node_id)
            .unwrap()
            .lookup_count = u64::MAX;
        let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
        let session = Session::new(destination.clone());
        destination
            .restore_microvm_state(&profile, state, &session)
            .unwrap();

        let entry = if link {
            destination.link(&request(FUSE_ROOT_ID), name("link"), node_id)
        } else {
            destination.lookup(&request(FUSE_ROOT_ID), name("file"))
        };
        assert_eq!(entry.unwrap().nodeid, node_id);
        destination.forget(node_id, 1);

        destination.get_attr(&request(node_id), 0, 0).unwrap();
        let state = destination
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        assert!(state.inodes.iter().any(|inode| inode.node_id == node_id));
    }
}

/// A saved state may hold the largest next node ID, `u64::MAX`, which no node
/// takes: the node table is then full, so a lookup of an object that has no
/// node yet fails with `ENOSPC`, and the state saves with the same next node
/// ID, which a restore accepts, rather than one wrapped to zero.
#[test]
fn largest_restored_next_node_id_is_never_handed_out() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::write(root_path.join("file"), b"data").unwrap();
    let profile = profile(root_path);
    let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    state.next_node_id = u64::MAX;
    let destination = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap();

    assert_eq!(
        destination
            .lookup(&request(FUSE_ROOT_ID), name("file"))
            .map(|entry| entry.nodeid),
        Err(lx::Error::ENOSPC)
    );

    let state = destination
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    assert_eq!(state.next_node_id, u64::MAX);
    let restored = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(restored.clone());
    restored
        .restore_microvm_state(&profile, state, &session)
        .unwrap();
}

/// A reset or `FUSE_DESTROY` ends a restored session as it ends a new one, so
/// a remounting guest gets nodes from 2 and handles from 1 again, even after a
/// restore whose tables were full.
#[test]
fn destroy_restarts_restored_ids_from_the_start() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::write(root_path.join("file"), b"data").unwrap();
    let profile = profile(root_path);
    let source = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let node_id = source
        .lookup(&request(FUSE_ROOT_ID), name("file"))
        .unwrap()
        .nodeid;
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
    let mut state = source.save_microvm_state(&profile, session_state).unwrap();
    state.next_node_id = u64::MAX;
    state.next_handle_id = u64::MAX;
    let fs = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    let session = Session::new(fs.clone());
    fs.restore_microvm_state(&profile, state, &session).unwrap();
    assert_eq!(
        fs.open(&request(node_id), lx::O_RDONLY as u32)
            .map(|open| open.fh),
        Err(lx::Error::ENOSPC)
    );

    // As a device reset or a `FUSE_DESTROY` does.
    session.destroy();

    let node_id = fs
        .lookup(&request(FUSE_ROOT_ID), name("file"))
        .unwrap()
        .nodeid;
    assert_eq!(node_id, FUSE_ROOT_ID + 1);
    assert_eq!(
        fs.open(&request(node_id), lx::O_RDONLY as u32)
            .map(|open| open.fh),
        Ok(1)
    );
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

/// Returns a microVM filesystem of `root_path`, whose root holds the regular
/// files `names`, with an open handle of the root.
fn microvm_with_an_open_root(root_path: &Path, names: &[&str]) -> (VirtioFs, Arc<VirtioFsFile>) {
    for name in names {
        std::fs::write(root_path.join(name), name).unwrap();
    }
    let fs = VirtioFs::new_microvm(root_path, profile(root_path)).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let handle = fs.insert_file(root.open(lx::O_RDONLY).unwrap()).unwrap();
    let directory = fs.get_file(FUSE_ROOT_ID, handle).unwrap();
    (fs, directory)
}

/// Each READDIRPLUS entry other than `.` and `..` hands the guest a lookup
/// reference, which the guest can release only if it receives the entry. A
/// READDIRPLUS that fails at an entry, here because the inode limit refuses
/// the entry's node, must therefore return the entries that it has looked up,
/// and fail only the next read, which starts at the failing entry. Once the
/// guest forgets those entries, the listing goes on.
#[test]
fn readdirplus_stopped_by_the_inode_limit_returns_the_entries_it_looked_up() {
    let temporary_directory = tempdir().unwrap();
    let names = ["first", "second"];
    let (fs, directory) = microvm_with_an_open_root(temporary_directory.path(), &names);
    // Leave room for one more node.
    track_inodes_up_to_the_limit(&fs, directory.inode());
    let filler = *fs
        .inner
        .inodes
        .read()
        .inodes_by_node_id
        .values
        .keys()
        .max()
        .unwrap();
    fs.forget(filler, 1);

    let entries = decode_dir_entries(&directory.read_dir(&fs, 0, 4096, true).unwrap());
    let children: Vec<_> = entries
        .iter()
        .filter(|entry| entry.name != "." && entry.name != "..")
        .collect();
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_ne!(child.node_id, 0);
    let offset = entries.last().unwrap().offset;
    assert_eq!(
        directory.read_dir(&fs, offset, 4096, true).unwrap_err(),
        lx::Error::ENOSPC
    );

    fs.forget(child.node_id, 1);
    let rest = decode_dir_entries(&directory.read_dir(&fs, offset, 4096, true).unwrap());
    let rest: Vec<_> = rest
        .iter()
        .map(|entry| entry.name.as_str())
        .filter(|name| *name != "." && *name != "..")
        .collect();
    let other = names.into_iter().find(|name| *name != child.name).unwrap();
    assert_eq!(rest, [other]);
}

/// A READDIRPLUS that fails at a listed name that no longer reaches the
/// listed object, here because a new file replaced it, must leave the guest
/// every lookup reference that it took: once the guest forgets each entry that
/// it received, only the root's node remains.
#[test]
fn readdirplus_of_a_replaced_entry_keeps_no_reference_the_guest_lacks() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    let (fs, directory) = microvm_with_an_open_root(root_path, &["first", "second", "third"]);
    // A plain READDIR lists the directory without looking its entries up.
    directory.read_dir(&fs, 0, 4096, false).unwrap();
    // Replace the last listed entry, which READDIRPLUS reaches after it looks
    // the others up, with a file that exists alongside it until the rename,
    // so that the host numbers the two differently.
    let replaced = directory
        .directory_entries()
        .into_iter()
        .map(|entry| entry.name)
        .rfind(|name| name != b"." && name != b"..")
        .unwrap();
    let replaced = root_path.join(String::from_utf8(replaced).unwrap());
    std::fs::write(root_path.join("replacement"), "replacement").unwrap();
    std::fs::rename(root_path.join("replacement"), replaced).unwrap();

    // Read on as the guest does, until the listing ends or fails, and forget
    // each entry received.
    let mut offset = 0;
    while let Ok(reply) = directory.read_dir(&fs, offset, 4096, true) {
        let entries = decode_dir_entries(&reply);
        let Some(last) = entries.last() else {
            break;
        };
        offset = last.offset;
        for entry in entries {
            if entry.node_id != 0 {
                fs.forget(entry.node_id, 1);
            }
        }
    }
    assert_eq!(fs.inner.inodes.read().inodes_by_node_id.values.len(), 1);
}

/// Another guest process or the host may unlink entries of a directory while
/// the guest lists it, a page at a time. READDIRPLUS looks each entry of the
/// directory's snapshot up, which fails with ENOENT for an unlinked one, so it
/// must skip the entry, which POSIX lets a listing omit, rather than fail:
/// every later read would start at the entry and fail too, so the guest could
/// never list the rest of the directory. Here the next read starts at an
/// unlinked entry, and reaches another one later.
#[test]
fn readdirplus_skips_entries_unlinked_during_the_listing() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    // Enough files for several of the guest's 4 KiB pages.
    let names: Vec<_> = (0..60).map(|index| format!("f{index:02}")).collect();
    let names: Vec<_> = names.iter().map(String::as_str).collect();
    let (fs, directory) = microvm_with_an_open_root(root_path, &names);

    // Read the first page, and forget its entries, as the guest does.
    let first_page = decode_dir_entries(&directory.read_dir(&fs, 0, 4096, true).unwrap());
    let offset = first_page.last().unwrap().offset;
    let mut listed = Vec::new();
    for entry in first_page {
        if entry.node_id != 0 {
            fs.forget(entry.node_id, 1);
        }
        listed.push(entry.name);
    }

    // Unlink the first file that the next read reaches, and a later one.
    let snapshot = directory.directory_entries();
    let next = 1 + snapshot
        .iter()
        .position(|entry| entry.next_cookie == offset)
        .unwrap();
    let unlinked: Vec<_> = snapshot[next..]
        .iter()
        .map(|entry| String::from_utf8(entry.name.clone()).unwrap())
        .filter(|name| name != "." && name != "..")
        .step_by(5)
        .take(2)
        .collect();
    assert_eq!(unlinked.len(), 2);
    for name in &unlinked {
        std::fs::remove_file(root_path.join(name)).unwrap();
    }

    listed.extend(crate::tests::read_directory(&fs, &directory, offset, 4096, true).into_keys());
    listed.sort();
    let mut expected: Vec<_> = [".", ".."]
        .into_iter()
        .chain(names)
        .filter(|name| !unlinked.iter().any(|unlinked| unlinked == name))
        .map(str::to_owned)
        .collect();
    expected.sort();
    assert_eq!(listed, expected);
    // The skipped entries hold no lookup reference.
    assert_eq!(fs.inner.inodes.read().inodes_by_node_id.values.len(), 1);
}

/// An empty READDIRPLUS reply ends the guest's listing. A read that skips
/// every entry left in the directory's snapshot, because each was unlinked,
/// has reached the end of the directory, so it must return one. A read that
/// skips some entries but stops at one that its buffer cannot hold has not,
/// so it must fail, as a read that stops at its first entry does.
#[test]
fn readdirplus_ends_the_listing_only_after_skipping_every_entry_left() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    let unlinked = ["first", "second", "third"];
    let long_name = "a-name-too-long-for-the-buffer";
    let (fs, directory) =
        microvm_with_an_open_root(root_path, &[&unlinked[..], &[long_name]].concat());
    // List the directory without looking its entries up, and order its
    // snapshot as a host may: the dots, the short names, then the long one.
    directory.read_dir(&fs, 0, 4096, false).unwrap();
    let mut entries = directory.directory_entries();
    entries.sort_by_key(|entry| entry.name.len());
    for (cookie, entry) in (1..).zip(&mut entries) {
        entry.next_cookie = cookie;
    }
    directory
        .restore_directory_snapshot(&fs.inner.directory_snapshot_budget.read(), true, entries)
        .unwrap();
    let after_dots = 2;
    for name in unlinked {
        std::fs::remove_file(root_path.join(name)).unwrap();
    }

    // A buffer that holds the entry of a short name, but not that of the long
    // one.
    let size = fuse_dirent_align(size_of::<fuse_direntplus>() + "second".len());
    assert!(size < fuse_dirent_align(size_of::<fuse_direntplus>() + long_name.len()));
    assert_eq!(
        directory
            .read_dir(&fs, after_dots, size as u32, true)
            .unwrap_err(),
        lx::Error::EINVAL
    );

    std::fs::remove_file(root_path.join(long_name)).unwrap();
    assert!(
        directory
            .read_dir(&fs, after_dots, 4096, true)
            .unwrap()
            .is_empty()
    );
}

/// Returns the offset at which a read of `directory`, whose snapshot lists
/// `name`, starts at that entry.
fn offset_of(directory: &VirtioFsFile, name: &[u8]) -> u64 {
    let entries = directory.directory_entries();
    let index = entries.iter().position(|entry| entry.name == name).unwrap();
    index
        .checked_sub(1)
        .map_or(0, |index| entries[index].next_cookie)
}

/// A listed name that a rename replaced since the listing no longer names the
/// listed object, which the host now lists by another number, so READDIRPLUS
/// must fail at it rather than list the object that replaced it.
#[test]
fn readdirplus_fails_at_an_entry_replaced_since_the_listing() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    let (fs, directory) = microvm_with_an_open_root(root_path, &["entry"]);
    // A plain READDIR lists the directory without looking its entries up.
    directory.read_dir(&fs, 0, 4096, false).unwrap();
    std::fs::write(root_path.join("replacement"), "replacement").unwrap();
    std::fs::rename(root_path.join("replacement"), root_path.join("entry")).unwrap();

    let offset = offset_of(&directory, b"entry");
    assert_eq!(
        directory.read_dir(&fs, offset, 4096, true).unwrap_err(),
        lx::Error::EIO
    );
}

/// READDIRPLUS looks a listed name up in the directory at its inode's path,
/// which the host may have replaced since the guest opened the listed one, as
/// by renaming the listed one away and creating another in its place. A name
/// that reaches an object of that other directory does not name the listed
/// object, even though the listed directory still lists the name as it did,
/// so READDIRPLUS must fail at it.
#[test]
fn readdirplus_fails_at_an_entry_of_another_directory_at_the_listed_path() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::create_dir(root_path.join("listed")).unwrap();
    std::fs::write(root_path.join("listed").join("entry"), "listed").unwrap();
    let fs = VirtioFs::new_microvm(root_path, profile(root_path)).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let listed = fs
        .lookup_helper(&root, lx::LxStr::from_bytes(b"listed"))
        .unwrap()
        .nodeid;
    let handle = fs
        .insert_file(fs.get_inode(listed).unwrap().open(lx::O_RDONLY).unwrap())
        .unwrap();
    let directory = fs.get_file(listed, handle).unwrap();
    directory.read_dir(&fs, 0, 4096, false).unwrap();
    std::fs::rename(root_path.join("listed"), root_path.join("moved")).unwrap();
    std::fs::create_dir(root_path.join("listed")).unwrap();
    std::fs::write(root_path.join("listed").join("entry"), "replacement").unwrap();

    let offset = offset_of(&directory, b"entry");
    assert_eq!(
        directory.read_dir(&fs, offset, 4096, true).unwrap_err(),
        lx::Error::EIO
    );
}

/// The host lists some entries of a directory by another inode number than a
/// lookup of the entry reports on the directory's own device: a btrfs
/// snapshot lists the placeholder of a nested subvolume by the subvolume's ID,
/// while a lookup reaches an empty directory numbered 2, and a directory lists
/// a mount point of its own filesystem by the number of the directory that the
/// mount covers, while a lookup reaches the mounted one. Such an entry still
/// names the listed object, so READDIRPLUS must list it, with the number that
/// its lookup reports. A bind mount, which an unprivileged process can make in
/// user and mount namespaces of its own, gives such an entry here; where the
/// host allows no such namespaces, the test does nothing.
#[cfg(target_os = "linux")]
#[test]
fn readdirplus_lists_an_entry_that_the_host_lists_by_another_number() {
    use std::os::unix::fs::DirEntryExt;
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;

    const ROOT: &str = "VIRTIOFS_TEST_BIND_MOUNT_ROOT";
    let namespaces = ["--user", "--map-root-user", "--mount"];
    let Some(root_path) = std::env::var_os(ROOT) else {
        // Only a single-threaded process can enter a new user namespace, so
        // the test runs again in a child process in the namespaces, in a
        // directory that this process removes once the child is done.
        let temporary_directory = tempdir().unwrap();
        let root_path = temporary_directory.path();
        let bind_mount_works = Command::new("unshare")
            .args(namespaces)
            .args(["mount", "--bind"])
            .args([root_path, root_path])
            .status()
            .is_ok_and(|status| status.success());
        if !bind_mount_works {
            eprintln!("skipping: no bind mount in user and mount namespaces");
            return;
        }
        let status = Command::new("unshare")
            .args(namespaces)
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "microvm::fs_tests::readdirplus_lists_an_entry_that_the_host_lists_by_another_number",
                "--no-capture",
            ])
            .env(ROOT, root_path)
            .status()
            .unwrap();
        assert!(status.success(), "{status}");
        return;
    };
    let root_path = Path::new(&root_path);
    std::fs::create_dir(root_path.join("mounted")).unwrap();
    std::fs::create_dir(root_path.join("point")).unwrap();
    let status = Command::new("mount")
        .arg("--bind")
        .args([root_path.join("mounted"), root_path.join("point")])
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
    let listed = std::fs::read_dir(root_path)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name() == "point")
        .unwrap()
        .ino();
    let point = std::fs::symlink_metadata(root_path.join("point")).unwrap();
    assert_ne!(point.ino(), listed);
    assert_eq!(
        point.dev(),
        std::fs::symlink_metadata(root_path).unwrap().dev()
    );

    let (fs, directory) = microvm_with_an_open_root(root_path, &[]);
    let entries = crate::tests::read_directory(&fs, &directory, 0, 4096, true);
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let point = fs
        .lookup_helper(&root, lx::LxStr::from_bytes(b"point"))
        .unwrap();
    assert_eq!(entries["point"].0, point.attr.ino);
}

/// Returns the names of the entries of `reply`, a READDIRPLUS reply, sorted,
/// and the node ID of the entry named `name`.
fn names_and_node_id_of(reply: &[u8], name: &str) -> (Vec<String>, u64) {
    let entries = decode_dir_entries(reply);
    let node_id = entries
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.node_id)
        .unwrap();
    let mut names: Vec<_> = entries.into_iter().map(|entry| entry.name).collect();
    names.sort();
    (names, node_id)
}

/// A READDIRPLUS entry hands the guest the node of the object that its name
/// reaches, and a lookup that reaches the object of a node makes the name's
/// path the node's path. A name that a handle lists can reach the object of
/// the handle's own node: once the host moves the node's directory into a new
/// directory at the node's path, as here, a handle that the guest opens on
/// the node lists that new directory, which holds the moved one, and a bind
/// mount of a directory on one of its own subdirectories reaches the directory
/// too. The guest cannot hold a directory below itself, and READDIRPLUS looks
/// the handle's other names up at the node's path, which must therefore stay
/// the path of the directory that the handle lists. So READDIRPLUS must list
/// the entry without a node, as READDIR lists it, and leave the node as it
/// was.
#[test]
fn readdirplus_lists_an_entry_that_reaches_the_listed_node_without_a_node() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    std::fs::create_dir(root_path.join("listed")).unwrap();
    let fs = VirtioFs::new_microvm(root_path, profile(root_path)).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let node_id = fs
        .lookup_helper(&root, lx::LxStr::from_bytes(b"listed"))
        .unwrap()
        .nodeid;
    std::fs::rename(root_path.join("listed"), root_path.join("moved")).unwrap();
    std::fs::create_dir(root_path.join("listed")).unwrap();
    std::fs::rename(
        root_path.join("moved"),
        root_path.join("listed").join("moved"),
    )
    .unwrap();
    std::fs::write(root_path.join("listed").join("file"), "file").unwrap();
    let listed = fs.get_inode(node_id).unwrap();
    let handle = fs
        .insert_file(Arc::clone(&listed).open(lx::O_RDONLY).unwrap())
        .unwrap();
    let directory = fs.get_file(node_id, handle).unwrap();
    // List the directory without looking its entries up, and order its
    // snapshot as a host may, with the moved directory before the file.
    directory.read_dir(&fs, 0, 4096, false).unwrap();
    let mut entries = directory.directory_entries();
    entries.sort_by_key(|entry| entry.name == b"file");
    for (cookie, entry) in (1..).zip(&mut entries) {
        entry.next_cookie = cookie;
    }
    directory
        .restore_directory_snapshot(&fs.inner.directory_snapshot_budget.read(), true, entries)
        .unwrap();

    let reply = directory.read_dir(&fs, 0, 4096, true).unwrap();
    let (names, moved) = names_and_node_id_of(&reply, "moved");
    assert_eq!(names, [".", "..", "file", "moved"]);
    assert_eq!(moved, 0);
    assert_eq!(listed.clone_path(), PathBuf::from("listed"));
    assert_eq!(listed.aliases(), [PathBuf::from("listed")]);
    assert_eq!(listed.lookup_count(), 1);
}

/// A bind mount of a directory on one of its own subdirectories, which an
/// unprivileged process can make in user and mount namespaces of its own,
/// reaches the directory, which the host lists by the number of the
/// subdirectory that the mount covers. READDIRPLUS must list it without a
/// node, as READDIR lists it, and leave the directory's node, here the share's
/// root, at its path; where the host allows no such namespaces, the test does
/// nothing.
#[cfg(target_os = "linux")]
#[test]
fn readdirplus_lists_a_bind_mount_of_the_listed_directory_without_a_node() {
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;

    const ROOT: &str = "VIRTIOFS_TEST_BIND_MOUNT_ROOT";
    let namespaces = ["--user", "--map-root-user", "--mount"];
    let Some(root_path) = std::env::var_os(ROOT) else {
        // Only a single-threaded process can enter a new user namespace, so
        // the test runs again in a child process in the namespaces, in a
        // directory that this process removes once the child is done.
        let temporary_directory = tempdir().unwrap();
        let root_path = temporary_directory.path();
        let bind_mount_works = Command::new("unshare")
            .args(namespaces)
            .args(["mount", "--bind"])
            .args([root_path, root_path])
            .status()
            .is_ok_and(|status| status.success());
        if !bind_mount_works {
            eprintln!("skipping: no bind mount in user and mount namespaces");
            return;
        }
        let status = Command::new("unshare")
            .args(namespaces)
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "microvm::fs_tests::readdirplus_lists_a_bind_mount_of_the_listed_directory_without_a_node",
                "--no-capture",
            ])
            .env(ROOT, root_path)
            .status()
            .unwrap();
        assert!(status.success(), "{status}");
        return;
    };
    let root_path = Path::new(&root_path);
    std::fs::create_dir(root_path.join("loop")).unwrap();
    let status = Command::new("mount")
        .arg("--bind")
        .arg(root_path)
        .arg(root_path.join("loop"))
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
    let identity = |path: &Path| {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        (metadata.dev(), metadata.ino())
    };
    assert_eq!(identity(&root_path.join("loop")), identity(root_path));

    let (fs, directory) = microvm_with_an_open_root(root_path, &["file"]);
    let reply = directory.read_dir(&fs, 0, 4096, true).unwrap();
    let (names, node_id) = names_and_node_id_of(&reply, "loop");
    assert_eq!(names, [".", "..", "file", "loop"]);
    assert_eq!(node_id, 0);
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    assert_eq!(root.clone_path(), PathBuf::new());
    assert_eq!(root.aliases(), [PathBuf::new()]);
    // The guest may read the pages of one listing with either request, so
    // READDIRPLUS lists the mount point by the number and type by which
    // READDIR lists it.
    assert_eq!(
        crate::tests::read_directory(&fs, &directory, 0, 4096, true)["loop"],
        crate::tests::read_directory(&fs, &directory, 0, 4096, false)["loop"]
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
    let file = source.get_file(FUSE_ROOT_ID, handle).unwrap();

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
        .get_file(FUSE_ROOT_ID, handle)
        .unwrap()
        .read_dir(&destination, continuation_offset, 4096, false)
        .unwrap();
    assert_eq!(continuation, expected_continuation);
}

/// The entries of each listing of the root of [`microvm_with_root_listings`],
/// `.` and `..` included.
const ROOT_LISTING_ENTRIES: usize = 64;

/// How many listings of the root of [`microvm_with_root_listings`] a saved
/// state holds at most: its bound on the entries of the listings of all
/// handles.
const ROOT_LISTINGS_AT_THE_BOUND: usize = MAX_DIRECTORY_ENTRIES / ROOT_LISTING_ENTRIES;

/// Fills `root_path` with files, so that a listing of it holds
/// [`ROOT_LISTING_ENTRIES`] entries, and returns its profile and a microVM
/// filesystem of it.
fn microvm_with_root_listings(root_path: &Path) -> (MicroVmVirtioFsProfile, VirtioFs) {
    for index in 2..ROOT_LISTING_ENTRIES {
        std::fs::write(root_path.join(index.to_string()), b"").unwrap();
    }
    let profile = profile(root_path);
    let fs = VirtioFs::new_microvm(root_path, profile.clone()).unwrap();
    (profile, fs)
}

/// Opens the root, as FUSE_OPENDIR does, and returns the handle.
fn open_root(fs: &VirtioFs) -> u64 {
    fs.open_dir(&request(FUSE_ROOT_ID), lx::O_RDONLY as u32)
        .unwrap()
        .fh
}

/// Reads the root through the handle `fh` from its start, as FUSE_READDIR or,
/// with `plus`, FUSE_READDIRPLUS does. The first read of a handle lists the
/// root.
fn read_root(fs: &VirtioFs, fh: u64, plus: bool) -> lx::Result<()> {
    let mut arg = fuse_read_in::new_zeroed();
    arg.fh = fh;
    arg.size = 4096;
    if plus {
        fs.read_dir_plus(&request(FUSE_ROOT_ID), &arg)?;
    } else {
        fs.read_dir(&request(FUSE_ROOT_ID), &arg)?;
    }
    Ok(())
}

/// Releases the handle `fh` of the root, as FUSE_RELEASEDIR does.
fn release_root(fs: &VirtioFs, fh: u64) {
    let mut arg = fuse_release_in::new_zeroed();
    arg.fh = fh;
    fs.release_dir(&request(FUSE_ROOT_ID), &arg).unwrap();
}

/// A saved state holds at most MAX_DIRECTORY_ENTRIES entries in the listings
/// of all handles, so a guest that lists more directories must not fail the
/// save: the first read of a handle, which lists its directory, fails with
/// ENOSPC instead, and leaves the handle without a listing. Releasing a
/// handle returns its listing, and FUSE_DESTROY returns them all.
#[test]
fn directory_listings_beyond_the_saved_bound_are_refused() {
    let temporary_directory = tempdir().unwrap();
    let (profile, fs) = microvm_with_root_listings(temporary_directory.path());
    let handles: Vec<_> = (0..=ROOT_LISTINGS_AT_THE_BOUND)
        .map(|_| open_root(&fs))
        .collect();
    let reads: Vec<_> = handles
        .iter()
        .map(|&fh| read_root(&fs, fh, false))
        .collect();
    let state = fs
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    let saved_entries: usize = state
        .handles
        .iter()
        .map(|handle| handle.directory_entries.len())
        .sum();
    assert_eq!(saved_entries, MAX_DIRECTORY_ENTRIES);
    let (refused, listed) = reads.split_last().unwrap();
    assert!(listed.iter().all(Result::is_ok), "{listed:?}");
    assert_eq!(*refused, Err(lx::Error::ENOSPC));
    let refused = *handles.last().unwrap();
    assert_eq!(read_root(&fs, refused, true), Err(lx::Error::ENOSPC));

    release_root(&fs, handles[0]);
    read_root(&fs, refused, false).unwrap();
    assert_eq!(
        read_root(&fs, open_root(&fs), false),
        Err(lx::Error::ENOSPC)
    );

    fs.destroy();
    for _ in 0..ROOT_LISTINGS_AT_THE_BOUND {
        read_root(&fs, open_root(&fs), false).unwrap();
    }
}

/// A restore charges the listings that it restores to the bound of a saved
/// state, in place of the listings of the handles that it replaces, so the
/// restored attachment refuses the listings that the saved one would.
#[test]
fn restored_directory_listings_count_toward_the_saved_bound() {
    let temporary_directory = tempdir().unwrap();
    let root_path = temporary_directory.path();
    let (profile, source) = microvm_with_root_listings(root_path);
    let handles: Vec<_> = (0..ROOT_LISTINGS_AT_THE_BOUND)
        .map(|_| {
            let fh = open_root(&source);
            read_root(&source, fh, false).unwrap();
            fh
        })
        .collect();

    // A new attachment, and the source itself, whose handles hold listings
    // up to the bound.
    let destinations = [
        VirtioFs::new_microvm(root_path, profile.clone()).unwrap(),
        source.clone(),
    ];
    for destination in destinations {
        let state = source
            .save_microvm_state(&profile, SessionState::default())
            .unwrap();
        let session = Session::new(destination.clone());
        destination
            .restore_microvm_state(&profile, state, &session)
            .unwrap();
        let fh = open_root(&destination);
        assert_eq!(read_root(&destination, fh, false), Err(lx::Error::ENOSPC));
        release_root(&destination, handles[0]);
        read_root(&destination, fh, false).unwrap();
    }
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
        .get_file(sys.nodeid, handle)
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
        &destination.get_file(sys.nodeid, handle).unwrap(),
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
    let entries = crate::tests::read_directory(
        &fs,
        &fs.get_file(FUSE_ROOT_ID, handle).unwrap(),
        0,
        4096,
        true,
    );
    for name in ["proc", "sys"] {
        let lookup = fs
            .lookup_helper(&root, lx::LxStr::from_bytes(name.as_bytes()))
            .unwrap();
        assert_eq!(entries[name].0, lookup.attr.ino, "{name}");
    }
    assert_ne!(entries["proc"].0, entries["sys"].0);
}
