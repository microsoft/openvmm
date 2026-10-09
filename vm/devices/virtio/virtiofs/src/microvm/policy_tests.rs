// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the access policy of microVM shares: write narrowing, hidden
//! subtrees, and the allowed paths inside them.

use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use super::saved_state::SCHEMA_VERSION;
use super::saved_state::SUBTREE_POLICY_SCHEMA_VERSION;
use super::state::save_dormant_microvm_state;
use super::state::validate_dormant_microvm_state;
use super::state::validate_microvm_state;
use crate::RENAME_EXCHANGE;
use crate::VirtioFs;
use fuse::Fuse;
use fuse::Request;
use fuse::Session;
use fuse::SessionState;
use fuse::protocol::FATTR_MODE;
use fuse::protocol::FATTR_SIZE;
use fuse::protocol::FUSE_LOOKUP;
use fuse::protocol::FUSE_ROOT_ID;
use fuse::protocol::fuse_create_in;
use fuse::protocol::fuse_in_header;
use fuse::protocol::fuse_mkdir_in;
use fuse::protocol::fuse_read_in;
use fuse::protocol::fuse_release_in;
use fuse::protocol::fuse_setattr_in;
use fuse::protocol::fuse_write_in;
use std::path::PathBuf;
use tempfile::TempDir;
use tempfile::tempdir;
use test_with_tracing::test;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

pub(super) fn request(node_id: u64) -> Request {
    let header = fuse_in_header {
        len: size_of::<fuse_in_header>() as u32,
        opcode: FUSE_LOOKUP,
        unique: 1,
        nodeid: node_id,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    Request::new(header.as_bytes()).unwrap()
}

pub(super) fn name(value: &str) -> &lx::LxStr {
    lx::LxStr::from_bytes(value.as_bytes())
}

pub(super) fn error<T>(result: lx::Result<T>) -> lx::Error {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => error,
    }
}

pub(super) fn owned(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

pub(super) fn setattr(valid: u32, update: impl FnOnce(&mut fuse_setattr_in)) -> fuse_setattr_in {
    let mut arg = fuse_setattr_in::new_zeroed();
    arg.valid = valid;
    update(&mut arg);
    arg
}

/// Looks `path` up one component at a time, as the guest kernel does, and
/// returns its node ID.
pub(super) fn lookup(fs: &VirtioFs, path: &str) -> lx::Result<u64> {
    let mut node_id = FUSE_ROOT_ID;
    for component in path.split('/') {
        node_id = fs.lookup(&request(node_id), name(component))?.nodeid;
    }
    Ok(node_id)
}

pub(super) fn open(fs: &VirtioFs, node_id: u64, flags: i32) -> lx::Result<u64> {
    Ok(fs.open(&request(node_id), flags as u32)?.fh)
}

pub(super) fn release(fs: &VirtioFs, node_id: u64, fh: u64) {
    let mut arg = fuse_release_in::new_zeroed();
    arg.fh = fh;
    fs.release(&request(node_id), &arg).unwrap();
}

/// Creates and opens a file, and returns its node ID and handle.
pub(super) fn create(fs: &VirtioFs, parent: u64, file_name: &str) -> lx::Result<(u64, u64)> {
    let arg = fuse_create_in {
        flags: lx::O_RDWR as u32,
        mode: lx::S_IFREG | 0o644,
        umask: 0,
        padding: 0,
    };
    let created = fs.create(&request(parent), name(file_name), &arg)?;
    Ok((created.entry.nodeid, created.open.fh))
}

pub(super) fn mkdir(fs: &VirtioFs, parent: u64, directory_name: &str) -> lx::Result<u64> {
    let arg = fuse_mkdir_in {
        mode: 0o755,
        umask: 0,
    };
    Ok(fs
        .mkdir(&request(parent), name(directory_name), &arg)?
        .nodeid)
}

pub(super) fn write(fs: &VirtioFs, node_id: u64, fh: u64, data: &[u8]) -> lx::Result<usize> {
    let mut arg = fuse_write_in::new_zeroed();
    arg.fh = fh;
    arg.size = data.len() as u32;
    fs.write(&request(node_id), &arg, data)
}

/// Returns up to 64 bytes from the start of a file.
pub(super) fn read_node(fs: &VirtioFs, node_id: u64) -> Vec<u8> {
    let fh = open(fs, node_id, lx::O_RDONLY).unwrap();
    let mut arg = fuse_read_in::new_zeroed();
    arg.fh = fh;
    arg.size = 64;
    let data = fs.read(&request(node_id), &arg).unwrap();
    release(fs, node_id, fh);
    data
}

/// Returns the names that the guest lists in a directory, other than `.` and
/// `..`, sorted.
pub(super) fn list(fs: &VirtioFs, node_id: u64) -> Vec<String> {
    let fh = fs
        .open_dir(&request(node_id), lx::O_RDONLY as u32)
        .unwrap()
        .fh;
    let mut arg = fuse_read_in::new_zeroed();
    arg.fh = fh;
    arg.size = 4096;
    let buffer = fs.read_dir(&request(node_id), &arg).unwrap();
    let mut names = Vec::new();
    let mut offset = 0;
    while offset < buffer.len() {
        // A `fuse_dirent` header is the inode number, the next offset, the
        // name length, and the type, followed by the name, padded to eight
        // bytes.
        let header = &buffer[offset..offset + 24];
        let length = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
        let entry = &buffer[offset + 24..offset + 24 + length];
        let entry = String::from_utf8(entry.to_vec()).unwrap();
        if entry != "." && entry != ".." {
            names.push(entry);
        }
        offset += fuse::protocol::fuse_dirent_align(24 + length);
    }
    release(fs, node_id, fh);
    names.sort();
    names
}

/// A share laid out like the directories that a sandbox policy protects.
///
/// - `marker` and `build.log` are files, and `out` is a directory with the
///   file `existing`.
/// - `logs` holds the secret file `secret` and the directory `gateway`, and
///   `logs/payloads` holds the file `payload` and the directory `private`.
struct Share {
    _directory: TempDir,
    root: PathBuf,
}

impl Share {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path().join("share");
        let share = Self {
            _directory: directory,
            root,
        };
        for path in ["out", "logs/gateway", "logs/payloads/private"] {
            std::fs::create_dir_all(share.path(path)).unwrap();
        }
        for (path, contents) in [
            ("marker", "host"),
            ("build.log", "log"),
            ("out/existing", "existing"),
            ("logs/secret", "secret"),
            ("logs/gateway/log", "gateway"),
            ("logs/payloads/payload", "payload"),
            ("logs/payloads/private/key", "key"),
        ] {
            std::fs::write(share.path(path), contents).unwrap();
        }
        share
    }

    fn path(&self, relative: &str) -> PathBuf {
        relative
            .split('/')
            .fold(self.root.clone(), |path, component| path.join(component))
    }

    fn read(&self, relative: &str) -> String {
        String::from_utf8(std::fs::read(self.path(relative)).unwrap()).unwrap()
    }

    fn entries(&self, relative: &str) -> Vec<String> {
        let mut names = std::fs::read_dir(self.path(relative))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn profile(
        &self,
        denied: &[&str],
        allowed: &[&str],
        writable: &[&str],
    ) -> MicroVmVirtioFsProfile {
        MicroVmVirtioFsProfile::from_attachment_with_policy(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(&self.root).unwrap(),
            false,
            owned(denied),
            owned(allowed),
            owned(writable),
        )
        .unwrap()
    }

    fn fs(&self, denied: &[&str], allowed: &[&str], writable: &[&str]) -> VirtioFs {
        VirtioFs::new_microvm(&self.root, self.profile(denied, allowed, writable)).unwrap()
    }

    /// Checks that nothing outside `out` and `build.log` changed.
    fn assert_protected_paths_unchanged(&self) {
        assert_eq!(self.entries(""), ["build.log", "logs", "marker", "out"]);
        assert_eq!(self.read("marker"), "host");
        assert_eq!(self.entries("logs"), ["gateway", "payloads", "secret"]);
        assert_eq!(self.read("logs/secret"), "secret");
        assert_eq!(self.read("logs/gateway/log"), "gateway");
        assert_eq!(self.read("logs/payloads/private/key"), "key");
    }
}

#[test]
fn writes_succeed_only_under_writable_paths() {
    let share = Share::new();
    let fs = share.fs(&[], &[], &["build.log", "out"]);
    let root = FUSE_ROOT_ID;
    let marker = lookup(&fs, "marker").unwrap();
    let out = lookup(&fs, "out").unwrap();
    let existing = lookup(&fs, "out/existing").unwrap();

    // Outside the writable paths, every mutation fails with EROFS.
    assert_eq!(error(create(&fs, root, "new")), lx::Error::EROFS);
    assert_eq!(error(mkdir(&fs, root, "new")), lx::Error::EROFS);
    assert_eq!(
        error(fs.symlink(&request(root), name("link"), name("marker"))),
        lx::Error::EROFS
    );
    assert_eq!(error(open(&fs, marker, lx::O_RDWR)), lx::Error::EROFS);
    assert_eq!(
        error(open(&fs, marker, lx::O_WRONLY | lx::O_TRUNC)),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.set_attr(&request(marker), &setattr(FATTR_SIZE, |arg| arg.size = 0))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.set_attr(
            &request(marker),
            &setattr(FATTR_MODE, |arg| arg.mode = 0o600)
        )),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.set_xattr(&request(marker), name("user.nvx"), b"x", 0)),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.remove_xattr(&request(marker), name("user.nvx"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.unlink(&request(root), name("marker"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rmdir(&request(root), name("out"))),
        lx::Error::EROFS
    );
    // Moving an entry into or out of a writable path changes a read-only
    // directory.
    assert_eq!(
        error(fs.rename(&request(root), name("marker"), out, name("marker"), 0)),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rename(&request(out), name("existing"), root, name("existing"), 0)),
        lx::Error::EROFS
    );
    // A hard link in a writable path would make a read-only file writable.
    assert_eq!(
        error(fs.link(&request(out), name("marker-link"), marker)),
        lx::Error::EXDEV
    );
    assert_eq!(
        error(fs.link(&request(root), name("existing-link"), existing)),
        lx::Error::EROFS
    );

    // Inside them, the guest can create, modify, rename, link, and remove
    // entries.
    let (file, fh) = create(&fs, out, "file").unwrap();
    write(&fs, file, fh, b"guest").unwrap();
    release(&fs, file, fh);
    let directory = mkdir(&fs, out, "directory").unwrap();
    fs.rename(&request(out), name("file"), directory, name("renamed"), 0)
        .unwrap();
    fs.link(&request(out), name("hard"), existing).unwrap();
    fs.unlink(&request(out), name("hard")).unwrap();
    fs.set_attr(&request(existing), &setattr(FATTR_SIZE, |arg| arg.size = 0))
        .unwrap();
    fs.symlink(&request(out), name("link"), name("../marker"))
        .unwrap();
    // A writable file can change, but not move, because its directory is
    // read-only.
    let log = lookup(&fs, "build.log").unwrap();
    let fh = open(&fs, log, lx::O_RDWR).unwrap();
    write(&fs, log, fh, b"guest").unwrap();
    release(&fs, log, fh);
    assert_eq!(
        error(fs.unlink(&request(root), name("build.log"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rename(&request(root), name("build.log"), out, name("build.log"), 0)),
        lx::Error::EROFS
    );

    assert_eq!(share.read("out/directory/renamed"), "guest");
    assert_eq!(share.read("out/existing"), "");
    assert_eq!(share.entries("out"), ["directory", "existing", "link"]);
    assert_eq!(share.read("build.log"), "guest");
    share.assert_protected_paths_unchanged();
}

#[test]
fn allowed_paths_stay_readable_inside_a_hidden_subtree() {
    let share = Share::new();
    let fs = share.fs(&["logs", "logs/payloads/private"], &["logs/payloads"], &[]);
    let root = FUSE_ROOT_ID;

    // The denied directory leads to an allowed path, so the guest can list
    // and traverse it, but it lists only the way to the allowed path.
    assert_eq!(list(&fs, root), ["build.log", "logs", "marker", "out"]);
    let logs = lookup(&fs, "logs").unwrap();
    assert_eq!(list(&fs, logs), ["payloads"]);
    for hidden in [
        "logs/secret",
        "logs/gateway",
        "logs/gateway/log",
        "logs/missing",
        "logs/payloads/private",
        "logs/payloads/private/key",
    ] {
        assert_eq!(error(lookup(&fs, hidden)), lx::Error::EACCES, "{hidden}");
    }
    let payloads = lookup(&fs, "logs/payloads").unwrap();
    assert_eq!(list(&fs, payloads), ["payload"]);
    let payload = lookup(&fs, "logs/payloads/payload").unwrap();
    assert_eq!(read_node(&fs, payload), b"payload");

    // The share is read-write and has no writable paths, so the allowed path
    // is writable.
    let (spilled, fh) = create(&fs, payloads, "spilled").unwrap();
    release(&fs, spilled, fh);

    // Nothing can change the traverse-only directory or its entries.
    assert_eq!(error(create(&fs, logs, "new")), lx::Error::EROFS);
    assert_eq!(error(mkdir(&fs, logs, "new")), lx::Error::EROFS);
    assert_eq!(
        error(fs.unlink(&request(logs), name("secret"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rmdir(&request(logs), name("payloads"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rename(&request(logs), name("payloads"), root, name("moved"), 0)),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.set_attr(&request(logs), &setattr(FATTR_MODE, |arg| arg.mode = 0o777))),
        lx::Error::EROFS
    );
    assert_eq!(error(open(&fs, logs, lx::O_RDWR)), lx::Error::EROFS);
    // Its writable parent cannot remove, move, or replace it, so another
    // directory cannot take its place and expose its hidden entries.
    assert_eq!(
        error(fs.rmdir(&request(root), name("logs"))),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.rename(&request(root), name("logs"), root, name("moved"), 0)),
        lx::Error::EROFS
    );
    mkdir(&fs, root, "other").unwrap();
    assert_eq!(
        error(fs.rename(
            &request(root),
            name("other"),
            root,
            name("logs"),
            RENAME_EXCHANGE
        )),
        lx::Error::EROFS
    );
    assert_eq!(
        error(fs.symlink(&request(root), name("logs"), name("out"))),
        lx::Error::EROFS
    );

    assert_eq!(
        share.entries("logs/payloads"),
        ["payload", "private", "spilled"]
    );
    assert_eq!(
        share.entries(""),
        ["build.log", "logs", "marker", "other", "out"]
    );
    std::fs::remove_dir(share.path("other")).unwrap();
    std::fs::remove_file(share.path("logs/payloads/spilled")).unwrap();
    share.assert_protected_paths_unchanged();
}

#[test]
fn a_hidden_share_root_exposes_only_its_allowed_paths() {
    let share = Share::new();
    let fs = share.fs(&[""], &["build.log", "logs/payloads"], &[]);
    let root = FUSE_ROOT_ID;
    assert_eq!(list(&fs, root), ["build.log", "logs"]);
    assert_eq!(list(&fs, lookup(&fs, "logs").unwrap()), ["payloads"]);
    for hidden in ["marker", "out", "logs/secret", "missing"] {
        assert_eq!(error(lookup(&fs, hidden)), lx::Error::EACCES, "{hidden}");
    }
    assert_eq!(read_node(&fs, lookup(&fs, "build.log").unwrap()), b"log");
    assert_eq!(error(create(&fs, root, "new")), lx::Error::EROFS);
    assert_eq!(
        error(fs.unlink(&request(root), name("build.log"))),
        lx::Error::EROFS
    );
    // The share is read-write, so its allowed paths are writable.
    let payloads = lookup(&fs, "logs/payloads").unwrap();
    let (spilled, fh) = create(&fs, payloads, "spilled").unwrap();
    release(&fs, spilled, fh);
    std::fs::remove_file(share.path("logs/payloads/spilled")).unwrap();
    share.assert_protected_paths_unchanged();

    // A hidden root needs allowed paths.
    assert!(
        MicroVmVirtioFsProfile::from_attachment_with_policy(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(&share.root).unwrap(),
            false,
            owned(&[""]),
            Vec::new(),
            Vec::new(),
        )
        .is_err()
    );
}

#[test]
fn policy_names_may_contain_spaces() {
    let share = Share::new();
    for path in ["Program Files/My Tool/out dir", "Program Files/Other Tool"] {
        std::fs::create_dir_all(share.path(path)).unwrap();
    }
    let fs = share.fs(
        &["Program Files"],
        &["Program Files/My Tool"],
        &["Program Files/My Tool/out dir"],
    );
    assert_eq!(
        list(&fs, lookup(&fs, "Program Files").unwrap()),
        ["My Tool"]
    );
    assert_eq!(
        error(lookup(&fs, "Program Files/Other Tool")),
        lx::Error::EACCES
    );
    let tool = lookup(&fs, "Program Files/My Tool").unwrap();
    assert_eq!(error(create(&fs, tool, "new")), lx::Error::EROFS);
    let out = lookup(&fs, "Program Files/My Tool/out dir").unwrap();
    let (file, fh) = create(&fs, out, "new file").unwrap();
    release(&fs, file, fh);
    assert!(
        share
            .path("Program Files/My Tool/out dir/new file")
            .exists()
    );
}

#[test]
fn allowed_paths_follow_the_write_policy() {
    let share = Share::new();
    let fs = share.fs(&["logs"], &["logs/payloads"], &["out"]);
    let payloads = lookup(&fs, "logs/payloads").unwrap();
    let payload = lookup(&fs, "logs/payloads/payload").unwrap();
    assert_eq!(error(create(&fs, payloads, "spilled")), lx::Error::EROFS);
    assert_eq!(error(open(&fs, payload, lx::O_RDWR)), lx::Error::EROFS);

    let fs = share.fs(&["logs"], &["logs/payloads"], &["logs/payloads"]);
    let payload = lookup(&fs, "logs/payloads/payload").unwrap();
    let fh = open(&fs, payload, lx::O_RDWR).unwrap();
    write(&fs, payload, fh, b"updated").unwrap();
    release(&fs, payload, fh);
    assert_eq!(error(create(&fs, FUSE_ROOT_ID, "new")), lx::Error::EROFS);
    assert_eq!(share.read("logs/payloads/payload"), "updated");
}

/// Filesystems may report an entry's type as unknown, so HostFs asks the host
/// whether a traverse-only entry is a directory before listing it.
#[test]
fn traverse_only_entries_of_unknown_type_are_resolved_on_the_host() {
    let share = Share::new();
    let fs = share.fs(&["logs"], &["logs/payloads/private"], &[]);
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let listable = |path: &str, file_type| {
        root.volume
            .entry_listable(&path.split('/').collect::<PathBuf>(), file_type)
    };
    for (path, expected) in [
        ("marker", true),
        ("logs", true),
        ("logs/secret", false),
        ("logs/payloads", true),
        ("logs/payloads/payload", false),
        ("logs/payloads/private", true),
    ] {
        assert_eq!(listable(path, lx::DT_UNK), expected, "{path}");
    }

    // A traverse-only path that the host replaced with a file is not listed.
    std::fs::rename(share.path("logs/payloads"), share.path("logs/moved")).unwrap();
    std::fs::write(share.path("logs/payloads"), "file").unwrap();
    assert!(!listable("logs/payloads", lx::DT_UNK));
    assert!(!listable("logs/payloads", lx::DT_REG));
}

/// A successful exchange swaps the names that the guest knows for the two
/// objects, so each node still reaches its own object.
#[cfg(unix)]
#[test]
fn exchanged_entries_keep_their_nodes() {
    let share = Share::new();
    std::fs::create_dir(share.path("out/directory")).unwrap();
    std::fs::write(share.path("out/directory/file"), "nested").unwrap();
    let fs = share.fs(&[], &[], &["out"]);
    let out = lookup(&fs, "out").unwrap();
    let existing = lookup(&fs, "out/existing").unwrap();
    let directory = lookup(&fs, "out/directory").unwrap();
    let nested = lookup(&fs, "out/directory/file").unwrap();

    fs.rename(
        &request(out),
        name("existing"),
        out,
        name("directory"),
        RENAME_EXCHANGE,
    )
    .unwrap();
    assert_eq!(share.read("out/directory"), "existing");
    assert_eq!(share.read("out/existing/file"), "nested");

    assert_eq!(read_node(&fs, existing), b"existing");
    assert_eq!(read_node(&fs, nested), b"nested");
    assert_eq!(list(&fs, directory), ["file"]);
    let fh = open(&fs, nested, lx::O_RDWR | lx::O_TRUNC).unwrap();
    write(&fs, nested, fh, b"guest").unwrap();
    release(&fs, nested, fh);
    assert_eq!(share.read("out/existing/file"), "guest");
    assert_eq!(lookup(&fs, "out/directory").unwrap(), existing);
    assert_eq!(lookup(&fs, "out/existing").unwrap(), directory);
    share.assert_protected_paths_unchanged();
}

#[test]
fn hidden_objects_stay_hidden_at_other_paths() {
    let share = Share::new();
    let fs = share.fs(&["logs", "marker"], &["logs/payloads"], &[]);
    lookup(&fs, "logs/payloads/payload").unwrap();

    // A hidden file stays hidden through a hard link.
    std::fs::hard_link(share.path("marker"), share.path("alias")).unwrap();
    assert_eq!(error(lookup(&fs, "alias")), lx::Error::EACCES);

    // A traverse-only directory that the host moves is reachable only at its
    // own path, so its hidden entries stay hidden.
    std::fs::rename(share.path("logs"), share.path("moved")).unwrap();
    assert_eq!(error(lookup(&fs, "moved")), lx::Error::EACCES);
    assert_eq!(error(lookup(&fs, "moved/secret")), lx::Error::EACCES);

    // Anything but a directory at a traverse-only path is hidden.
    std::fs::write(share.path("logs"), "file").unwrap();
    assert_eq!(error(lookup(&fs, "logs")), lx::Error::EACCES);
    assert!(!list(&fs, FUSE_ROOT_ID).contains(&"logs".to_owned()));
    std::fs::remove_file(share.path("logs")).unwrap();
    std::fs::rename(share.path("moved"), share.path("logs")).unwrap();
    std::fs::remove_file(share.path("alias")).unwrap();
    share.assert_protected_paths_unchanged();
}

#[test]
fn attaching_requires_directories_on_the_way_to_allowed_paths() {
    let share = Share::new();
    for allowed in ["logs/secret/inner", "logs/missing/inner"] {
        let profile = share.profile(&["logs"], &[allowed], &[]);
        assert!(
            VirtioFs::new_microvm(&share.root, profile).is_err(),
            "{allowed}"
        );
    }
}

#[test]
fn access_policy_survives_save_and_restore() {
    let share = Share::new();
    let profile = share.profile(&["logs"], &["logs/payloads"], &["out"]);
    let source = VirtioFs::new_microvm(&share.root, profile.clone()).unwrap();
    let existing = lookup(&source, "out/existing").unwrap();
    let fh = open(&source, existing, lx::O_RDWR).unwrap();
    let logs = lookup(&source, "logs").unwrap();
    let state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    assert_eq!(state.schema_version, SUBTREE_POLICY_SCHEMA_VERSION);
    assert_eq!(state.denied_paths, ["logs"]);
    assert_eq!(state.allowed_paths, ["logs/payloads"]);
    assert_eq!(state.writable_paths, ["out"]);

    // The restore attachment needs exactly the same policy.
    for other in [
        share.profile(&["logs"], &["logs/payloads"], &[]),
        share.profile(&["logs"], &[], &["out"]),
        share.profile(&[], &[], &[]),
    ] {
        assert!(validate_microvm_state(&state, &other).is_err());
    }
    let destination = VirtioFs::new_microvm(&share.root, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap();
    write(&destination, existing, fh, b"restored").unwrap();
    assert_eq!(
        error(create(&destination, FUSE_ROOT_ID, "new")),
        lx::Error::EROFS
    );
    assert_eq!(list(&destination, logs), ["payloads"]);
    assert_eq!(share.read("out/existing"), "restored");
}

#[test]
fn restore_rejects_a_writable_handle_outside_the_writable_paths() {
    let share = Share::new();
    let profile = share.profile(&[], &[], &["out"]);
    let source = VirtioFs::new_microvm(&share.root, profile.clone()).unwrap();
    let marker = lookup(&source, "marker").unwrap();
    open(&source, marker, lx::O_RDONLY).unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    validate_microvm_state(&state, &profile).unwrap();

    let handle = state
        .handles
        .iter_mut()
        .find(|handle| handle.node_id == marker)
        .unwrap();
    handle.open_flags = lx::O_RDWR as u32 | lx::O_NOFOLLOW as u32;
    assert!(validate_microvm_state(&state, &profile).is_err());
}

#[test]
fn saved_state_records_only_policies_that_restrict_writes() {
    let share = Share::new();
    // A deny-only attachment keeps the earlier schema, so that earlier
    // releases, which enforce denied paths, can restore it.
    let deny_only = share.profile(&["logs"], &[], &[]);
    let source = VirtioFs::new_microvm(&share.root, deny_only.clone()).unwrap();
    let mut state = source
        .save_microvm_state(&deny_only, SessionState::default())
        .unwrap();
    assert_eq!(state.schema_version, SCHEMA_VERSION);
    assert!(state.denied_paths.is_empty() && state.allowed_paths.is_empty());
    validate_microvm_state(&state, &deny_only).unwrap();

    // A state without a policy cannot restore an attachment with one, and an
    // earlier schema cannot carry one.
    let narrowed = share.profile(&[], &[], &["out"]);
    assert!(validate_microvm_state(&state, &narrowed).is_err());
    state.writable_paths = owned(&["out"]);
    assert!(validate_microvm_state(&state, &deny_only).is_err());
    assert!(validate_microvm_state(&state, &narrowed).is_err());

    // A dormant slot carries no policy.
    let mut dormant =
        save_dormant_microvm_state(MICROVM_ATTACHMENT_ID, SessionState::default()).unwrap();
    validate_dormant_microvm_state(&dormant, MICROVM_ATTACHMENT_ID).unwrap();
    dormant.allowed_paths = owned(&["logs/payloads"]);
    assert!(validate_dormant_microvm_state(&dormant, MICROVM_ATTACHMENT_ID).is_err());
}
