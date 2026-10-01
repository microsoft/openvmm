// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for volumes that resolve paths without following symbolic links.

use super::LxVolume;
use super::confine::Resolver;
use crate::LxCreateOptions;
use crate::LxVolumeOptions;
use crate::SetAttributes;
use crate::SetTime;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;

/// A volume root and a sibling directory that must stay unreachable from it.
struct Fixture {
    _directory: TempDir,
    root: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let outside = directory.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let secret = outside.join("secret");
        fs::write(&secret, b"secret").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
        Self {
            _directory: directory,
            root,
            outside,
        }
    }

    /// Returns a confined volume for every resolver that this kernel supports.
    fn volumes(&self) -> Vec<LxVolume> {
        let mut options = LxVolumeOptions::new();
        options.confine_paths(true);
        let mut volumes = Vec::new();
        let probed = LxVolume::new(&self.root, &options).unwrap();
        if probed.resolver == Some(Resolver::Openat2) {
            volumes.push(probed);
        }
        let mut walk = LxVolume::new(&self.root, &options).unwrap();
        walk.resolver = Some(Resolver::Walk);
        volumes.push(walk);
        volumes
    }

    fn assert_outside_unchanged(&self) {
        let names = fs::read_dir(&self.outside)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, [OsString::from("secret")]);
        let secret = self.outside.join("secret");
        assert_eq!(fs::read(&secret).unwrap(), b"secret");
        assert_eq!(
            fs::metadata(&secret).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }
}

fn errno<T>(result: lx::Result<T>) -> i32 {
    match result {
        Ok(_) => panic!("operation unexpectedly succeeded"),
        Err(error) => error.value(),
    }
}

fn attributes(update: impl FnOnce(&mut SetAttributes)) -> SetAttributes {
    let mut attr = SetAttributes::default();
    update(&mut attr);
    attr
}

fn name(value: &[u8]) -> &lx::LxStr {
    lx::LxStr::from_bytes(value)
}

#[test]
fn confined_paths_do_not_traverse_symlinks() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("dir")).unwrap();
    symlink(&fixture.outside, fixture.root.join("absolute")).unwrap();
    symlink("../outside", fixture.root.join("relative")).unwrap();
    symlink("../../outside", fixture.root.join("dir").join("nested")).unwrap();

    for volume in fixture.volumes() {
        for link in ["absolute", "relative", "dir/nested"] {
            let link = Path::new(link);
            let secret = link.join("secret");
            let created = link.join("created");
            let file = LxCreateOptions::new(0o644, 0, 0);
            let fifo = LxCreateOptions::new(lx::S_IFIFO | 0o644, 0, 0);
            let directory = LxCreateOptions::new(0o755, 0, 0);

            assert_eq!(errno(volume.lstat(&secret)), lx::ELOOP);
            assert_eq!(errno(volume.open(&secret, lx::O_RDONLY, None)), lx::ELOOP);
            assert_eq!(
                errno(volume.open(&created, lx::O_WRONLY | lx::O_CREAT, Some(file))),
                lx::ELOOP
            );
            assert_eq!(errno(volume.mkdir(&created, directory)), lx::ELOOP);
            let target = name(b"target");
            let link_options = LxCreateOptions::new(0, 0, 0);
            assert_eq!(
                errno(volume.symlink(&created, target, link_options)),
                lx::ELOOP
            );
            assert_eq!(errno(volume.mknod(&created, fifo, 0)), lx::ELOOP);
            assert_eq!(errno(volume.read_link(&secret)), lx::ELOOP);
            assert_eq!(errno(volume.unlink(&secret, 0)), lx::ELOOP);
            assert_eq!(
                errno(volume.rename(&secret, Path::new("moved"), 0)),
                lx::ELOOP
            );
            assert_eq!(errno(volume.link(&secret, Path::new("hard"))), lx::ELOOP);
            let truncate = attributes(|attr| attr.size = Some(0));
            assert_eq!(errno(volume.set_attr(&secret, truncate)), lx::ELOOP);
            let chmod = attributes(|attr| attr.mode = Some(0o600));
            assert_eq!(errno(volume.set_attr(&secret, chmod)), lx::ELOOP);
            assert_eq!(errno(volume.stat_fs(&secret)), lx::ELOOP);
            let xattr = name(b"user.nvx");
            assert_eq!(
                errno(volume.set_xattr(&secret, xattr, b"value", 0)),
                lx::ELOOP
            );
            assert_eq!(errno(volume.get_xattr(&secret, xattr, None)), lx::ELOOP);
            assert_eq!(errno(volume.list_xattr(&secret, None)), lx::ELOOP);
            assert_eq!(errno(volume.remove_xattr(&secret, xattr)), lx::ELOOP);
        }
        assert!(!fixture.root.join("moved").exists());
        assert!(!fixture.root.join("hard").exists());
    }
    fixture.assert_outside_unchanged();
}

#[test]
fn confined_final_symlink_is_not_followed() {
    let fixture = Fixture::new();
    let target = fixture.outside.join("secret");
    symlink(&target, fixture.root.join("final")).unwrap();
    symlink(
        fixture.outside.join("missing"),
        fixture.root.join("dangling"),
    )
    .unwrap();
    let modified = fs::metadata(&target).unwrap().modified().unwrap();

    for volume in fixture.volumes() {
        let link = Path::new("final");
        assert_eq!(
            u32::from(volume.lstat(link).unwrap().mode) & lx::S_IFMT,
            lx::S_IFLNK
        );
        assert_eq!(
            volume.read_link(link).unwrap().as_bytes(),
            target.as_os_str().as_bytes()
        );
        assert_eq!(errno(volume.open(link, lx::O_RDONLY, None)), lx::ELOOP);
        assert_eq!(
            errno(volume.open(link, lx::O_WRONLY | lx::O_TRUNC, None)),
            lx::ELOOP
        );
        let file = LxCreateOptions::new(0o644, 0, 0);
        assert_eq!(
            errno(volume.open(
                Path::new("dangling"),
                lx::O_WRONLY | lx::O_CREAT,
                Some(file)
            )),
            lx::ELOOP
        );
        let truncate = attributes(|attr| attr.size = Some(0));
        assert_eq!(errno(volume.set_attr(link, truncate)), lx::ELOOP);
        let chmod = attributes(|attr| attr.mode = Some(0o600));
        assert_eq!(errno(volume.set_attr(link, chmod)), lx::ENOTSUP);
        let touch = attributes(|attr| attr.mtime = SetTime::Now);
        volume.set_attr(link, touch).unwrap();
        volume.stat_fs(link).unwrap();
        // Linux rejects user attributes on a link; either way, the target is not modified.
        let _ = volume.set_xattr(link, name(b"user.nvx"), b"value", 0);
    }

    assert!(!fixture.outside.join("missing").exists());
    assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), modified);
    let outside = LxVolume::new(&fixture.outside, &LxVolumeOptions::new()).unwrap();
    assert!(
        outside
            .get_xattr(Path::new("secret"), name(b"user.nvx"), None)
            .is_err()
    );

    let volume = fixture.volumes().remove(0);
    volume.unlink(Path::new("final"), 0).unwrap();
    assert!(fs::symlink_metadata(fixture.root.join("final")).is_err());
    fixture.assert_outside_unchanged();
}

#[test]
fn confined_symlink_targets_are_verbatim() {
    let fixture = Fixture::new();
    let long = vec![b'x'; 1024];
    let targets: [&[u8]; 6] = [
        b"/absolute/host/path",
        b"../../escape",
        b"dangling/relative",
        b"with space",
        b"\xff\xfe-not-utf8",
        &long,
    ];

    for (volume_index, volume) in fixture.volumes().into_iter().enumerate() {
        for (target_index, target) in targets.iter().enumerate() {
            let link = PathBuf::from(format!("link-{volume_index}-{target_index}"));
            let stat = volume
                .symlink_stat(&link, name(target), LxCreateOptions::new(0, 0, 0))
                .unwrap();
            assert_eq!(stat.mode & lx::S_IFMT, lx::S_IFLNK);
            assert_eq!(stat.file_size, target.len() as u64);
            assert_eq!(volume.read_link(&link).unwrap().as_bytes(), *target);
            let host = fs::read_link(fixture.root.join(&link)).unwrap();
            assert_eq!(host.as_os_str().as_bytes(), *target);
        }
    }
}

#[test]
fn confined_volume_supports_common_operations() {
    let fixture = Fixture::new();
    for (index, volume) in fixture.volumes().into_iter().enumerate() {
        let base = PathBuf::from(format!("base-{index}"));
        let nested = base.join("nested");
        let file = nested.join("file");
        volume
            .mkdir(&base, LxCreateOptions::new(0o755, 0, 0))
            .unwrap();
        let stat = volume
            .mkdir_stat(&nested, LxCreateOptions::new(0o755, 0, 0))
            .unwrap();
        assert_eq!(stat.mode & lx::S_IFMT, lx::S_IFDIR);

        let options = LxCreateOptions::new(0o644, 0, 0);
        let handle = volume
            .open(&file, lx::O_RDWR | lx::O_CREAT | lx::O_EXCL, Some(options))
            .unwrap();
        assert_eq!(handle.pwrite(b"data", 0, 0).unwrap(), 4);
        drop(handle);

        let chmod = attributes(|attr| attr.mode = Some(0o600));
        assert_eq!(
            volume.set_attr_stat(&file, chmod).unwrap().mode & 0o7777,
            0o600
        );
        let truncate = attributes(|attr| attr.size = Some(2));
        assert_eq!(volume.set_attr_stat(&file, truncate).unwrap().file_size, 2);

        let link = nested.join("link");
        volume
            .symlink(&link, name(b"file"), LxCreateOptions::new(0, 0, 0))
            .unwrap();
        assert_eq!(volume.read_link(&link).unwrap().as_bytes(), b"file");

        let moved = base.join("moved");
        volume.rename(&file, &moved, 0).unwrap();
        let hard = nested.join("hard");
        assert_eq!(volume.link_stat(&moved, &hard).unwrap().link_count, 2);
        let handle = volume.open(&hard, lx::O_RDONLY, None).unwrap();
        let mut buffer = [0; 8];
        let size = handle.pread(&mut buffer, 0).unwrap();
        assert_eq!(&buffer[..size], b"da");
        drop(handle);

        volume.stat_fs(&moved).unwrap();
        let xattr = name(b"user.nvx");
        match volume.set_xattr(&moved, xattr, b"value", 0) {
            Ok(()) => {
                let mut value = [0; 16];
                let size = volume.get_xattr(&moved, xattr, Some(&mut value)).unwrap();
                assert_eq!(&value[..size], b"value");
                assert!(volume.list_xattr(&moved, None).unwrap() > 0);
                volume.remove_xattr(&moved, xattr).unwrap();
            }
            // Some file systems do not support user extended attributes.
            Err(error) => assert_eq!(error.value(), lx::ENOTSUP),
        }

        volume.unlink(&hard, 0).unwrap();
        volume.unlink(&link, 0).unwrap();
        volume.unlink(&nested, lx::AT_REMOVEDIR).unwrap();
        volume.unlink(&moved, 0).unwrap();
        volume.unlink(&base, lx::AT_REMOVEDIR).unwrap();
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
    }
}

#[test]
fn confined_root_uses_its_own_descriptor() {
    let fixture = Fixture::new();
    let root = Path::new("");
    for volume in fixture.volumes() {
        assert_eq!(
            u32::from(volume.lstat(root).unwrap().mode) & lx::S_IFMT,
            lx::S_IFDIR
        );
        let handle = volume
            .open(root, lx::O_RDONLY | lx::O_DIRECTORY, None)
            .unwrap();
        assert_eq!(
            u32::from(handle.fstat().unwrap().mode) & lx::S_IFMT,
            lx::S_IFDIR
        );
        let touch = attributes(|attr| attr.mtime = SetTime::Now);
        volume.set_attr(root, touch).unwrap();
        volume.stat_fs(root).unwrap();
        if let Err(error) = volume.list_xattr(root, None) {
            assert_eq!(error.value(), lx::ENOTSUP);
        }
    }
}

#[test]
fn swapped_directory_is_not_followed() {
    // A client can hold a node for `dir/secret` and, while an operation on it is in flight,
    // replace `dir` with a symbolic link to a host directory.
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("dir")).unwrap();
    fs::write(fixture.root.join("dir").join("secret"), b"inside").unwrap();

    for volume in fixture.volumes() {
        let secret = Path::new("dir/secret");
        volume.lstat(secret).unwrap();
        fs::rename(fixture.root.join("dir"), fixture.root.join("saved")).unwrap();
        symlink(&fixture.outside, fixture.root.join("dir")).unwrap();

        assert_eq!(errno(volume.lstat(secret)), lx::ELOOP);
        assert_eq!(errno(volume.open(secret, lx::O_RDWR, None)), lx::ELOOP);
        let chmod = attributes(|attr| attr.mode = Some(0o666));
        assert_eq!(errno(volume.set_attr(secret, chmod)), lx::ELOOP);
        assert_eq!(errno(volume.unlink(secret, 0)), lx::ELOOP);

        fs::remove_file(fixture.root.join("dir")).unwrap();
        fs::rename(fixture.root.join("saved"), fixture.root.join("dir")).unwrap();
    }
    fixture.assert_outside_unchanged();
}

#[test]
fn unconfined_volume_still_follows_intermediate_symlinks() {
    let fixture = Fixture::new();
    symlink("../outside", fixture.root.join("relative")).unwrap();
    let volume = LxVolume::new(&fixture.root, &LxVolumeOptions::new()).unwrap();
    assert!(volume.resolver.is_none());
    assert_eq!(
        volume
            .lstat(Path::new("relative/secret"))
            .unwrap()
            .file_size,
        6
    );
}
