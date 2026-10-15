// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::SYNTHETIC_ROOT_FH;
use crate::VirtioFs;
use crate::inode;
use fuse::protocol::FUSE_ROOT_ID;
use lxutil::LxVolumeOptions;
use std::sync::Arc;
use test_with_tracing::test;

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
    assert_eq!(
        fs.add_child("share_a", a.path(), None).unwrap_err(),
        lx::Error::EEXIST
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
    assert_eq!(
        fs.add_child("x", a.path(), None).unwrap_err(),
        lx::Error::EINVAL
    );
    assert_eq!(fs.remove_child("x").unwrap_err(), lx::Error::EINVAL);
}

#[test]
fn add_child_validates_name() {
    let root = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();

    for name in ["", ".", "..", "a/b", "a\0b"] {
        assert_eq!(
            fs.add_child(name, root.path(), None).unwrap_err(),
            lx::Error::EINVAL
        );
    }

    fs.add_child(&"a".repeat(255), root.path(), None).unwrap();
    assert_eq!(
        fs.add_child(&"b".repeat(256), root.path(), None)
            .unwrap_err(),
        lx::Error::ENAMETOOLONG
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
    assert_ne!(volume.map_inode(u64::MAX), u64::MAX);
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
    assert_eq!(
        fs.add_child("after", a.path(), None).unwrap_err(),
        lx::Error::EAGAIN
    );
    assert_eq!(
        fs.inner.aggregate().unwrap().registry.read().entries.len(),
        1
    );
}
