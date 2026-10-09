// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests that the guest makes no device node on a microVM share: `mknod`
//! makes only regular files, FIFOs, and sockets, and a rename leaves no
//! whiteout behind.

use super::policy_tests::name;
use super::policy_tests::request;
use super::profile::MICROVM_ATTACHMENT_ID;
use super::profile::MicroVmVirtioFsProfile;
use super::profile::microvm_root_identity;
use crate::RENAME_EXCHANGE;
use crate::VirtioFs;
use fuse::Fuse;
use fuse::protocol::FUSE_ROOT_ID;
use fuse::protocol::fuse_mknod_in;
use std::fs::FileType;
use std::path::PathBuf;
use tempfile::TempDir;
use tempfile::tempdir;
use test_with_tracing::test;

/// Linux's `RENAME_NOREPLACE` flag for `renameat2`, which refuses to replace
/// an existing name.
const RENAME_NOREPLACE: u32 = 1;

/// Linux's `RENAME_WHITEOUT` flag for `renameat2`, which leaves a whiteout,
/// character device 0:0, at the old name.
const RENAME_WHITEOUT: u32 = 4;

/// Linux's `/dev/null`, character device 1:3, as the guest kernel encodes it
/// in `FUSE_MKNOD`.
const DEV_NULL: u32 = (1 << 8) | 3;

/// Linux's `/dev/loop0`, block device 7:0, as the guest kernel encodes it in
/// `FUSE_MKNOD`.
const DEV_LOOP0: u32 = 7 << 8;

/// A read-write microVM share without an access policy, which holds the file
/// `existing`.
struct Share {
    _directory: TempDir,
    root: PathBuf,
    fs: VirtioFs,
}

impl Share {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        let root = directory.path().join("share");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("existing"), "existing").unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(&root).unwrap(),
            false,
            Vec::new(),
        )
        .unwrap();
        let fs = VirtioFs::new_microvm(&root, profile).unwrap();
        Self {
            _directory: directory,
            root,
            fs,
        }
    }

    /// Makes the node `node` of type and mode `mode` and device `rdev` in the
    /// share's root with `FUSE_MKNOD`.
    fn mknod(&self, node: &str, mode: u32, rdev: u32) -> lx::Result<()> {
        let arg = fuse_mknod_in {
            mode,
            rdev,
            umask: 0,
            padding: 0,
        };
        self.fs.mknod(&request(FUSE_ROOT_ID), name(node), &arg)?;
        Ok(())
    }

    /// Renames `from` to `to` in the share's root with `FUSE_RENAME2`.
    fn rename(&self, from: &str, to: &str, flags: u32) -> lx::Result<()> {
        self.fs.rename(
            &request(FUSE_ROOT_ID),
            name(from),
            FUSE_ROOT_ID,
            name(to),
            flags,
        )
    }

    /// Returns the names in the share's host directory, sorted.
    fn entries(&self) -> Vec<String> {
        let mut names = std::fs::read_dir(&self.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.root.join(file)).unwrap()
    }

    /// Returns the host file type of `node`, without following a link.
    fn file_type(&self, node: &str) -> FileType {
        std::fs::symlink_metadata(self.root.join(node))
            .unwrap()
            .file_type()
    }
}

/// Linux lets a caller without `CAP_MKNOD` make a whiteout, character device
/// 0:0, and a privileged one any device, so HostFs refuses every character
/// and block device with `EPERM`, as Linux does where it denies a device,
/// before it asks the host: an existing name fails with `EPERM`, not the
/// host's `EEXIST`. Other types fail as `mknod(2)` fails them: a directory
/// with `EPERM`, and a symbolic link or an invalid type with `EINVAL`.
#[test]
fn mknod_makes_no_device_node() {
    let share = Share::new();
    for (kind, rdev, expected) in [
        (lx::S_IFCHR, 0, lx::Error::EPERM),
        (lx::S_IFCHR, DEV_NULL, lx::Error::EPERM),
        (lx::S_IFBLK, DEV_LOOP0, lx::Error::EPERM),
        (lx::S_IFDIR, 0, lx::Error::EPERM),
        (lx::S_IFLNK, 0, lx::Error::EINVAL),
        (0o030000, 0, lx::Error::EINVAL),
    ] {
        for node in ["node", "existing"] {
            assert_eq!(
                share.mknod(node, kind | 0o600, rdev),
                Err(expected),
                "{node}: mode {kind:#o}, device {rdev:#x}"
            );
        }
    }
    assert_eq!(share.entries(), ["existing"]);
    assert_eq!(share.read("existing"), "existing");

    // Regular files remain, and on Linux FIFOs and sockets, with a type of 0
    // making a regular file, as for `mknod(2)`. Windows hosts make only
    // regular files.
    share.mknod("regular", lx::S_IFREG | 0o600, 0).unwrap();
    assert!(share.file_type("regular").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        share.mknod("untyped", 0o600, 0).unwrap();
        assert!(share.file_type("untyped").is_file());
        share.mknod("fifo", lx::S_IFIFO | 0o600, 0).unwrap();
        assert!(share.file_type("fifo").is_fifo());
        share.mknod("socket", lx::S_IFSOCK | 0o600, 0).unwrap();
        assert!(share.file_type("socket").is_socket());
    }
}

/// `RENAME_WHITEOUT` leaves a whiteout, character device 0:0, at the old
/// name, which Linux lets a caller without `CAP_MKNOD` make, so HostFs passes
/// the host only `RENAME_NOREPLACE` or `RENAME_EXCHANGE`, and fails any other
/// flags with `EINVAL`, as Linux fails a flag that a filesystem does not
/// support or those two flags together, changing neither name, even for a
/// rename of a name onto itself.
#[test]
fn rename_leaves_no_whiteout() {
    let share = Share::new();
    for flags in [
        RENAME_WHITEOUT,
        RENAME_WHITEOUT | RENAME_NOREPLACE,
        RENAME_NOREPLACE | RENAME_EXCHANGE,
        1 << 3,
    ] {
        for to in ["moved", "existing"] {
            assert_eq!(
                share.rename("existing", to, flags),
                Err(lx::Error::EINVAL),
                "{to}: flags {flags:#x}"
            );
        }
    }
    assert_eq!(share.entries(), ["existing"]);
    assert_eq!(share.read("existing"), "existing");

    // RENAME_NOREPLACE still reaches the host, which refuses to replace a
    // name. Windows hosts reject every rename flag with EINVAL.
    std::fs::write(share.root.join("other"), "other").unwrap();
    let replace = share.rename("existing", "other", RENAME_NOREPLACE);
    if cfg!(windows) {
        assert_eq!(replace, Err(lx::Error::EINVAL));
    } else {
        assert_eq!(replace, Err(lx::Error::EEXIST));
        share.rename("existing", "moved", RENAME_NOREPLACE).unwrap();
        assert_eq!(share.entries(), ["moved", "other"]);
        assert_eq!(share.read("moved"), "existing");
    }
}
