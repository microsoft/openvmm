// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for microVM aggregates: the synthetic root and the children, each
//! with its own access policy.

use super::policy_tests::create;
use super::policy_tests::error;
use super::policy_tests::list;
use super::policy_tests::lookup;
use super::policy_tests::mkdir;
use super::policy_tests::name;
use super::policy_tests::open;
use super::policy_tests::owned;
use super::policy_tests::read_node;
use super::policy_tests::release;
use super::policy_tests::request;
use super::policy_tests::setattr;
use super::policy_tests::write;
use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmAggregateChild;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use crate::VirtioFs;
use fuse::Fuse;
use fuse::protocol::FATTR_MODE;
use fuse::protocol::FUSE_ROOT_ID;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;
use tempfile::tempdir;
use test_with_tracing::test;

/// A child of a test aggregate: its name, host root, mode, and policy.
struct Child<'a> {
    name: &'a str,
    root: &'a Path,
    read_only: bool,
    denied: &'a [&'a str],
    allowed: &'a [&'a str],
    writable: &'a [&'a str],
}

impl<'a> Child<'a> {
    fn new(name: &'a str, root: &'a Path, read_only: bool) -> Self {
        Self {
            name,
            root,
            read_only,
            denied: &[],
            allowed: &[],
            writable: &[],
        }
    }

    fn profile(&self) -> MicroVmAggregateChild {
        MicroVmAggregateChild::new(
            self.name.to_owned(),
            microvm_root_identity(self.root).unwrap(),
            self.read_only,
            owned(self.denied),
            owned(self.allowed),
            owned(self.writable),
        )
        .unwrap()
    }
}

fn profile(children: &[Child<'_>]) -> MicroVmVirtioFsProfile {
    MicroVmVirtioFsProfile::from_aggregate(
        MICROVM_ATTACHMENT_ID.to_owned(),
        children.iter().map(Child::profile).collect(),
    )
    .unwrap()
}

fn aggregate(children: &[Child<'_>]) -> VirtioFs {
    let roots = children.iter().map(|child| child.root).collect::<Vec<_>>();
    VirtioFs::new_microvm_aggregate(&roots, profile(children)).unwrap()
}

/// Two host directories, `work` and `tools`, each holding the file `file`.
struct Roots {
    _directory: TempDir,
    work: PathBuf,
    tools: PathBuf,
}

impl Roots {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let work = directory.path().join("work");
        let tools = directory.path().join("tools");
        for root in [&work, &tools] {
            std::fs::create_dir(root).unwrap();
            std::fs::write(root.join("file"), b"host").unwrap();
        }
        Self {
            _directory: directory,
            work,
            tools,
        }
    }

    fn entries(root: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

#[test]
fn synthetic_root_lists_only_the_children_and_refuses_changes() {
    let roots = Roots::new();
    let fs = aggregate(&[
        Child::new("work", &roots.work, false),
        Child::new("tools", &roots.tools, true),
    ]);
    let root = FUSE_ROOT_ID;

    let attr = fs.get_attr(&request(root), 0, 0).unwrap().attr;
    assert_eq!(attr.mode, lx::S_IFDIR | 0o500);
    assert_eq!((attr.uid, attr.gid), (0, 0));
    assert_eq!(list(&fs, root), ["tools", "work"]);
    assert_eq!(error(lookup(&fs, "missing")), lx::Error::ENOENT);

    assert_eq!(error(create(&fs, root, "new")), lx::Error::EROFS);
    assert_eq!(error(mkdir(&fs, root, "new")), lx::Error::EROFS);
    assert_eq!(
        error(fs.symlink(&request(root), name("link"), name("work"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.unlink(&request(root), name("work"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rmdir(&request(root), name("tools"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rename(&request(root), name("work"), root, name("moved"), 0)),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.set_attr(&request(root), &setattr(FATTR_MODE, |arg| arg.mode = 0o777))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.set_xattr(&request(root), name("user.nvx"), b"x", 0)),
        lx::Error::EROFS
    );
    // A microVM aggregate's children are fixed.
    assert_eq!(
        fs.add_child("more", &roots.work, None).unwrap_err(),
        lx::Error::EINVAL
    );
    assert_eq!(fs.remove_child("work").unwrap_err(), lx::Error::EINVAL);
    assert_eq!(list(&fs, root), ["tools", "work"]);
}

#[test]
fn children_enforce_their_own_access_modes() {
    let roots = Roots::new();
    let fs = aggregate(&[
        Child::new("work", &roots.work, false),
        Child::new("tools", &roots.tools, true),
    ]);
    let work = lookup(&fs, "work").unwrap();
    let tools = lookup(&fs, "tools").unwrap();
    assert_eq!(list(&fs, work), ["file"]);
    assert_eq!(list(&fs, tools), ["file"]);
    assert_eq!(read_node(&fs, lookup(&fs, "tools/file").unwrap()), b"host");

    let (file, fh) = create(&fs, work, "new").unwrap();
    write(&fs, file, fh, b"guest").unwrap();
    release(&fs, file, fh);
    let existing = lookup(&fs, "work/file").unwrap();
    let fh = open(&fs, existing, lx::O_RDWR).unwrap();
    write(&fs, existing, fh, b"GUEST").unwrap();
    release(&fs, existing, fh);

    let tools_file = lookup(&fs, "tools/file").unwrap();
    assert_eq!(error(create(&fs, tools, "new")), lx::Error::EROFS);
    assert_eq!(error(mkdir(&fs, tools, "new")), lx::Error::EROFS);
    assert_eq!(error(open(&fs, tools_file, lx::O_RDWR)), lx::Error::EROFS);
    assert_eq!(
        error(fs.unlink(&request(tools), name("file"))),
        lx::Error::EROFS
    );

    assert_eq!(std::fs::read(roots.work.join("new")).unwrap(), b"guest");
    assert_eq!(std::fs::read(roots.work.join("file")).unwrap(), b"GUEST");
    assert_eq!(Roots::entries(&roots.tools), ["file"]);
    assert_eq!(std::fs::read(roots.tools.join("file")).unwrap(), b"host");
}

#[test]
fn children_enforce_their_own_policies() {
    let roots = Roots::new();
    std::fs::create_dir(roots.work.join("secret")).unwrap();
    std::fs::create_dir(roots.tools.join("out")).unwrap();
    std::fs::create_dir(roots.tools.join("secret")).unwrap();
    let fs = aggregate(&[
        Child {
            denied: &["secret"],
            ..Child::new("work", &roots.work, false)
        },
        Child {
            writable: &["out"],
            ..Child::new("tools", &roots.tools, false)
        },
    ]);

    assert_eq!(error(lookup(&fs, "work/secret")), lx::Error::EACCES);
    assert_eq!(list(&fs, lookup(&fs, "work").unwrap()), ["file"]);
    // Another child's denied path does not apply.
    lookup(&fs, "tools/secret").unwrap();

    let tools = lookup(&fs, "tools").unwrap();
    assert_eq!(error(create(&fs, tools, "new")), lx::Error::EROFS);
    let out = lookup(&fs, "tools/out").unwrap();
    let (file, fh) = create(&fs, out, "new").unwrap();
    release(&fs, file, fh);
    let (file, fh) = create(&fs, lookup(&fs, "work").unwrap(), "new").unwrap();
    release(&fs, file, fh);
    assert_eq!(Roots::entries(&roots.tools.join("out")), ["new"]);
    assert_eq!(Roots::entries(&roots.tools), ["file", "out", "secret"]);
}

/// Renames between children fail with EXDEV, as between Linux filesystems,
/// even into a read-only child. Like Linux's linkat, a link first checks that
/// its destination is writable, and only a link into a writable directory of
/// another child fails with EXDEV.
#[test]
fn renames_and_links_between_children_fail() {
    let roots = Roots::new();
    let directory = tempdir().unwrap();
    let cache = directory.path().join("cache");
    let narrow = directory.path().join("narrow");
    std::fs::create_dir(&cache).unwrap();
    std::fs::create_dir_all(narrow.join("out")).unwrap();
    let fs = aggregate(&[
        Child::new("work", &roots.work, false),
        Child::new("tools", &roots.tools, false),
        Child::new("cache", &cache, true),
        Child {
            writable: &["out"],
            ..Child::new("narrow", &narrow, false)
        },
    ]);
    let work = lookup(&fs, "work").unwrap();
    let tools = lookup(&fs, "tools").unwrap();
    let cache_node = lookup(&fs, "cache").unwrap();
    let narrow_node = lookup(&fs, "narrow").unwrap();
    let narrow_out = lookup(&fs, "narrow/out").unwrap();
    let work_file = lookup(&fs, "work/file").unwrap();

    assert_eq!(
        error(fs.rename(&request(work), name("file"), tools, name("moved"), 0)),
        lx::Error::EXDEV
    );
    assert_eq!(
        error(fs.rename(&request(work), name("file"), cache_node, name("moved"), 0)),
        lx::Error::EXDEV
    );
    assert_eq!(
        error(fs.link(&request(tools), name("link"), work_file)),
        lx::Error::EXDEV
    );
    assert_eq!(
        error(fs.link(&request(narrow_out), name("link"), work_file)),
        lx::Error::EXDEV
    );
    // A read-only child, and a directory that a child's policy keeps
    // read-only, refuse the link before it would cross children.
    assert_eq!(
        error(fs.link(&request(cache_node), name("link"), work_file)),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.link(&request(narrow_node), name("link"), work_file)),
        lx::Error::EROFS
    );
    // Within a child, both still work.
    fs.link(&request(work), name("link"), work_file).unwrap();
    fs.rename(&request(work), name("link"), work, name("renamed"), 0)
        .unwrap();

    assert_eq!(Roots::entries(&roots.work), ["file", "renamed"]);
    assert_eq!(Roots::entries(&roots.tools), ["file"]);
    assert!(Roots::entries(&cache).is_empty());
    assert_eq!(Roots::entries(&narrow), ["out"]);
    assert!(Roots::entries(&narrow.join("out")).is_empty());
}

#[test]
fn children_report_distinct_inode_numbers() {
    let roots = Roots::new();
    let fs = aggregate(&[
        Child::new("work", &roots.work, true),
        Child::new("tools", &roots.tools, true),
    ]);
    let ino = |path: &str| {
        let node_id = lookup(&fs, path).unwrap();
        fs.get_attr(&request(node_id), 0, 0).unwrap().attr.ino
    };
    let inodes = [
        FUSE_ROOT_ID,
        ino("work"),
        ino("tools"),
        ino("work/file"),
        ino("tools/file"),
    ];
    for (index, inode) in inodes.iter().enumerate() {
        assert!(!inodes[..index].contains(inode), "{inodes:?}");
    }
    // Repeated lookups reach the same node.
    assert_eq!(lookup(&fs, "work").unwrap(), lookup(&fs, "work").unwrap());
}

#[test]
fn aggregate_rejects_a_root_that_is_not_its_child() {
    let roots = Roots::new();
    let profile = profile(&[
        Child::new("work", &roots.work, false),
        Child::new("tools", &roots.tools, true),
    ]);
    assert!(
        VirtioFs::new_microvm_aggregate(&[&roots.tools, &roots.work], profile.clone()).is_err()
    );
    assert!(VirtioFs::new_microvm_aggregate(&[&roots.work], profile.clone()).is_err());
    assert!(VirtioFs::new_microvm(&roots.work, profile).is_err());
}
