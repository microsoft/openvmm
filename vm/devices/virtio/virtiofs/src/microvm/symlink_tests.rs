// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for symbolic links on microVM shares.

use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use super::saved_state::SavedHandle;
use super::saved_state::SavedObjectIdentity;
use super::state::validate_microvm_state;
use crate::VirtioFs;
use fuse::Fuse;
use fuse::Request;
use fuse::Session;
use fuse::SessionState;
use fuse::protocol::FATTR_MODE;
use fuse::protocol::FATTR_SIZE;
use fuse::protocol::FUSE_READLINK;
use fuse::protocol::FUSE_ROOT_ID;
use fuse::protocol::fuse_in_header;
use fuse::protocol::fuse_setattr_in;
use lxutil::LxCreateOptions;
use lxutil::LxVolume;
use lxutil::LxVolumeOptions;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;
use test_with_tracing::test;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

fn profile(root_path: &Path, read_only: bool, denied_paths: &[&str]) -> MicroVmVirtioFsProfile {
    MicroVmVirtioFsProfile::from_attachment(
        MICROVM_ATTACHMENT_ID.to_owned(),
        microvm_root_identity(root_path).unwrap(),
        read_only,
        denied_paths.iter().map(|path| (*path).to_owned()).collect(),
    )
    .unwrap()
}

/// Builds a request header for `node_id`, as the guest kernel would send it.
fn request(node_id: u64) -> Request {
    let header = fuse_in_header {
        len: size_of::<fuse_in_header>() as u32,
        opcode: FUSE_READLINK,
        unique: 1,
        nodeid: node_id,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    Request::new(header.as_bytes()).unwrap()
}

fn name(value: &[u8]) -> &lx::LxStr {
    lx::LxStr::from_bytes(value)
}

fn error<T>(result: lx::Result<T>) -> lx::Error {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => error,
    }
}

/// Creates a link on the host the way the microVM volume does on every platform.
fn host_symlink(root: &Path, link: &str, target: &str) {
    LxVolumeOptions::new()
        .confine_paths(true)
        .new_volume(root)
        .unwrap()
        .symlink(link, target, LxCreateOptions::new(0, 0, 0))
        .unwrap();
}

fn host_read_link(root: &Path, link: &str) -> Vec<u8> {
    LxVolume::new(root)
        .unwrap()
        .read_link(link)
        .unwrap()
        .as_bytes()
        .to_vec()
}

fn host_exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn setattr(valid: u32, update: impl FnOnce(&mut fuse_setattr_in)) -> fuse_setattr_in {
    let mut arg = fuse_setattr_in::new_zeroed();
    arg.valid = valid;
    update(&mut arg);
    arg
}

/// A share and a sibling host directory that the guest must never reach.
struct Fixture {
    _directory: tempfile::TempDir,
    root: std::path::PathBuf,
    outside: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path().join("share");
        let outside = directory.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"host secret").unwrap();
        Self {
            _directory: directory,
            root,
            outside,
        }
    }

    fn outside_target(&self) -> Vec<u8> {
        self.outside
            .join("secret")
            .to_str()
            .unwrap()
            .replace('\\', "/")
            .into_bytes()
    }

    fn assert_outside_unchanged(&self) {
        let names = std::fs::read_dir(&self.outside)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, ["secret"]);
        assert_eq!(
            std::fs::read(self.outside.join("secret")).unwrap(),
            b"host secret"
        );
    }
}

#[test]
fn rw_symlink_preserves_target_verbatim() {
    let fixture = Fixture::new();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();
    let targets: [&[u8]; 6] = [
        b"/absolute/host/path",
        b"../../outside/secret",
        b"dangling/relative",
        b"with space",
        b"\xff\xfe-not-utf8",
        b"node_modules/.bin/../tool/bin/cli.js",
    ];

    for (index, target) in targets.iter().enumerate() {
        let link = format!("link-{index}");
        let entry = fs
            .symlink(&request(FUSE_ROOT_ID), name(link.as_bytes()), name(target))
            .unwrap();
        assert_eq!(entry.attr.mode & lx::S_IFMT, lx::S_IFLNK);
        assert_eq!(entry.attr.size, target.len() as u64);

        let node = request(entry.nodeid);
        assert_eq!(fs.read_link(&node).unwrap().as_bytes(), *target);
        assert_eq!(
            fs.get_attr(&node, 0, 0).unwrap().attr.mode & lx::S_IFMT,
            lx::S_IFLNK
        );
        assert_eq!(host_read_link(&fixture.root, &link), *target);

        let lookup = fs
            .lookup(&request(FUSE_ROOT_ID), name(link.as_bytes()))
            .unwrap();
        assert_eq!(lookup.nodeid, entry.nodeid);
    }
    fixture.assert_outside_unchanged();
}

#[test]
fn ro_symlink_is_rejected_with_erofs() {
    let fixture = Fixture::new();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, true, &[])).unwrap();

    assert_eq!(
        error(fs.symlink(&request(FUSE_ROOT_ID), name(b"link"), name(b"target"))),
        lx::Error::EROFS
    );
    assert!(!host_exists(&fixture.root.join("link")));
}

#[test]
fn symlink_target_bounds_match_linux() {
    let fixture = Fixture::new();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();
    let root = request(FUSE_ROOT_ID);

    assert_eq!(
        error(fs.symlink(&root, name(b"empty"), name(b""))),
        lx::Error::ENOENT
    );
    assert_eq!(
        error(fs.symlink(&root, name(b"long"), name(&[b'x'; 4096]))),
        lx::Error::ENAMETOOLONG
    );
    assert!(!host_exists(&fixture.root.join("empty")));
    assert!(!host_exists(&fixture.root.join("long")));
}

#[test]
fn symlink_respects_inode_limits_before_creating_the_link() {
    let fixture = Fixture::new();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();
    fs.inner.inodes.write().inodes_by_node_id.next_handle = 0;

    assert_eq!(
        error(fs.symlink(&request(FUSE_ROOT_ID), name(b"link"), name(b"target"))),
        lx::Error::ENOSPC
    );
    assert!(!host_exists(&fixture.root.join("link")));
}

#[test]
fn guest_links_cannot_reach_a_denied_path() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.root.join("allowed")).unwrap();
    std::fs::create_dir(fixture.root.join("secrets")).unwrap();
    std::fs::write(fixture.root.join("secrets").join("token"), b"token").unwrap();
    let fs =
        VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &["secrets"])).unwrap();
    let root = request(FUSE_ROOT_ID);

    // A link cannot take the place of the denied path.
    std::fs::rename(
        fixture.root.join("secrets"),
        fixture.root.join("moved-secrets"),
    )
    .unwrap();
    assert_eq!(
        error(fs.symlink(&root, name(b"secrets"), name(b"allowed"))),
        lx::Error::EACCES
    );
    assert!(!host_exists(&fixture.root.join("secrets")));
    std::fs::rename(
        fixture.root.join("moved-secrets"),
        fixture.root.join("secrets"),
    )
    .unwrap();

    // A link may name the denied path, but the guest resolves it, and every
    // lookup of the denied path is still refused.
    let allowed = fs.lookup(&root, name(b"allowed")).unwrap();
    for (link, target) in [
        (b"alias".as_slice(), b"../secrets".as_slice()),
        (b"token".as_slice(), b"../secrets/token".as_slice()),
    ] {
        let entry = fs
            .symlink(&request(allowed.nodeid), name(link), name(target))
            .unwrap();
        assert_eq!(
            fs.read_link(&request(entry.nodeid)).unwrap().as_bytes(),
            target
        );
        // Using the link as a directory never traverses it on the host.
        assert_eq!(
            error(fs.lookup(&request(entry.nodeid), name(b"token"))),
            lx::Error::ELOOP
        );
    }
    assert_eq!(error(fs.lookup(&root, name(b"secrets"))), lx::Error::EACCES);
    assert_eq!(
        std::fs::read(fixture.root.join("secrets").join("token")).unwrap(),
        b"token"
    );
}

#[test]
fn symlink_inode_operations_do_not_follow() {
    let fixture = Fixture::new();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();
    let root = request(FUSE_ROOT_ID);
    let target = fixture.outside_target();
    let entry = fs.symlink(&root, name(b"escape"), name(&target)).unwrap();
    let node = request(entry.nodeid);

    assert_eq!(error(fs.open(&node, lx::O_RDWR as u32)), lx::Error::ELOOP);
    assert_eq!(error(fs.lookup(&node, name(b"secret"))), lx::Error::ELOOP);
    // Neither truncation nor a mode change may reach the target. Linux
    // rejects both; Windows applies them to the inert link file itself.
    let truncate = fs.set_attr(&node, &setattr(FATTR_SIZE, |arg| arg.size = 0));
    let chmod = fs.set_attr(&node, &setattr(FATTR_MODE, |arg| arg.mode = 0o600));
    if cfg!(unix) {
        assert_eq!(error(truncate), lx::Error::ELOOP);
        assert_eq!(error(chmod), lx::Error::ENOTSUP);
    }
    fs.statfs(&node).unwrap();
    assert_eq!(
        fs.get_attr(&node, 0, 0).unwrap().attr.mode & lx::S_IFMT,
        lx::S_IFLNK
    );

    // Renaming, hard-linking, and removing the link act on the link itself.
    fs.rename(&root, name(b"escape"), FUSE_ROOT_ID, name(b"renamed"), 0)
        .unwrap();
    assert_eq!(fs.read_link(&node).unwrap().as_bytes(), target);
    let hard = fs.link(&root, name(b"hard"), entry.nodeid).unwrap();
    assert_eq!(hard.nodeid, entry.nodeid);
    fs.unlink(&root, name(b"renamed")).unwrap();
    assert_eq!(fs.read_link(&node).unwrap().as_bytes(), target);
    fs.unlink(&root, name(b"hard")).unwrap();
    assert!(!host_exists(&fixture.root.join("hard")));
    fixture.assert_outside_unchanged();
}

#[test]
fn renamed_nodes_keep_an_exact_path() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("file"), b"file").unwrap();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();
    let root = request(FUSE_ROOT_ID);
    let file = fs.lookup(&root, name(b"file")).unwrap();
    let link = fs.symlink(&root, name(b"link"), name(b"file")).unwrap();

    // The guest may use a node again before it looks the new name up.
    fs.rename(&root, name(b"file"), FUSE_ROOT_ID, name(b"moved-file"), 0)
        .unwrap();
    fs.rename(&root, name(b"link"), FUSE_ROOT_ID, name(b"moved-link"), 0)
        .unwrap();
    for (node, path, kind) in [
        (file.nodeid, "moved-file", lx::S_IFREG),
        (link.nodeid, "moved-link", lx::S_IFLNK),
    ] {
        // A trailing separator would make the host resolve the name as a
        // directory and follow a link.
        let inode = fs.get_inode(node).unwrap();
        assert_eq!(inode.clone_path().as_os_str(), path);
        assert!(
            inode
                .aliases()
                .iter()
                .all(|alias| alias.as_os_str() == path)
        );
        assert_eq!(
            fs.get_attr(&request(node), 0, 0).unwrap().attr.mode & lx::S_IFMT,
            kind
        );
    }
    assert_eq!(
        fs.read_link(&request(link.nodeid)).unwrap().as_bytes(),
        b"file"
    );
}

#[test]
fn stale_node_replaced_by_a_link_cannot_reach_the_target() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("victim"), b"victim").unwrap();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();
    let root = request(FUSE_ROOT_ID);
    let victim = fs.lookup(&root, name(b"victim")).unwrap();

    // The guest keeps the node of an unlinked file and puts a link at its
    // former name.
    fs.unlink(&root, name(b"victim")).unwrap();
    fs.symlink(&root, name(b"victim"), name(&fixture.outside_target()))
        .unwrap();

    let node = request(victim.nodeid);
    let _ = fs.set_attr(&node, &setattr(FATTR_SIZE, |arg| arg.size = 0));
    let _ = fs.set_attr(&node, &setattr(FATTR_MODE, |arg| arg.mode = 0o777));
    let _ = fs.open(&node, lx::O_RDWR as u32);
    fixture.assert_outside_unchanged();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(fixture.outside.join("secret"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o777, 0o777);
    }
}

#[cfg(unix)]
#[test]
fn replaced_directory_cannot_redirect_a_node_outside_the_share() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.root.join("dir")).unwrap();
    std::fs::write(fixture.root.join("dir").join("secret"), b"inside").unwrap();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();
    let directory = fs.lookup(&request(FUSE_ROOT_ID), name(b"dir")).unwrap();
    let secret = fs
        .lookup(&request(directory.nodeid), name(b"secret"))
        .unwrap();

    // A concurrent rename can swap the directory for a link to the host
    // between any check and the host operation.
    std::fs::rename(fixture.root.join("dir"), fixture.root.join("saved")).unwrap();
    std::os::unix::fs::symlink(&fixture.outside, fixture.root.join("dir")).unwrap();

    assert_eq!(
        error(fs.open(&request(secret.nodeid), lx::O_RDWR as u32)),
        lx::Error::ELOOP
    );
    // The volume itself refuses to traverse the link, so the outcome does not
    // depend on the component check winning the race.
    let volume = fs.get_inode(FUSE_ROOT_ID).unwrap().volume();
    let path = Path::new("dir/secret");
    assert_eq!(error(volume.open(path, lx::O_RDWR, None)), lx::Error::ELOOP);
    let attr = lxutil::SetAttributes {
        mode: Some(0o777),
        ..Default::default()
    };
    assert_eq!(error(volume.set_attr(path, attr)), lx::Error::ELOOP);
    assert_eq!(error(volume.unlink(path, 0)), lx::Error::ELOOP);
    fixture.assert_outside_unchanged();
}

#[test]
fn readdirplus_returns_usable_symlink_nodes() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("file"), b"file").unwrap();
    host_symlink(&fixture.root, "link", "file");
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, true, &[])).unwrap();
    let root = fs.get_inode(FUSE_ROOT_ID).unwrap();
    let handle = fs
        .insert_file(Arc::clone(&root).open(lx::O_RDONLY as u32).unwrap())
        .unwrap();

    let entries = fs
        .get_file(handle)
        .unwrap()
        .read_dir(&fs, 0, 4096, true)
        .unwrap();
    assert!(!entries.is_empty());
    let link = fs.lookup(&request(FUSE_ROOT_ID), name(b"link")).unwrap();
    assert_eq!(link.attr.mode & lx::S_IFMT, lx::S_IFLNK);
    assert_eq!(
        fs.read_link(&request(link.nodeid)).unwrap().as_bytes(),
        b"file"
    );
}

#[test]
fn symlink_inode_survives_save_and_restore() {
    let fixture = Fixture::new();
    let profile = profile(&fixture.root, false, &[]);
    let source = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    let entry = source
        .symlink(
            &request(FUSE_ROOT_ID),
            name(b"link"),
            name(b"../verbatim/target"),
        )
        .unwrap();

    let state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    let saved = state
        .inodes
        .iter()
        .find(|inode| inode.node_id == entry.nodeid)
        .unwrap();
    assert_eq!(saved.object_identity.kind, lx::S_IFLNK);
    validate_microvm_state(&state, &profile).unwrap();

    let destination = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap();
    assert_eq!(
        destination
            .read_link(&request(entry.nodeid))
            .unwrap()
            .as_bytes(),
        b"../verbatim/target"
    );
}

#[test]
fn restore_rejects_a_replaced_symlink() {
    let fixture = Fixture::new();
    std::fs::write(fixture.root.join("file"), b"file").unwrap();
    let profile = profile(&fixture.root, false, &[]);
    let source = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    source
        .symlink(&request(FUSE_ROOT_ID), name(b"link"), name(b"file"))
        .unwrap();
    let link_state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    let file_state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();

    // Keep the original link alive so that its identity cannot be reused.
    std::fs::rename(fixture.root.join("link"), fixture.root.join("saved")).unwrap();
    host_symlink(&fixture.root, "link", "file");
    let destination = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    assert!(
        destination
            .restore_microvm_state(&profile, link_state, &session)
            .is_err()
    );

    std::fs::remove_file(fixture.root.join("link")).unwrap();
    std::fs::write(fixture.root.join("link"), b"file").unwrap();
    let destination = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    assert!(
        destination
            .restore_microvm_state(&profile, file_state, &session)
            .is_err()
    );
}

#[test]
fn restore_rejects_an_alias_that_crosses_a_symlink() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.root.join("dir")).unwrap();
    std::fs::write(fixture.root.join("dir").join("file"), b"file").unwrap();
    let profile = profile(&fixture.root, false, &[]);
    let source = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    let directory = source.lookup(&request(FUSE_ROOT_ID), name(b"dir")).unwrap();
    source
        .lookup(&request(directory.nodeid), name(b"file"))
        .unwrap();
    // Keep only the file's node, whose saved alias names the directory.
    source.forget(directory.nodeid, 1);
    let state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    assert!(
        state
            .inodes
            .iter()
            .all(|inode| inode.node_id != directory.nodeid)
    );

    // The same file remains reachable, but only through a link.
    std::fs::rename(fixture.root.join("dir"), fixture.root.join("saved")).unwrap();
    host_symlink(&fixture.root, "dir", "saved");
    let destination = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    let session = Session::new(destination.clone());
    let error = destination
        .restore_microvm_state(&profile, state, &session)
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("alias crosses a symbolic-link component"),
        "{error:#}"
    );
}

#[test]
fn saved_symlink_handle_is_still_rejected() {
    let fixture = Fixture::new();
    let profile = profile(&fixture.root, false, &[]);
    let source = VirtioFs::new_microvm(&fixture.root, profile.clone()).unwrap();
    let entry = source
        .symlink(&request(FUSE_ROOT_ID), name(b"link"), name(b"target"))
        .unwrap();
    let mut state = source
        .save_microvm_state(&profile, SessionState::default())
        .unwrap();
    let identity = &state
        .inodes
        .iter()
        .find(|inode| inode.node_id == entry.nodeid)
        .unwrap()
        .object_identity;
    let handle = SavedHandle {
        handle_id: state.next_handle_id,
        node_id: entry.nodeid,
        open_flags: (lx::O_RDONLY | lx::O_NOFOLLOW) as u32,
        kind: lx::S_IFLNK,
        object_identity: SavedObjectIdentity {
            device_id: identity.device_id,
            inode_id: identity.inode_id,
            kind: identity.kind,
        },
        directory_entries: Vec::new(),
        directory_snapshot_built: false,
    };
    state.next_handle_id += 1;
    state.handles.push(handle);

    assert!(validate_microvm_state(&state, &profile).is_err());
}

#[cfg(windows)]
#[test]
fn windows_microvm_symlinks_are_inert_wsl_links() {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    let fixture = Fixture::new();
    std::fs::create_dir(fixture.root.join("dir")).unwrap();
    std::fs::write(fixture.root.join("dir").join("file"), b"file").unwrap();
    let fs = VirtioFs::new_microvm(&fixture.root, profile(&fixture.root, false, &[])).unwrap();

    for (link, target) in [
        ("file-link", "dir/file"),
        ("dir-link", "dir"),
        ("escape", "../outside/secret"),
    ] {
        fs.symlink(
            &request(FUSE_ROOT_ID),
            name(link.as_bytes()),
            name(target.as_bytes()),
        )
        .unwrap();
        let host = fixture.root.join(link);
        let metadata = std::fs::symlink_metadata(&host).unwrap();
        assert_ne!(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT, 0);
        assert!(std::fs::read_link(&host).is_err());
        assert!(std::fs::metadata(&host).is_err());
        assert!(std::fs::read(&host).is_err());
    }
    fixture.assert_outside_unchanged();
}
