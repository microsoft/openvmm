// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::SYNTHETIC_ROOT_FH;
use crate::VirtioFs;
use crate::inode;
use crate::inode::MAX_AGGREGATE_VOLUMES;
use fuse::protocol::FUSE_ATTR_SUBMOUNT;
use fuse::protocol::FUSE_ROOT_ID;
use lxutil::LxVolumeOptions;
use std::sync::Arc;
use test_with_tracing::test;

#[test]
fn aggregate_child_registry() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();

    fs.add_child("share_a", a.path(), None).unwrap();
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
    }

    // Removal drops only the named child.
    fs.remove_child("share_a").unwrap();
    assert_eq!(fs.remove_child("share_a").unwrap_err(), lx::Error::ENOENT);
    assert_eq!(
        fs.inner.aggregate().unwrap().registry.read().entries.len(),
        1
    );
}

#[test]
fn add_child_rejected_in_direct_mode() {
    let a = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new(a.path(), None).unwrap();
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
fn submount_flag_requires_negotiation() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();
    fs.add_child("first", first.path(), None).unwrap();

    let volume = |index: usize| {
        let children = fs.inner.aggregate().unwrap().registry.read();
        Arc::clone(&children.entries[index].volume)
    };
    assert_eq!(
        volume(0).map_inode(u64::MAX).unwrap_err(),
        lx::Error::EOVERFLOW
    );

    let entry = fs
        .lookup_synthetic_root(lx::LxStr::from_bytes(b"first"))
        .unwrap();
    assert_eq!(entry.attr.flags & FUSE_ATTR_SUBMOUNT, 0);

    assert!(fs.initialize_submounts(true));
    assert_eq!(volume(0).map_inode(u64::MAX).unwrap(), u64::MAX);

    fs.add_child("second", second.path(), None).unwrap();
    assert_eq!(volume(1).map_inode(u64::MAX).unwrap(), u64::MAX);
    assert_ne!(
        fs.lookup_synthetic_root(lx::LxStr::from_bytes(b"second"))
            .unwrap()
            .attr
            .flags
            & FUSE_ATTR_SUBMOUNT,
        0
    );

    fs.reset_submounts();
    assert!(!fs.initialize_submounts(false));
    assert_eq!(volume(0).map_inode(u64::MAX), Err(lx::Error::EOVERFLOW));
    assert_eq!(volume(1).map_inode(u64::MAX), Err(lx::Error::EOVERFLOW));
}

#[test]
fn synthetic_root_handle_is_scoped_to_root() {
    let aggregate = VirtioFs::new_aggregate();
    assert!(aggregate.is_synthetic_root_handle(FUSE_ROOT_ID, SYNTHETIC_ROOT_FH));
    assert!(!aggregate.is_synthetic_root_handle(FUSE_ROOT_ID + 1, SYNTHETIC_ROOT_FH));

    let root = tempfile::tempdir().unwrap();
    let direct = VirtioFs::new(root.path(), None).unwrap();
    assert!(!direct.is_synthetic_root_handle(FUSE_ROOT_ID, SYNTHETIC_ROOT_FH));
}

#[test]
fn synthetic_root_link_count_includes_children() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();

    assert_eq!(fs.synthetic_root_attr().nlink, 2);
    assert_eq!(fs.synthetic_root_statx(lx::StatExMask::new()).nlink, 2);

    fs.add_child("first", first.path(), None).unwrap();
    fs.add_child("second", second.path(), None).unwrap();
    assert_eq!(fs.synthetic_root_attr().nlink, 4);
    assert_eq!(fs.synthetic_root_statx(lx::StatExMask::new()).nlink, 4);

    fs.remove_child("first").unwrap();
    assert_eq!(fs.synthetic_root_attr().nlink, 3);
    assert_eq!(fs.synthetic_root_statx(lx::StatExMask::new()).nlink, 3);
}

#[test]
fn inode_namespacing_avoids_cross_volume_collisions() {
    // Direct mode (volume id 0) is the identity transform.
    assert_eq!(inode::namespace_ino(0, 42).unwrap(), 42);
    assert_eq!(inode::namespace_ino(0, u64::MAX).unwrap(), u64::MAX);

    const INODE_MASK: u64 = (1 << 58) - 1;
    let raw = INODE_MASK;
    assert_eq!(inode::namespace_ino(1, raw).unwrap(), INODE_MASK);
    assert_eq!(
        inode::namespace_ino(2, raw).unwrap(),
        (1 << 58) | INODE_MASK
    );
    assert_eq!(inode::namespace_ino(64, raw).unwrap(), u64::MAX);
    assert_eq!(
        inode::namespace_ino(65, raw).unwrap_err(),
        lx::Error::ENOSPC
    );
    assert_eq!(
        inode::namespace_ino(1, INODE_MASK + 1).unwrap_err(),
        lx::Error::EOVERFLOW
    );

    // Distinct host inode numbers remain distinct within a volume.
    assert_ne!(
        inode::namespace_ino(1, 10).unwrap(),
        inode::namespace_ino(1, 11).unwrap()
    );
}

#[test]
fn aggregate_volume_count_is_limited_to_namespace_capacity() {
    let root = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();
    assert!(!fs.initialize_submounts(false));

    for index in 0..MAX_AGGREGATE_VOLUMES {
        fs.add_child(&format!("share_{index}"), root.path(), None)
            .unwrap();
    }

    assert_eq!(
        fs.add_child("one_too_many", root.path(), None).unwrap_err(),
        lx::Error::ENOSPC
    );
}

#[test]
fn aggregate_volume_count_is_not_limited_when_submounts_are_available() {
    let root = tempfile::tempdir().unwrap();
    let fs = VirtioFs::new_aggregate();

    fs.inner
        .aggregate()
        .unwrap()
        .registry
        .write()
        .next_volume_id = MAX_AGGREGATE_VOLUMES + 1;
    fs.add_child("pending", root.path(), None).unwrap();

    assert!(fs.initialize_submounts(true));
    fs.add_child("negotiated", root.path(), None).unwrap();

    let aggregate = fs.inner.aggregate().unwrap();
    let children = aggregate.registry.read();
    assert_eq!(children.entries[0].volume.id(), MAX_AGGREGATE_VOLUMES + 1);
    assert_eq!(children.entries[1].volume.id(), MAX_AGGREGATE_VOLUMES + 2);
    assert_eq!(children.entries[0].volume.map_inode(42).unwrap(), 42);
    assert_eq!(children.entries[1].volume.map_inode(42).unwrap(), 42);

    let fallback = VirtioFs::new_aggregate();
    fallback
        .inner
        .aggregate()
        .unwrap()
        .registry
        .write()
        .next_volume_id = MAX_AGGREGATE_VOLUMES + 1;
    assert!(!fallback.initialize_submounts(false));
    assert_eq!(
        fallback.add_child("fallback", root.path(), None),
        Err(lx::Error::ENOSPC)
    );
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
fn add_child_allows_per_child_readonly() {
    let a = tempfile::tempdir().unwrap();
    let mut ro = LxVolumeOptions::default();
    ro.readonly(true);
    let mut rw = LxVolumeOptions::default();
    rw.readonly(false);

    // A writable aggregate lets each child pick its own readonly setting.
    let fs = VirtioFs::new_aggregate();
    fs.add_child("ro_child", a.path(), Some(&ro)).unwrap();
    fs.add_child("rw_child", a.path(), Some(&rw)).unwrap();

    let aggregate = fs.inner.aggregate().unwrap();
    let children = aggregate.registry.read();
    let ro_entry = children
        .entries
        .iter()
        .find(|e| e.name == "ro_child")
        .unwrap();
    let rw_entry = children
        .entries
        .iter()
        .find(|e| e.name == "rw_child")
        .unwrap();
    assert!(ro_entry.volume.readonly());
    assert!(!rw_entry.volume.readonly());
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
