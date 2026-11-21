// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::SYNTHETIC_ROOT_FH;
use crate::VirtioFs;
use crate::inode;
use crate::inode::VirtioFsVolume;
use crate::test_helpers::decode_dir_entries;
use fuse::Fuse;
use fuse::protocol::FUSE_ROOT_ID;
use fuse::protocol::fuse_dirent;
use fuse::protocol::fuse_dirent_align;
use fuse::protocol::fuse_direntplus;
use lxutil::LxVolumeOptions;
use std::sync::Arc;
use test_with_tracing::test;
use zerocopy::FromBytes;

fn assert_config_errno(error: crate::CreateError, expected: lx::Error) {
    match error {
        crate::CreateError::Filesystem(actual) => assert_eq!(actual, expected),
        other => panic!("expected configuration errno {expected}, got {other}"),
    }
}

#[test]
fn setup_failures_preserve_error_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let missing_root = tmp.path().join("missing");
    let aggregate = VirtioFs::new_aggregate();
    for error in [
        VirtioFs::new(&missing_root, None).err().unwrap(),
        aggregate
            .add_child("share", &missing_root, None)
            .unwrap_err(),
    ] {
        assert!(matches!(error, crate::CreateError::Volume(_)));
        assert!(
            error
                .to_string()
                .contains("failed to initialize filesystem volume")
        );
        let cause = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<lx::Error>()
            .expect("filesystem setup must preserve the OS error");
        assert_eq!(*cause, lx::Error::ENOENT);
    }
}

#[test]
fn aggregate_child_registry() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut readonly = LxVolumeOptions::default();
    readonly.readonly(true);
    let fs = VirtioFs::new_aggregate();

    assert_eq!(fs.synthetic_root_attr().nlink, 2);
    assert_eq!(fs.synthetic_root_statx(lx::StatExMask::new()).nlink, 2);

    fs.add_child("share_a", a.path(), Some(&readonly)).unwrap();
    fs.add_child("share_b", b.path(), None).unwrap();

    // Duplicate names are rejected.
    assert_config_errno(
        fs.add_child("share_a", a.path(), None).unwrap_err(),
        lx::Error::EEXIST,
    );

    // Each child gets a distinct, non-zero volume id (0 is reserved for
    // direct mode).
    {
        let aggregate = fs.inner.aggregate().unwrap();
        let children = aggregate.registry.read();
        assert_eq!(children.entries.len(), 2);
        assert_ne!(children.entries[0].volume.id(), 0);
        assert_ne!(
            children.entries[0].volume.id(),
            children.entries[1].volume.id()
        );
        assert!(children.entries[0].volume.readonly());
        assert!(!children.entries[1].volume.readonly());
    }
    assert_eq!(fs.synthetic_root_attr().nlink, 4);
    assert_eq!(fs.synthetic_root_statx(lx::StatExMask::new()).nlink, 4);

    // Removal drops only the named child.
    fs.remove_child("share_a").unwrap();
    assert_eq!(fs.remove_child("share_a").unwrap_err(), lx::Error::ENOENT);
    assert_eq!(
        fs.inner.aggregate().unwrap().registry.read().entries.len(),
        1
    );
    assert_eq!(fs.synthetic_root_attr().nlink, 3);
    assert_eq!(fs.synthetic_root_statx(lx::StatExMask::new()).nlink, 3);
}

#[test]
fn aggregate_operations_are_scoped_to_aggregate_mode() {
    let aggregate = VirtioFs::new_aggregate();
    assert!(aggregate.is_synthetic_root_handle(FUSE_ROOT_ID, SYNTHETIC_ROOT_FH));
    assert!(!aggregate.is_synthetic_root_handle(FUSE_ROOT_ID + 1, SYNTHETIC_ROOT_FH));

    let a = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new(a.path(), None).unwrap();
    assert!(!fs.is_synthetic_root_handle(FUSE_ROOT_ID, SYNTHETIC_ROOT_FH));
    assert_config_errno(
        fs.add_child("x", a.path(), None).unwrap_err(),
        lx::Error::EINVAL,
    );
    assert_eq!(fs.remove_child("x").unwrap_err(), lx::Error::EINVAL);
}

#[test]
fn add_child_validates_name() {
    let root = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();

    for name in ["", ".", "..", "a/b", "a\0b"] {
        assert_config_errno(
            fs.add_child(name, root.path(), None).unwrap_err(),
            lx::Error::EINVAL,
        );
    }

    fs.add_child(&"a".repeat(255), root.path(), None).unwrap();
    assert_config_errno(
        fs.add_child(&"b".repeat(256), root.path(), None)
            .unwrap_err(),
        lx::Error::ENAMETOOLONG,
    );
}

#[test]
fn synthetic_root_node_ids_start_after_root() {
    // In aggregate mode the synthetic root occupies FUSE_ROOT_ID, so the
    // first real inode inserted must be allocated a higher id.
    let a = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();
    fs.add_child("share", a.path(), None).unwrap();
    let entry = fs
        .lookup_synthetic_root(lx::LxStr::from_bytes(b"share"))
        .unwrap();
    assert!(entry.nodeid > FUSE_ROOT_ID);
}

#[test]
fn aggregate_children_namespace_inodes() {
    // Under the single shared superblock, each aggregated child namespaces its
    // inode numbers so that even the largest host inode maps to a value other
    // than the identity transform reserved for direct mode (volume id 0).
    let root = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();
    fs.add_child("child", root.path(), None).unwrap();
    let volume = {
        let children = fs.inner.aggregate().unwrap().registry.read();
        Arc::clone(&children.entries[0].volume)
    };
    assert_ne!(
        volume.map_inode(volume.root_device_nr(), u64::MAX),
        u64::MAX
    );
}

#[test]
fn inode_namespacing_avoids_cross_volume_collisions() {
    // Direct mode (volume id 0) is the identity transform.
    assert_eq!(inode::namespace_ino(0, 42), 42);
    assert_eq!(inode::namespace_ino(0, u64::MAX), u64::MAX);

    // Namespacing uses the full 64-bit inode space, so even the largest host
    // inode numbers map without overflowing or being rejected, and there is no
    // limit on the number of volumes.
    let _ = inode::namespace_ino(1, u64::MAX);
    let _ = inode::namespace_ino(1000, u64::MAX);

    // The transform is a bijection within a volume: distinct host inode numbers
    // stay distinct.
    assert_ne!(inode::namespace_ino(1, 10), inode::namespace_ino(1, 11));

    // The same host inode number maps to different values in different volumes,
    // reducing cross-volume st_ino collisions under the shared superblock.
    assert_ne!(inode::namespace_ino(1, 42), inode::namespace_ino(2, 42));
}

#[test]
fn hard_link_rejects_cross_volume_target() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(first.path().join("target"), b"data").unwrap();

    let fs = VirtioFs::new_aggregate();
    fs.add_child("first", first.path(), None).unwrap();
    fs.add_child("second", second.path(), None).unwrap();

    let first_root = fs
        .lookup_synthetic_root(lx::LxStr::from_bytes(b"first"))
        .unwrap();
    let second_root = fs
        .lookup_synthetic_root(lx::LxStr::from_bytes(b"second"))
        .unwrap();
    let target = fs
        .lookup_helper(
            &fs.get_inode(first_root.nodeid).unwrap(),
            lx::LxStr::from_bytes(b"target"),
        )
        .unwrap();

    assert_eq!(
        fs.get_inode(second_root.nodeid)
            .unwrap()
            .link(
                lx::LxStr::from_bytes(b"link"),
                &fs.get_inode(target.nodeid).unwrap()
            )
            .unwrap_err(),
        lx::Error::EXDEV
    );
    assert!(!second.path().join("link").exists());
}

#[test]
fn add_child_rejected_after_teardown() {
    let a = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();
    fs.add_child("before", a.path(), None).unwrap();

    fs.begin_teardown();

    // Once tearing down, no further children can be added.
    assert_config_errno(
        fs.add_child("after", a.path(), None).unwrap_err(),
        lx::Error::EAGAIN,
    );
    assert_eq!(
        fs.inner.aggregate().unwrap().registry.read().entries.len(),
        1
    );
}

#[test]
fn failed_synthetic_root_readdirplus_returns_entries_holding_references() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();
    fs.add_child("first", first.path(), None).unwrap();
    fs.add_child("second", second.path(), None).unwrap();
    let second_volume_id = fs.inner.aggregate().unwrap().registry.read().entries[1]
        .volume
        .id();

    // Look up the first child's root, and fail the second's with a host error.
    let mut lookup = |volume: Arc<VirtioFsVolume>| {
        if volume.id() == second_volume_id {
            Err(lx::Error::EIO)
        } else {
            fs.insert_child_root_entry(volume)
        }
    };

    // The read stops at the second child, but still returns the first child,
    // whose entry holds the reference its lookup took.
    let reply = fs
        .read_synthetic_root_dir_with(0, 4096, true, &mut lookup)
        .unwrap();
    let entries = decode_dir_entries(&reply);
    let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, [".", "..", "first"]);
    let node_id = FUSE_ROOT_ID + 1;
    assert_eq!(entries[2].node_id, node_id);

    // The next read starts at the second child, so the guest gets the error.
    assert_eq!(
        fs.read_synthetic_root_dir_with(entries[2].offset, 4096, true, &mut lookup)
            .unwrap_err(),
        lx::Error::EIO
    );

    // The guest's forget for the entry it received releases the first child's
    // root.
    fs.forget(node_id, 1);
    assert!(fs.get_inode(node_id).is_err(), "inode leaked");
}

/// Decodes the name of each entry of a READDIR or READDIRPLUS reply and the
/// cookie of the entry after it.
fn decode_entries(reply: &[u8], plus: bool) -> Vec<(String, u64)> {
    if plus {
        return decode_dir_entries(reply)
            .into_iter()
            .map(|entry| (entry.name, entry.offset))
            .collect();
    }
    let mut entries = Vec::new();
    let mut reply = reply;
    while !reply.is_empty() {
        let (dirent, _) = fuse_dirent::read_from_prefix(reply).unwrap();
        let name_end = size_of::<fuse_dirent>() + dirent.namelen as usize;
        let name = &reply[size_of::<fuse_dirent>()..name_end];
        entries.push((String::from_utf8(name.to_vec()).unwrap(), dirent.off));
        reply = &reply[fuse_dirent_align(name_end)..];
    }
    entries
}

/// A read of the synthetic root whose buffer cannot hold the next entry fails
/// with `EINVAL`, as a read of any other directory does, rather than
/// returning an empty reply, which the guest takes for the end of the root,
/// so that it lists none of the children after that entry. A read at the end
/// of the root still returns an empty reply, whatever the size of its buffer.
#[test]
fn synthetic_root_refuses_a_buffer_too_small_for_the_next_entry() {
    let work = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    // A name whose entry is larger than those of `.`, `..`, and `work`.
    let long = "build-tools";
    let fs = VirtioFs::new_aggregate();
    fs.add_child("work", work.path(), None).unwrap();
    fs.add_child(long, tools.path(), None).unwrap();
    let read = |offset: u64, size: usize, plus: bool| {
        fs.read_synthetic_root_dir(offset, size.try_into().unwrap(), plus)
            .map(|reply| decode_entries(&reply, plus))
    };
    // Lists the root as the guest does: each read starts at the last cookie
    // of the read before, until a read returns no entry or fails. Returns the
    // names listed and the error.
    let listing = |size: usize, plus: bool| {
        let mut names = Vec::new();
        let mut offset = 0;
        loop {
            match read(offset, size, plus) {
                Ok(entries) => {
                    let Some(&(_, next)) = entries.last() else {
                        return (names, None);
                    };
                    names.extend(entries.into_iter().map(|(name, _)| name));
                    offset = next;
                }
                Err(error) => return (names, Some(error)),
            }
        }
    };
    // The size of the entry for `name` in a reply: its header and its name,
    // padded to eight bytes.
    let entry_size = |name: &str, plus: bool| {
        let header = if plus {
            size_of::<fuse_direntplus>()
        } else {
            size_of::<fuse_dirent>()
        };
        fuse_dirent_align(header + name.len())
    };

    for plus in [false, true] {
        // A buffer that holds the largest entry lists the whole root.
        let largest = entry_size(long, plus);
        let (names, error) = listing(largest, plus);
        assert_eq!(names, [".", "..", "work", long], "plus: {plus}");
        assert_eq!(error, None, "plus: {plus}");
        // A buffer one byte smaller lists the entries before the largest, and
        // fails at it rather than ending the listing there.
        let (names, error) = listing(largest - 1, plus);
        assert_eq!(names, [".", "..", "work"], "plus: {plus}");
        assert_eq!(error, Some(lx::Error::EINVAL), "plus: {plus}");
        // A read from the start whose buffer cannot hold `.` fails too.
        assert_eq!(
            read(0, entry_size(".", plus) - 1, plus),
            Err(lx::Error::EINVAL),
            "plus: {plus}"
        );
        // A read at the end of the root returns no entry, even with a buffer
        // too small for one.
        let end = read(0, 4096, plus).unwrap().last().unwrap().1;
        assert_eq!(read(end, 1, plus), Ok(Vec::new()), "plus: {plus}");
    }
}
