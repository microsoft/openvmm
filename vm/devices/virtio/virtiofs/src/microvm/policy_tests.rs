// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the access policy of microVM shares: write narrowing, hidden
//! subtrees, and the allowed paths inside them.

use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use super::saved_state::SCHEMA_VERSION;
use super::state::save_dormant_microvm_state;
use super::state::validate_dormant_microvm_state;
use super::state::validate_microvm_state;
use crate::RENAME_EXCHANGE;
use crate::VirtioFs;
use fuse::Fuse;
use fuse::Request;
use fuse::Session;
use fuse::SessionState;
use fuse::protocol::FATTR_FH;
use fuse::protocol::FATTR_MODE;
use fuse::protocol::FATTR_SIZE;
use fuse::protocol::FUSE_GETATTR_FH;
use fuse::protocol::FUSE_LOOKUP;
use fuse::protocol::FUSE_ROOT_ID;
use fuse::protocol::StatxFlags;
use fuse::protocol::fuse_create_in;
use fuse::protocol::fuse_in_header;
use fuse::protocol::fuse_mkdir_in;
use fuse::protocol::fuse_mknod_in;
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
    create_with_flags(fs, parent, file_name, lx::O_RDWR)
}

/// Creates and opens a file with the open flags `flags`, and returns its node
/// ID and handle.
pub(super) fn create_with_flags(
    fs: &VirtioFs,
    parent: u64,
    file_name: &str,
    flags: i32,
) -> lx::Result<(u64, u64)> {
    let arg = fuse_create_in {
        flags: flags as u32,
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
fn writes_need_a_handle_open_for_writing() {
    let share = Share::new();
    let fs = share.fs(&[], &[], &["out"]);
    let out = lookup(&fs, "out").unwrap();
    let existing = lookup(&fs, "out/existing").unwrap();

    // Like Linux's write(2), a write through a handle that the guest did not
    // open for writing fails with EBADF, whether FUSE_OPEN or FUSE_CREATE
    // opened it, even with O_TRUNC, for which a Windows host needs write
    // access.
    for flags in [lx::O_RDONLY, lx::O_RDONLY | lx::O_TRUNC] {
        let fh = open(&fs, existing, flags).unwrap();
        assert_eq!(error(write(&fs, existing, fh, b"guest")), lx::Error::EBADF);
        release(&fs, existing, fh);
    }
    let flags = lx::O_RDONLY | lx::O_CREAT | lx::O_TRUNC;
    let (file, fh) = create_with_flags(&fs, out, "created", flags).unwrap();
    assert_eq!(error(write(&fs, file, fh, b"guest")), lx::Error::EBADF);
    release(&fs, file, fh);

    // That error comes before the policy's EROFS outside the writable paths.
    let log = lookup(&fs, "build.log").unwrap();
    let fh = open(&fs, log, lx::O_RDONLY).unwrap();
    assert_eq!(error(write(&fs, log, fh, b"guest")), lx::Error::EBADF);
    release(&fs, log, fh);

    let fh = open(&fs, existing, lx::O_WRONLY).unwrap();
    assert_eq!(write(&fs, existing, fh, b"guest").unwrap(), 5);
    release(&fs, existing, fh);

    assert_eq!(share.read("out/existing"), "guest");
    assert_eq!(share.read("out/created"), "");
    assert_eq!(share.read("build.log"), "log");
    share.assert_protected_paths_unchanged();
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

/// Without `O_EXCL`, a create of a name that exists opens the object that the
/// name links to, and hands the guest a handle on the object's node, so it may
/// write or truncate the object only where an open of that node may: when
/// every name that the guest knows for the object is writable.
#[test]
fn a_create_of_an_existing_name_opens_it_as_open_does() {
    let share = Share::new();
    std::fs::hard_link(share.path("marker"), share.path("out/twin")).unwrap();
    // The guest knows the read-only file by its name `marker`, and in the
    // first case by its writable second name `out/twin` too.
    for twin_known in [true, false] {
        let fs = share.fs(&[], &[], &["out"]);
        let out = lookup(&fs, "out").unwrap();
        let marker = lookup(&fs, "marker").unwrap();
        if twin_known {
            assert_eq!(lookup(&fs, "out/twin").unwrap(), marker);
        }
        for flags in [
            lx::O_WRONLY | lx::O_TRUNC,
            lx::O_RDONLY | lx::O_TRUNC,
            lx::O_WRONLY,
            lx::O_RDWR,
        ] {
            assert_eq!(
                error(open(&fs, marker, flags)),
                lx::Error::EROFS,
                "{flags:#o}"
            );
            assert_eq!(
                error(create_with_flags(&fs, out, "twin", lx::O_CREAT | flags)),
                lx::Error::EROFS,
                "twin_known={twin_known}, {flags:#o}"
            );
            assert_eq!(share.read("marker"), "host");
        }
        // The guest may still read the file, and an exclusive create fails
        // because the name exists.
        let (node, fh) = create_with_flags(&fs, out, "twin", lx::O_CREAT | lx::O_RDONLY).unwrap();
        assert_eq!(node, marker);
        release(&fs, node, fh);
        assert_eq!(
            error(create_with_flags(
                &fs,
                out,
                "twin",
                lx::O_CREAT | lx::O_EXCL | lx::O_RDWR
            )),
            lx::Error::EEXIST
        );
    }

    // A create still truncates a writable file that exists, whose node the
    // guest holds.
    let fs = share.fs(&[], &[], &["out"]);
    let out = lookup(&fs, "out").unwrap();
    let existing = lookup(&fs, "out/existing").unwrap();
    let (node, fh) = create_with_flags(
        &fs,
        out,
        "existing",
        lx::O_CREAT | lx::O_WRONLY | lx::O_TRUNC,
    )
    .unwrap();
    assert_eq!(node, existing);
    release(&fs, node, fh);
    assert_eq!(share.read("out/existing"), "");
    share.assert_protected_paths_unchanged();
}

/// Sends each request that names an open file's handle, `fh`, as a request
/// about the node `node_id`, and returns each request's name and result.
fn file_handle_requests(
    fs: &VirtioFs,
    node_id: u64,
    fh: u64,
) -> Vec<(&'static str, lx::Result<()>)> {
    let node = request(node_id);
    let mut read = fuse_read_in::new_zeroed();
    read.fh = fh;
    read.size = 64;
    let truncate = setattr(FATTR_FH | FATTR_SIZE, |arg| {
        arg.fh = fh;
        arg.size = 1;
    });
    vec![
        ("GETATTR", fs.get_attr(&node, FUSE_GETATTR_FH, fh).map(drop)),
        (
            "STATX",
            fs.get_statx(
                &node,
                fh,
                FUSE_GETATTR_FH,
                StatxFlags::new(),
                lx::StatExMask::new(),
            )
            .map(drop),
        ),
        ("SETATTR", fs.set_attr(&node, &truncate).map(drop)),
        ("READ", fs.read(&node, &read).map(drop)),
        ("WRITE", write(fs, node_id, fh, b"x").map(drop)),
        ("FSYNC", fs.fsync(&node, fh, 0)),
    ]
}

/// Sends each request that names an open directory's handle, `fh`, as a
/// request about the node `node_id`, and returns each request's name and
/// result.
fn directory_handle_requests(
    fs: &VirtioFs,
    node_id: u64,
    fh: u64,
) -> Vec<(&'static str, lx::Result<()>)> {
    let node = request(node_id);
    let mut read = fuse_read_in::new_zeroed();
    read.fh = fh;
    read.size = 4096;
    vec![
        ("READDIR", fs.read_dir(&node, &read).map(drop)),
        ("READDIRPLUS", fs.read_dir_plus(&node, &read).map(drop)),
        ("FSYNCDIR", fs.fsync_dir(&node, fh, 0)),
    ]
}

/// A request through a handle must name the node that the handle was opened
/// on, and the guest must still hold that node: HostFs keeps the names of the
/// nodes that the guest holds current, and they decide what a request through
/// the handle may do, such as whether it may write. So a request with another
/// node fails with `EBADF`, and one with a node that the guest forgot fails
/// with `EINVAL`, as other requests about a forgotten node do. The guest can
/// still release such a handle, which uses no name, so that it does not leak.
#[test]
fn a_handle_serves_only_its_node_while_the_guest_holds_it() {
    let share = Share::new();
    std::fs::create_dir(share.path("out/directory")).unwrap();
    let fs = share.fs(&[], &[], &["out"]);
    let out = lookup(&fs, "out").unwrap();
    let file = lookup(&fs, "out/existing").unwrap();
    let directory = lookup(&fs, "out/directory").unwrap();
    let marker = lookup(&fs, "marker").unwrap();
    let file_handle = open(&fs, file, lx::O_RDWR).unwrap();
    let directory_handle = fs
        .open_dir(&request(directory), lx::O_RDONLY as u32)
        .unwrap()
        .fh;
    let requests = |file_node, directory_node| {
        let mut sent = file_handle_requests(&fs, file_node, file_handle);
        sent.extend(directory_handle_requests(
            &fs,
            directory_node,
            directory_handle,
        ));
        sent
    };

    // Each request that names the handle's node succeeds, even once the file
    // is unlinked, since the handle reaches its file without a name.
    fs.unlink(&request(out), name("existing")).unwrap();
    for (operation, result) in requests(file, directory) {
        assert!(result.is_ok(), "{operation}: {result:?}");
    }

    // Another node's ID.
    for (operation, result) in requests(marker, marker) {
        assert_eq!(error(result), lx::Error::EBADF, "{operation}");
    }

    // The IDs of the handles' nodes, once the guest forgets them.
    fs.forget(file, 1);
    fs.forget(directory, 1);
    for (operation, result) in requests(file, directory) {
        assert_eq!(error(result), lx::Error::EINVAL, "{operation}");
    }

    release(&fs, file, file_handle);
    let mut arg = fuse_release_in::new_zeroed();
    arg.fh = directory_handle;
    fs.release_dir(&request(directory), &arg).unwrap();
    assert_eq!(error(write(&fs, file, file_handle, b"x")), lx::Error::EBADF);
    share.assert_protected_paths_unchanged();
}

/// An object is writable only while every name that the guest has used for it
/// is. A guest that forgets the node of an open handle and looks the object up
/// again at a read-only name gets a new node, whose names the forgotten node
/// never learns, so the handle must write through neither node.
#[test]
fn a_handle_of_a_forgotten_node_cannot_write_a_read_only_file() {
    let share = Share::new();
    // A host hard link puts the read-only `marker` inside the writable path.
    std::fs::hard_link(share.path("marker"), share.path("out/link")).unwrap();
    let fs = share.fs(&[], &[], &["out"]);
    let link = lookup(&fs, "out/link").unwrap();
    let fh = open(&fs, link, lx::O_RDWR).unwrap();

    fs.forget(link, 1);
    let marker = lookup(&fs, "marker").unwrap();
    assert_ne!(marker, link);
    assert_eq!(error(open(&fs, marker, lx::O_RDWR)), lx::Error::EROFS);
    for (node_id, expected) in [(link, lx::Error::EINVAL), (marker, lx::Error::EBADF)] {
        assert_eq!(error(write(&fs, node_id, fh, b"guest")), expected);
        let truncate = setattr(FATTR_FH | FATTR_SIZE, |arg| {
            arg.fh = fh;
            arg.size = 0;
        });
        assert_eq!(error(fs.set_attr(&request(node_id), &truncate)), expected);
    }

    release(&fs, link, fh);
    assert_eq!(share.read("marker"), "host");
}

/// `LINK` gives the target node's object another name. Once the guest has
/// removed every name of that object, by unlinking it or by renaming another
/// entry over it, the node keeps its former path, where another object may
/// now be, so a link to the node must fail with `ENOENT`, as Linux's `linkat`
/// does for an unlinked file, rather than link that object and record the new
/// name for the node. That object may even be a read-only file, which the
/// guest may not link into a writable path.
#[test]
fn a_link_to_a_node_whose_names_are_gone_links_nothing() {
    for overwrite in [false, true] {
        let share = Share::new();
        // A host hard link puts the read-only `marker` inside the writable
        // path.
        std::fs::hard_link(share.path("marker"), share.path("out/marker")).unwrap();
        let fs = share.fs(&[], &[], &["out"]);
        let out = lookup(&fs, "out").unwrap();
        let (node, fh) = create(&fs, out, "file").unwrap();
        let replacement = if overwrite {
            release(&fs, node, fh);
            let marker = lookup(&fs, "marker").unwrap();
            assert_eq!(lookup(&fs, "out/marker").unwrap(), marker);
            fs.rename(&request(out), name("marker"), out, name("file"), 0)
                .unwrap();
            assert_eq!(
                error(fs.link(&request(out), name("link"), marker)),
                lx::Error::EXDEV
            );
            marker
        } else {
            // The open handle keeps the host from giving the new file the
            // unlinked file's inode number, and with it the unlinked file's
            // node.
            fs.unlink(&request(out), name("file")).unwrap();
            let (new, new_fh) = create(&fs, out, "file").unwrap();
            release(&fs, new, new_fh);
            release(&fs, node, fh);
            new
        };
        assert_ne!(node, replacement);

        assert_eq!(
            error(fs.link(&request(out), name("link"), node)),
            lx::Error::ENOENT,
            "overwrite={overwrite}"
        );
        assert!(!share.path("out/link").exists());
        assert!(fs.get_inode(node).unwrap().aliases().is_empty());
        assert_eq!(lookup(&fs, "out/file").unwrap(), replacement);
        share.assert_protected_paths_unchanged();
    }
}

/// Returns the share's filesystem: a microVM share whose only writable path is
/// `out`, or a share without the microVM profile, which writes anywhere.
fn stale_node_fs(share: &Share, microvm: bool) -> VirtioFs {
    if microvm {
        share.fs(&[], &[], &["out"])
    } else {
        VirtioFs::new(&share.root, None).unwrap()
    }
}

/// Returns the requests of `requests` that do not fail with `ENOENT`, with
/// their results.
fn not_enoent(
    requests: Vec<(&'static str, lx::Result<()>)>,
) -> Vec<(&'static str, lx::Result<()>)> {
    requests
        .into_iter()
        .filter(|(_, result)| *result != Err(lx::Error::ENOENT))
        .collect()
}

/// Sends each request about the node `node_id` of a file in the directory
/// `parent` that names no handle, and returns each request's name and result.
fn file_node_requests(
    fs: &VirtioFs,
    parent: u64,
    node_id: u64,
) -> Vec<(&'static str, lx::Result<()>)> {
    let node = request(node_id);
    let xattr = name("user.nvx");
    let truncate = setattr(FATTR_SIZE, |arg| arg.size = 0);
    let chmod = setattr(FATTR_MODE, |arg| arg.mode = 0o600);
    vec![
        ("GETATTR", fs.get_attr(&node, 0, 0).map(drop)),
        (
            "STATX",
            fs.get_statx(&node, 0, 0, StatxFlags::new(), lx::StatExMask::new())
                .map(drop),
        ),
        ("SETATTR size", fs.set_attr(&node, &truncate).map(drop)),
        ("SETATTR mode", fs.set_attr(&node, &chmod).map(drop)),
        ("OPEN", open(fs, node_id, lx::O_RDONLY).map(drop)),
        (
            "OPEN O_TRUNC",
            open(fs, node_id, lx::O_WRONLY | lx::O_TRUNC).map(drop),
        ),
        ("READLINK", fs.read_link(&node).map(drop)),
        ("STATFS", fs.statfs(&node).map(drop)),
        ("GETXATTR", fs.get_xattr(&node, xattr, 64).map(drop)),
        ("GETXATTR size", fs.get_xattr_size(&node, xattr).map(drop)),
        ("LISTXATTR", fs.list_xattr(&node, 64).map(drop)),
        ("LISTXATTR size", fs.list_xattr_size(&node).map(drop)),
        ("SETXATTR", fs.set_xattr(&node, xattr, b"guest", 0)),
        ("REMOVEXATTR", fs.remove_xattr(&node, xattr)),
        (
            "LINK",
            fs.link(&request(parent), name("link"), node_id).map(drop),
        ),
    ]
}

/// Once the guest has removed every name of a file, by unlinking it or by
/// renaming another entry over it, the file's node keeps its former path,
/// where another object may now be: a new file, or a read-only file that a
/// host hard link puts inside the writable path. A request about the node
/// that names no handle would reach that object, so each must fail with
/// `ENOENT`, as an operation through an unlinked file's former name does,
/// rather than read or change that object. The node's open handles still reach
/// its own file, as Linux's do.
#[test]
fn a_file_whose_names_are_gone_is_reached_only_through_its_handles() {
    for microvm in [false, true] {
        for overwrite in [false, true] {
            let case = format!("microvm={microvm}, overwrite={overwrite}");
            let share = Share::new();
            if overwrite {
                // A host hard link puts the read-only `marker` inside the
                // writable path.
                std::fs::hard_link(share.path("marker"), share.path("out/marker")).unwrap();
            }
            let fs = stale_node_fs(&share, microvm);
            let out = lookup(&fs, "out").unwrap();
            let (node, fh) = create(&fs, out, "file").unwrap();
            write(&fs, node, fh, b"guest").unwrap();
            let replacement = if overwrite {
                fs.rename(&request(out), name("marker"), out, name("file"), 0)
                    .unwrap();
                "host"
            } else {
                // The open handle keeps the host from giving the new file the
                // unlinked file's inode number, and with it the unlinked
                // file's node.
                fs.unlink(&request(out), name("file")).unwrap();
                let (new, new_fh) = create(&fs, out, "file").unwrap();
                write(&fs, new, new_fh, b"new").unwrap();
                release(&fs, new, new_fh);
                assert_ne!(new, node, "{case}");
                "new"
            };

            let unexpected = not_enoent(file_node_requests(&fs, out, node));
            assert!(unexpected.is_empty(), "{case}: {unexpected:?}");
            let attr = fs
                .get_attr(&request(node), FUSE_GETATTR_FH, fh)
                .unwrap()
                .attr;
            assert_eq!(attr.size, 5, "{case}");
            let mut read = fuse_read_in::new_zeroed();
            read.fh = fh;
            read.size = 64;
            assert_eq!(fs.read(&request(node), &read).unwrap(), b"guest", "{case}");
            release(&fs, node, fh);

            assert_eq!(share.read("out/file"), replacement, "{case}");
            assert_eq!(share.entries("out"), ["existing", "file"], "{case}");
            share.assert_protected_paths_unchanged();
        }
    }
}

/// Likewise, once the guest has removed a directory, by `RMDIR` or by renaming
/// another directory over it, the directory's node keeps its former path,
/// where another directory may now be. A request about the node must fail
/// with `ENOENT`, as Linux refuses to look up, create, or remove an entry of a
/// removed directory, rather than reach that directory or its entries.
#[test]
fn a_directory_whose_names_are_gone_reaches_no_entry() {
    for microvm in [false, true] {
        for overwrite in [false, true] {
            let case = format!("microvm={microvm}, overwrite={overwrite}");
            let share = Share::new();
            let fs = stale_node_fs(&share, microvm);
            let out = lookup(&fs, "out").unwrap();
            let existing = lookup(&fs, "out/existing").unwrap();
            let directory = mkdir(&fs, out, "directory").unwrap();
            // The open handle keeps the host from giving a new directory the
            // removed one's inode number, and with it the removed one's node.
            let fh = fs
                .open_dir(&request(directory), lx::O_RDONLY as u32)
                .unwrap()
                .fh;
            if overwrite {
                mkdir(&fs, out, "other").unwrap();
                fs.rename(&request(out), name("other"), out, name("directory"), 0)
                    .unwrap();
            } else {
                fs.rmdir(&request(out), name("directory")).unwrap();
                assert_ne!(mkdir(&fs, out, "directory").unwrap(), directory, "{case}");
            }
            std::fs::write(share.path("out/directory/entry"), "entry").unwrap();
            std::fs::write(share.path("out/directory/moving"), "moving").unwrap();
            std::fs::create_dir(share.path("out/directory/subdirectory")).unwrap();

            let node = request(directory);
            let mut mknod = fuse_mknod_in::new_zeroed();
            mknod.mode = lx::S_IFREG | 0o644;
            let unexpected = not_enoent(vec![
                ("GETATTR", fs.get_attr(&node, 0, 0).map(drop)),
                ("OPENDIR", fs.open_dir(&node, lx::O_RDONLY as u32).map(drop)),
                ("LOOKUP", fs.lookup(&node, name("entry")).map(drop)),
                ("CREATE", create(&fs, directory, "created").map(drop)),
                ("MKDIR", mkdir(&fs, directory, "made").map(drop)),
                ("MKNOD", fs.mknod(&node, name("node"), &mknod).map(drop)),
                (
                    "SYMLINK",
                    fs.symlink(&node, name("symlink"), name("entry")).map(drop),
                ),
                ("LINK", fs.link(&node, name("hard"), existing).map(drop)),
                ("UNLINK", fs.unlink(&node, name("entry"))),
                ("RMDIR", fs.rmdir(&node, name("subdirectory"))),
                (
                    "RENAME out of it",
                    fs.rename(&node, name("moving"), out, name("moved"), 0),
                ),
                (
                    "RENAME into it",
                    fs.rename(&request(out), name("existing"), directory, name("moved"), 0),
                ),
            ]);
            assert!(unexpected.is_empty(), "{case}: {unexpected:?}");
            let mut arg = fuse_release_in::new_zeroed();
            arg.fh = fh;
            fs.release_dir(&node, &arg).unwrap();

            assert_eq!(
                share.entries("out/directory"),
                ["entry", "moving", "subdirectory"],
                "{case}"
            );
            assert_eq!(share.entries("out"), ["directory", "existing"], "{case}");
            assert_eq!(share.read("out/existing"), "existing", "{case}");
            share.assert_protected_paths_unchanged();
        }
    }
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

/// READDIRPLUS looks up each entry that it lists, which the access policy
/// refuses with EACCES for a name that reaches a hidden object, such as a
/// hard link that the host makes to a hidden file. The listing must skip that
/// entry, rather than fail at it: every later read would start at the entry
/// and fail too, so the guest could never list the rest of the directory.
#[test]
fn readdirplus_skips_entries_that_reach_hidden_objects() {
    let share = Share::new();
    let fs = share.fs(&["marker"], &[], &[]);
    std::fs::hard_link(share.path("marker"), share.path("alias")).unwrap();
    assert_eq!(error(lookup(&fs, "alias")), lx::Error::EACCES);

    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let directory = root.open(lx::O_RDONLY | lx::O_DIRECTORY).unwrap();
    let entries = crate::tests::read_directory(&fs, &directory, 0, 4096, true);
    assert_eq!(
        entries.into_keys().collect::<Vec<_>>(),
        [".", "..", "build.log", "logs", "out"]
    );
    // The skipped entry holds no lookup reference.
    assert_eq!(fs.inner.inodes.read().inodes_by_node_id.values.len(), 1);
}

/// A rename moves everything below the names that it moves, so renaming a
/// directory that holds a denied path would move the hidden entries to a path
/// that the guest can list, open, and remove. Whichever name holds the denied
/// path, even one inside an allowed path, the rename fails with EROFS before
/// the host changes anything.
#[test]
fn renames_never_move_a_denied_path() {
    let share = Share::new();
    let fs = share.fs(&["logs/secret"], &[], &[]);
    let root = FUSE_ROOT_ID;
    let logs = lookup(&fs, "logs").unwrap();
    let out = lookup(&fs, "out").unwrap();
    let rename = |parent, entry, new_parent, new_entry, flags| {
        fs.rename(
            &request(parent),
            name(entry),
            new_parent,
            name(new_entry),
            flags,
        )
    };
    for (parent, entry, new_parent, new_entry, flags) in [
        (root, "logs", root, "moved", 0),
        (root, "logs", out, "logs", 0),
        (root, "logs", root, "out", RENAME_EXCHANGE),
        (root, "out", root, "logs", RENAME_EXCHANGE),
    ] {
        assert_eq!(
            rename(parent, entry, new_parent, new_entry, flags),
            Err(lx::Error::EROFS),
            "{entry} to {new_entry} with flags {flags}"
        );
    }
    assert_eq!(list(&fs, root), ["build.log", "logs", "marker", "out"]);
    assert_eq!(list(&fs, logs), ["gateway", "payloads"]);
    assert_eq!(share.entries("out"), ["existing"]);
    share.assert_protected_paths_unchanged();

    // Renames that move no denied path still succeed, even in the directory
    // that holds one.
    rename(logs, "gateway", logs, "renamed", 0).unwrap();
    rename(logs, "renamed", logs, "gateway", 0).unwrap();
    rename(root, "out", root, "renamed", 0).unwrap();
    rename(root, "renamed", root, "out", 0).unwrap();
    assert_eq!(share.read("out/existing"), "existing");
    share.assert_protected_paths_unchanged();

    let nested = share.fs(
        &["logs", "logs/payloads/private/key"],
        &["logs/payloads"],
        &[],
    );
    let payloads = lookup(&nested, "logs/payloads").unwrap();
    assert_eq!(
        nested.rename(
            &request(payloads),
            name("private"),
            payloads,
            name("moved"),
            0
        ),
        Err(lx::Error::EROFS)
    );
    share.assert_protected_paths_unchanged();
}

/// A rename moves the entries below its source to its destination, so renaming
/// a directory onto a name that holds a denied path would put the guest's
/// entries at the denied path, which the guest may not modify. The host may
/// remove the directory that holds a denied path while the share is attached,
/// and the rename still fails with EROFS before the host changes anything.
#[test]
fn renames_never_move_entries_into_a_denied_path() {
    let share = Share::new();
    let fs = share.fs(&["logs/secret"], &[], &[]);
    let root = FUSE_ROOT_ID;
    std::fs::rename(share.path("logs"), share.path("host-logs")).unwrap();
    let staging = mkdir(&fs, root, "staging").unwrap();
    let (secret, fh) = create(&fs, staging, "secret").unwrap();
    write(&fs, secret, fh, b"guest").unwrap();
    release(&fs, secret, fh);

    assert_eq!(
        fs.rename(&request(root), name("staging"), root, name("logs"), 0),
        Err(lx::Error::EROFS)
    );
    assert!(!share.path("logs").exists());
    assert_eq!(share.read("staging/secret"), "guest");

    std::fs::remove_dir_all(share.path("staging")).unwrap();
    std::fs::rename(share.path("host-logs"), share.path("logs")).unwrap();
    share.assert_protected_paths_unchanged();
}

/// Windows resolves a name in another case to the entry of that name, unless
/// its directory is case sensitive. Such a name of a denied file is as hidden
/// as the file's own name, so the guest can neither see the file through it
/// nor remove, move, replace, or create the file.
#[cfg(windows)]
#[test]
fn a_denied_path_in_another_case_is_hidden() {
    let share = Share::new();
    let fs = share.fs(&["marker"], &[], &[]);
    let root = FUSE_ROOT_ID;
    let out = lookup(&fs, "out").unwrap();
    for spelling in ["marker", "MARKER", "Marker"] {
        assert_eq!(
            error(fs.unlink(&request(root), name(spelling))),
            lx::Error::EACCES,
            "{spelling}"
        );
        assert_eq!(
            error(fs.rename(&request(root), name(spelling), out, name("moved"), 0)),
            lx::Error::EACCES,
            "{spelling}"
        );
        assert_eq!(
            error(fs.rename(&request(out), name("existing"), root, name(spelling), 0)),
            lx::Error::EACCES,
            "{spelling}"
        );
        assert_eq!(
            error(create(&fs, root, spelling)),
            lx::Error::EACCES,
            "{spelling}"
        );
        assert_eq!(
            error(lookup(&fs, spelling)),
            lx::Error::EACCES,
            "{spelling}"
        );
    }
    assert_eq!(list(&fs, root), ["build.log", "logs", "out"]);
    assert_eq!(share.read("out/existing"), "existing");
    share.assert_protected_paths_unchanged();
}

/// A denied file that the host creates after the share is attached has no
/// pinned identity, so only the policy keeps a name in another case from
/// reaching it, and the guest cannot take its place under such a name.
#[cfg(windows)]
#[test]
fn a_recreated_denied_path_stays_hidden_in_another_case() {
    let share = Share::new();
    let fs = share.fs(&["marker"], &[], &[]);
    let root = FUSE_ROOT_ID;
    std::fs::remove_file(share.path("marker")).unwrap();
    assert_eq!(error(create(&fs, root, "MARKER")), lx::Error::EACCES);
    assert_eq!(error(mkdir(&fs, root, "MARKER")), lx::Error::EACCES);
    assert_eq!(
        error(fs.symlink(&request(root), name("MARKER"), name("out"))),
        lx::Error::EACCES
    );
    assert_eq!(share.entries(""), ["build.log", "logs", "out"]);

    std::fs::write(share.path("marker"), "recreated").unwrap();
    assert_eq!(error(lookup(&fs, "MARKER")), lx::Error::EACCES);
    // An exclusive create fails as it does without the file, so it does not
    // tell whether the file exists.
    assert_eq!(
        error(create_with_flags(
            &fs,
            root,
            "MARKER",
            lx::O_RDWR | lx::O_EXCL
        )),
        lx::Error::EACCES
    );
    assert_eq!(share.read("marker"), "recreated");
}

/// A traverse-only directory in another case is as protected as at its own
/// name, so its writable directory can neither remove it nor move it away from
/// its hidden entries, which a visible name would expose.
#[cfg(windows)]
#[test]
fn a_traverse_only_directory_in_another_case_stays_in_place() {
    let share = Share::new();
    std::fs::create_dir_all(share.path("out/readonly/pub")).unwrap();
    std::fs::write(share.path("out/readonly/secret"), "secret").unwrap();
    let cases: [(&[&str], &[&str], &[&str], &str, &str); 2] = [
        (&["logs"], &["logs/payloads"], &[], "", "LOGS"),
        (
            &["out/readonly"],
            &["out/readonly/pub"],
            &["out"],
            "out",
            "READONLY",
        ),
    ];
    for (denied, allowed, writable, directory, spelling) in cases {
        let fs = share.fs(denied, allowed, writable);
        let directory = if directory.is_empty() {
            FUSE_ROOT_ID
        } else {
            lookup(&fs, directory).unwrap()
        };
        assert_eq!(
            error(fs.rename(
                &request(directory),
                name(spelling),
                directory,
                name("moved"),
                0
            )),
            lx::Error::EROFS,
            "{spelling}"
        );
        assert_eq!(
            error(fs.rmdir(&request(directory), name(spelling))),
            lx::Error::EROFS,
            "{spelling}"
        );
    }
    assert_eq!(share.entries("out/readonly"), ["pub", "secret"]);
    share.assert_protected_paths_unchanged();
}

/// A rename moves everything below its names, so a directory that holds a
/// denied path is protected from renames also under a name in another case,
/// which Windows resolves to it: renaming it under that name would expose
/// its hidden entries, and once the host has removed it, renaming a guest
/// directory onto that name would put the guest's entries at the denied path.
#[cfg(windows)]
#[test]
fn renames_never_move_a_denied_path_in_another_case() {
    let share = Share::new();
    let fs = share.fs(&["logs/secret"], &[], &[]);
    let root = FUSE_ROOT_ID;
    assert_eq!(
        fs.rename(&request(root), name("LOGS"), root, name("moved"), 0),
        Err(lx::Error::EROFS)
    );
    share.assert_protected_paths_unchanged();

    std::fs::rename(share.path("logs"), share.path("host-logs")).unwrap();
    let staging = mkdir(&fs, root, "staging").unwrap();
    let (secret, fh) = create(&fs, staging, "secret").unwrap();
    write(&fs, secret, fh, b"guest").unwrap();
    release(&fs, secret, fh);
    assert_eq!(
        fs.rename(&request(root), name("staging"), root, name("LOGS"), 0),
        Err(lx::Error::EROFS)
    );
    assert!(!share.path("logs").exists());
    assert_eq!(share.read("staging/secret"), "guest");

    std::fs::remove_dir_all(share.path("staging")).unwrap();
    std::fs::rename(share.path("host-logs"), share.path("logs")).unwrap();
    share.assert_protected_paths_unchanged();
}

/// The guest reaches an existing entry only by the name under which the host
/// stores it, as on Linux, although Windows also resolves the name in another
/// case. Such a name neither reaches nor replaces the entry, so it cannot
/// become a name of an object that the policy checks writes through, which
/// would make a writable directory read-only.
#[cfg(windows)]
#[test]
fn the_guest_reaches_entries_only_by_their_own_names() {
    let share = Share::new();
    let fs = share.fs(&["logs"], &["logs/payloads"], &["out"]);
    let out = lookup(&fs, "out").unwrap();
    for path in ["OUT", "Out/existing", "out/EXISTING", "LOGS"] {
        assert_eq!(error(lookup(&fs, path)), lx::Error::ENOENT, "{path}");
    }
    assert_eq!(
        error(fs.unlink(&request(out), name("EXISTING"))),
        lx::Error::ENOENT
    );
    assert_eq!(
        error(fs.rename(&request(out), name("EXISTING"), out, name("moved"), 0)),
        lx::Error::ENOENT
    );
    assert_eq!(
        error(create_with_flags(
            &fs,
            out,
            "EXISTING",
            lx::O_RDWR | lx::O_TRUNC
        )),
        lx::Error::EACCES
    );
    // An exclusive create fails as the host's does.
    assert_eq!(
        error(create_with_flags(
            &fs,
            out,
            "EXISTING",
            lx::O_RDWR | lx::O_EXCL
        )),
        lx::Error::EEXIST
    );
    let (new, fh) = create(&fs, out, "new").unwrap();
    release(&fs, new, fh);
    assert_eq!(
        error(fs.rename(&request(out), name("new"), out, name("EXISTING"), 0)),
        lx::Error::EACCES
    );
    // Renaming an entry to its own name in another case changes its name.
    fs.rename(&request(out), name("new"), out, name("NEW"), 0)
        .unwrap();
    assert_eq!(lookup(&fs, "out/NEW").unwrap(), new);
    assert_eq!(error(lookup(&fs, "out/new")), lx::Error::ENOENT);

    // The writable directory is still writable.
    let (file, fh) = create(&fs, out, "file").unwrap();
    write(&fs, file, fh, b"guest").unwrap();
    release(&fs, file, fh);
    assert_eq!(share.entries("out"), ["NEW", "existing", "file"]);
    assert_eq!(share.read("out/existing"), "existing");
    share.assert_protected_paths_unchanged();
}

/// Windows also resolves the 8.3 short name of a long name, but the guest
/// reaches no entry by it, so it can neither see nor change a denied file
/// through it, even one that the host created after the share was attached,
/// whose identity is not pinned, nor through a node whose name has since
/// become the file's short name.
#[cfg(windows)]
#[test]
fn short_names_reach_no_entry() {
    let share = Share::new();
    std::fs::write(share.path("credentials.json"), "secret").unwrap();
    std::fs::write(share.path("out/long-file-name.txt"), "visible").unwrap();
    let short = "CREDEN~1.JSO";
    if !share.path(short).exists() || !share.path("out/LONG-F~1.TXT").exists() {
        tracing::info!("the volume does not generate 8.3 short names");
        return;
    }
    let fs = share.fs(&["credentials.json"], &[], &[]);
    let root = FUSE_ROOT_ID;
    assert_eq!(
        error(fs.unlink(&request(root), name(short))),
        lx::Error::ENOENT
    );
    assert_eq!(
        error(fs.rename(&request(root), name(short), root, name("moved"), 0)),
        lx::Error::ENOENT
    );
    assert_eq!(
        error(fs.rename(&request(root), name("marker"), root, name(short), 0)),
        lx::Error::EACCES
    );
    assert_eq!(error(lookup(&fs, short)), lx::Error::ENOENT);
    // A visible file is also reachable only by its own name.
    lookup(&fs, "out/long-file-name.txt").unwrap();
    for path in ["out/LONG-F~1.TXT", "out/long-f~1.txt"] {
        assert_eq!(error(lookup(&fs, path)), lx::Error::ENOENT, "{path}");
    }

    // The host recreates the denied file, which pins nothing, after the guest
    // created and removed a file whose name becomes the recreated file's short
    // name, and whose node the guest still holds.
    std::fs::remove_file(share.path("credentials.json")).unwrap();
    let (own, fh) = create(&fs, root, short).unwrap();
    release(&fs, own, fh);
    fs.unlink(&request(root), name(short)).unwrap();
    std::fs::write(share.path("credentials.json"), "recreated").unwrap();
    assert!(share.path(short).exists());
    assert_eq!(error(lookup(&fs, short)), lx::Error::ENOENT);
    assert_eq!(error(open(&fs, own, lx::O_RDONLY)), lx::Error::ENOENT);
    assert_eq!(error(fs.get_attr(&request(own), 0, 0)), lx::Error::ENOENT);
    assert_eq!(
        error(create_with_flags(
            &fs,
            root,
            short,
            lx::O_RDWR | lx::O_TRUNC
        )),
        lx::Error::EACCES
    );
    assert_eq!(
        error(fs.unlink(&request(root), name(short))),
        lx::Error::ENOENT
    );
    assert_eq!(share.read("credentials.json"), "recreated");
    assert_eq!(share.read("marker"), "host");
}

/// The check of a rename's names fails a missing source, as the host would,
/// rather than pass the rename on with its target unchecked: the guest could
/// create the source in between, and the rename would then replace the
/// denied file through its short name.
#[cfg(windows)]
#[test]
fn a_rename_without_a_source_fails_before_its_target_is_reached() {
    let share = Share::new();
    std::fs::write(share.path("credentials.json"), "secret").unwrap();
    let fs = share.fs(&["credentials.json"], &[], &[]);
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    for target in ["CREDEN~1.JSO", "moved"] {
        assert_eq!(
            crate::microvm::inode::ensure_stored_rename_names(
                &root.volume,
                &PathBuf::from("staging"),
                &PathBuf::from(target)
            ),
            Err(lx::Error::ENOENT),
            "{target}"
        );
    }
}

/// READDIRPLUS reads a directory's entries from the snapshot that its first
/// read takes, so a later read can list a name whose case has changed since,
/// or whose entry the host has replaced with another in another case. Windows
/// still reaches an entry by that name, but stores it under another, so the
/// read skips the name, as it skips a name that is gone, rather than hand the
/// guest a node whose every request would fail, or fail at the name because
/// it reaches another object than the one listed.
#[cfg(windows)]
#[test]
fn readdirplus_skips_names_whose_case_changed_since_its_snapshot() {
    let share = Share::new();
    std::fs::create_dir(share.path("out/listing")).unwrap();
    for name in ["a", "new", "old", "z"] {
        std::fs::write(share.path("out/listing").join(name), name).unwrap();
    }
    let fs = share.fs(&["marker"], &[], &[]);
    let listing = lookup(&fs, "out/listing").unwrap();
    let directory = fs
        .get_inode(listing)
        .unwrap()
        .open(lx::O_RDONLY | lx::O_DIRECTORY)
        .unwrap();
    // The first read takes the snapshot and returns only `.` and `..`.
    let entry_size = |name: &str| {
        (size_of::<fuse::protocol::fuse_direntplus>() + name.len()).next_multiple_of(8)
    };
    let page = directory
        .read_dir(&fs, 0, (entry_size(".") + entry_size("..")) as u32, true)
        .unwrap();
    let (dotdot, _) = <fuse::protocol::fuse_direntplus as zerocopy::FromBytes>::read_from_prefix(
        &page[entry_size(".")..],
    )
    .unwrap();
    assert_eq!(dotdot.dirent.namelen, 2);

    std::fs::rename(share.path("out/listing/new"), share.path("out/listing/NEW")).unwrap();
    std::fs::remove_file(share.path("out/listing/old")).unwrap();
    std::fs::write(share.path("out/listing/OLD"), "replaced").unwrap();
    let rest = crate::tests::read_directory(&fs, &directory, dotdot.dirent.off, 4096, true);
    assert_eq!(rest.into_keys().collect::<Vec<_>>(), ["a", "z"]);
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
    assert_eq!(state.schema_version, SCHEMA_VERSION);
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
fn saved_state_records_the_access_policy() {
    let share = Share::new();
    // A deny-only attachment records its denied paths.
    let deny_only = share.profile(&["logs"], &[], &[]);
    let source = VirtioFs::new_microvm(&share.root, deny_only.clone()).unwrap();
    let mut state = source
        .save_microvm_state(&deny_only, SessionState::default())
        .unwrap();
    assert_eq!(state.schema_version, SCHEMA_VERSION);
    assert_eq!(state.denied_paths, ["logs"]);
    assert!(state.allowed_paths.is_empty() && state.writable_paths.is_empty());
    validate_microvm_state(&state, &deny_only).unwrap();

    // A state cannot restore an attachment with another policy.
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
