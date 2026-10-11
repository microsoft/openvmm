// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for `RENAME_EXCHANGE` on microVM shares: the names that HostFs keeps
//! for each node, the alias limits, and snapshots taken right after an
//! exchange.

use super::policy_tests::error;
use super::policy_tests::list;
use super::policy_tests::lookup;
use super::policy_tests::mkdir;
use super::policy_tests::name;
use super::policy_tests::open;
use super::policy_tests::read_node;
use super::policy_tests::release;
use super::policy_tests::request;
use super::policy_tests::write;
use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use super::saved_state::MAX_PATH_BYTES;
use super::saved_state::SavedState;
use super::state::decode_relative_path;
use super::state::relative_path_encoded_len;
use crate::RENAME_EXCHANGE;
use crate::VirtioFs;
use fuse::Fuse;
use fuse::Session;
use fuse::SessionState;
use fuse::protocol::FUSE_ROOT_ID;
use std::collections::BTreeSet;
use std::path::PathBuf;
use tempfile::TempDir;
use tempfile::tempdir;
use test_with_tracing::test;

/// A read-write microVM share without an access policy.
struct Share {
    _directory: TempDir,
    root: PathBuf,
    profile: MicroVmVirtioFsProfile,
}

impl Share {
    /// Creates the share with `files`, and the directories that hold them.
    fn new(files: &[(&str, &str)]) -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path().join("share");
        std::fs::create_dir(&root).unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(&root).unwrap(),
            false,
            Vec::new(),
        )
        .unwrap();
        let share = Self {
            _directory: directory,
            root,
            profile,
        };
        for (path, contents) in files {
            let path = share.path(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        share
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative_path(relative))
    }

    fn read(&self, relative: &str) -> String {
        String::from_utf8(std::fs::read(self.path(relative)).unwrap()).unwrap()
    }

    fn fs(&self) -> VirtioFs {
        VirtioFs::new_microvm(&self.root, self.profile.clone()).unwrap()
    }

    fn save(&self, fs: &VirtioFs) -> SavedState {
        fs.save_microvm_state(&self.profile, SessionState::default())
            .unwrap()
    }

    /// Restores `state` into a new attachment of the share.
    fn restore(&self, state: SavedState) -> VirtioFs {
        let fs = self.fs();
        let session = Session::new(fs.clone());
        fs.restore_microvm_state(&self.profile, state, &session)
            .unwrap();
        fs
    }
}

fn relative_path(path: &str) -> PathBuf {
    path.split('/').collect()
}

fn path_set(paths: &[&str]) -> BTreeSet<PathBuf> {
    paths.iter().map(|path| relative_path(path)).collect()
}

/// Exchanges two entries with `RENAME_EXCHANGE`, and returns whether the host
/// exchanged them. Linux hosts pass the flag to `renameat2`; Windows hosts
/// reject every rename flag with `EINVAL` before they change anything.
fn exchange(fs: &VirtioFs, parent: u64, first: &str, new_parent: u64, second: &str) -> bool {
    let result = fs.rename(
        &request(parent),
        name(first),
        new_parent,
        name(second),
        RENAME_EXCHANGE,
    );
    if cfg!(windows) {
        assert_eq!(error(result), lx::Error::EINVAL);
        false
    } else {
        result.unwrap();
        true
    }
}

/// Asserts that HostFs knows `node_id` by exactly `names`, and that the path
/// that the node's operations use is one of them.
fn assert_names(fs: &VirtioFs, node_id: u64, names: &[&str]) {
    let inode = fs.get_inode(node_id).unwrap();
    let expected = path_set(names);
    let aliases: BTreeSet<_> = inode.aliases().into_iter().collect();
    assert_eq!(aliases, expected, "aliases of node {node_id}");
    assert!(
        expected.contains(&inode.clone_path()),
        "path of node {node_id}: {:?}",
        inode.clone_path()
    );
}

/// Returns the names that `state` records for `node_id`.
fn saved_names(state: &SavedState, node_id: u64) -> BTreeSet<PathBuf> {
    state
        .inodes
        .iter()
        .find(|inode| inode.node_id == node_id)
        .unwrap()
        .relative_aliases
        .iter()
        .map(|alias| decode_relative_path(alias).unwrap())
        .collect()
}

/// After two files trade names, each node reaches its own object through every
/// name that the guest knows for it, including a hard link that the exchange
/// leaves in place, and a snapshot taken right away saves and restores both
/// nodes and a handle that was open across the exchange.
#[test]
fn exchanged_files_keep_their_nodes_through_save_and_restore() {
    let share = Share::new(&[("first", "first"), ("second", "second")]);
    std::fs::hard_link(share.path("first"), share.path("first-link")).unwrap();
    let fs = share.fs();
    let first = lookup(&fs, "first").unwrap();
    assert_eq!(lookup(&fs, "first-link").unwrap(), first);
    let second = lookup(&fs, "second").unwrap();
    let fh = open(&fs, first, lx::O_RDWR).unwrap();

    let exchanged = exchange(&fs, FUSE_ROOT_ID, "first", FUSE_ROOT_ID, "second");
    let (first_names, second_names): (&[&str], &[&str]) = if exchanged {
        (&["first-link", "second"], &["first"])
    } else {
        (&["first", "first-link"], &["second"])
    };
    let nodes = [
        (first, first_names, "first"),
        (second, second_names, "second"),
    ];
    let check = |fs: &VirtioFs| {
        for (node, names, contents) in nodes {
            for name in names {
                assert_eq!(share.read(name), contents, "{name}");
            }
            assert_names(fs, node, names);
            assert_eq!(read_node(fs, node), contents.as_bytes());
        }
    };
    check(&fs);

    let state = share.save(&fs);
    for (node, names, _) in nodes {
        assert_eq!(saved_names(&state, node), path_set(names));
    }
    let restored = share.restore(state);
    check(&restored);

    // The handle still reaches the object that it opened.
    write(&restored, first, fh, b"written").unwrap();
    release(&restored, first, fh);
    for name in first_names {
        assert_eq!(share.read(name), "written", "{name}");
    }
    // A new lookup of each name finds the node of the object there.
    let (at_first, at_second) = if exchanged {
        (second, first)
    } else {
        (first, second)
    };
    assert_eq!(lookup(&restored, "first").unwrap(), at_first);
    assert_eq!(lookup(&restored, "second").unwrap(), at_second);
}

/// After two directories in different parents trade names, the nodes of the
/// directories and of the entries below them follow their objects, and a
/// snapshot taken right away saves and restores all of them.
#[test]
fn exchanged_directories_carry_their_nodes_through_save_and_restore() {
    let share = Share::new(&[
        ("one/left/inner", "left"),
        ("two/right/inner", "right"),
        ("two/right/nested/leaf", "leaf"),
    ]);
    let fs = share.fs();
    let one = lookup(&fs, "one").unwrap();
    let two = lookup(&fs, "two").unwrap();
    let left = lookup(&fs, "one/left").unwrap();
    let left_inner = lookup(&fs, "one/left/inner").unwrap();
    let right = lookup(&fs, "two/right").unwrap();
    let right_inner = lookup(&fs, "two/right/inner").unwrap();
    let nested = lookup(&fs, "two/right/nested").unwrap();
    let leaf = lookup(&fs, "two/right/nested/leaf").unwrap();

    let exchanged = exchange(&fs, one, "left", two, "right");
    let (left_name, right_name) = if exchanged {
        ("two/right", "one/left")
    } else {
        ("one/left", "two/right")
    };
    let directories = [
        (one, "one".to_owned()),
        (two, "two".to_owned()),
        (left, left_name.to_owned()),
        (right, right_name.to_owned()),
        (nested, format!("{right_name}/nested")),
    ];
    let files = [
        (left_inner, format!("{left_name}/inner"), "left"),
        (right_inner, format!("{right_name}/inner"), "right"),
        (leaf, format!("{right_name}/nested/leaf"), "leaf"),
    ];
    let check = |fs: &VirtioFs| {
        for (node, name) in &directories {
            assert_names(fs, *node, &[name]);
        }
        for (node, name, contents) in &files {
            assert_eq!(share.read(name), *contents, "{name}");
            assert_names(fs, *node, &[name]);
            assert_eq!(read_node(fs, *node), contents.as_bytes());
        }
        assert_eq!(list(fs, left), ["inner"]);
        assert_eq!(list(fs, right), ["inner", "nested"]);
    };
    check(&fs);

    let restored = share.restore(share.save(&fs));
    check(&restored);
    let at_left_inner = if exchanged { right_inner } else { left_inner };
    assert_eq!(lookup(&restored, "one/left/inner").unwrap(), at_left_inner);
}

/// The alias limits apply to the names that an exchange gives both objects.
/// An exchange that would give a node below the destination a name longer
/// than the limit fails before the host changes anything, although a one-way
/// rename would drop that node's names instead.
#[test]
fn exchange_checks_the_alias_limits_of_both_names() {
    let share = Share::new(&[]);
    let fs = share.fs();
    // Windows counts the limit in UTF-16 units, so measure as HostFs does.
    let encoded_len = |path: &str| relative_path_encoded_len(&relative_path(path)).unwrap();
    let component = "d".repeat(200);
    let destination = mkdir(&fs, FUSE_ROOT_ID, "a").unwrap();
    let mut deepest = destination;
    let mut deepest_path = String::from("a");
    loop {
        let path = format!("{deepest_path}/{component}");
        if encoded_len(&path) > MAX_PATH_BYTES {
            break;
        }
        deepest = mkdir(&fs, deepest, &component).unwrap();
        deepest_path = path;
    }
    let source = "b".repeat(250);
    mkdir(&fs, FUSE_ROOT_ID, &source).unwrap();
    let exchanged_path = format!("{source}{}", &deepest_path[1..]);
    assert!(encoded_len(&exchanged_path) > MAX_PATH_BYTES);

    assert_eq!(
        error(fs.rename(
            &request(FUSE_ROOT_ID),
            name(&source),
            FUSE_ROOT_ID,
            name("a"),
            RENAME_EXCHANGE,
        )),
        lx::Error::E2BIG
    );
    assert_names(&fs, deepest, &[&deepest_path]);
    assert_eq!(lookup(&fs, "a").unwrap(), destination);
    assert_eq!(
        std::fs::read_dir(share.path("a"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        [component.as_str()]
    );
    assert!(
        std::fs::read_dir(share.path(&source))
            .unwrap()
            .next()
            .is_none()
    );
}
