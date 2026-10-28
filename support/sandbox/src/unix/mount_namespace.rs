// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mount namespaces (`CLONE_NEWNS`) and helpers for building a sandbox
//! root filesystem.

use crate::unix::SyscallResult;
use std::ffi::CString;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;

/// Atomically create a unique, owner-only staging directory under `/tmp`.
pub fn create_staging_dir() -> io::Result<PathBuf> {
    let mut template = b"/tmp/.sandbox-root-XXXXXX\0".to_vec();
    // SAFETY: mkdtemp receives a writable, NUL-terminated template ending
    // in six Xs and creates the directory with owner-only permissions.
    if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(OsStr::from_bytes(
        &template[..template.len() - 1],
    )))
}

/// Detach the current mount namespace from its parent's propagation.
///
/// Sets the root subtree's propagation type first to `MS_SLAVE` (always
/// allowed from within a fresh user+mount namespace) and then to
/// `MS_PRIVATE`, so subsequent mounts and unmounts inside this namespace
/// don't propagate into the parent namespace.
///
/// This is the standard first step after
/// entering the mount namespace. Bubblewrap, systemd's mount namespace setup,
/// and every OCI runtime do the same.
pub fn make_root_private() -> io::Result<()> {
    set_root_propagation(libc::MS_REC | libc::MS_SLAVE)?;
    set_root_propagation(libc::MS_REC | libc::MS_PRIVATE)?;
    Ok(())
}

/// Change the mount-propagation type of `/`.
///
/// Typically called via [`make_root_private`]; exposed separately for
/// callers that need finer control (e.g., recursively marking `/` as
/// `MS_SHARED` for a different reason).
pub fn set_root_propagation(flags: libc::c_ulong) -> io::Result<()> {
    let c_root = CString::new("/").expect("literal has no NUL");
    // SAFETY: mount() with a valid target and null source/type/data as
    // documented for propagation-flag remounts.
    unsafe {
        libc::mount(
            std::ptr::null(),
            c_root.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    }
    .syscall_result()?;
    Ok(())
}

/// Mount a tmpfs at the given path.
pub fn mount_tmpfs(target: &Path) -> io::Result<()> {
    let c_target = path_to_cstring(target)?;
    let c_fstype = CString::new("tmpfs").expect("literal has no NUL");
    let c_source = CString::new("none").expect("literal has no NUL");

    // SAFETY: mount() with valid NUL-terminated C strings.
    unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    }
    .syscall_result()?;
    Ok(())
}

/// Recursively bind-mount `source` at `target`, then set the caller-supplied
/// `MOUNT_ATTR_*` restrictions on the entire imported mount tree.
///
/// Requires `mount_setattr` (Linux 5.12+) when restrictions are requested;
/// `MS_REMOUNT | MS_BIND` cannot enforce flags on submounts recursively.
/// Existing restrictions are preserved, and any failure is returned to the
/// caller rather than falling back to top-mount-only enforcement.
///
/// A directory or file mountpoint matching the source is created if needed.
pub fn bind_mount(source: &Path, target: &Path, mount_attrs: u64) -> io::Result<()> {
    let c_source = path_to_cstring(source)?;
    let c_target = path_to_cstring(target)?;

    if fs_err::metadata(source)?.is_dir() {
        fs_err::create_dir_all(target)?;
    } else {
        if let Some(parent) = target.parent() {
            fs_err::create_dir_all(parent)?;
        }
        match fs_err::symlink_metadata(target) {
            Ok(metadata) if !metadata.is_dir() && !metadata.is_symlink() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file bind mount target must not be a directory or symlink",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs_err::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(target)?;
            }
            Err(error) => return Err(error),
        }
    }

    // SAFETY: mount() with valid NUL-terminated C strings; MS_BIND
    // ignores the filesystem-type argument, so passing NULL is
    // documented.
    unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    }
    .syscall_result()?;

    if mount_attrs != 0 {
        let attributes = libc::mount_attr {
            attr_set: mount_attrs,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        // SAFETY: The target is NUL-terminated and attributes is initialized
        // with the size passed to the kernel.
        unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                libc::AT_FDCWD,
                c_target.as_ptr(),
                libc::AT_RECURSIVE,
                std::ptr::from_ref(&attributes),
                size_of::<libc::mount_attr>(),
            )
        }
        .syscall_result()?;
    }

    Ok(())
}

/// Mount a proc filesystem at the given path.
///
/// Requires being in a PID namespace for a fully isolated `/proc`, but is
/// still useful for providing `/proc/self` etc.
pub fn mount_proc(target: &Path) -> io::Result<()> {
    let c_target = path_to_cstring(target)?;
    let c_fstype = CString::new("proc").expect("literal has no NUL");
    let c_source = CString::new("proc").expect("literal has no NUL");

    fs_err::create_dir_all(target)?;

    // SAFETY: mount() with valid NUL-terminated C strings and no flags
    // or data pointer.
    unsafe {
        libc::mount(
            c_source.as_ptr(),
            c_target.as_ptr(),
            c_fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    }
    .syscall_result()?;
    Ok(())
}

/// Pivot the root filesystem to a new root, putting the old root at
/// `old_root`.
///
/// After this call, `new_root` becomes `/` and the previous root is
/// accessible at `old_root` (relative to `new_root`). Typically you then
/// unmount `old_root`.
pub fn pivot_root(new_root: &Path, old_root: &Path) -> io::Result<()> {
    let c_new = path_to_cstring(new_root)?;
    let c_old = path_to_cstring(old_root)?;

    // SAFETY: pivot_root(2) with valid NUL-terminated path pointers.
    unsafe { libc::syscall(libc::SYS_pivot_root, c_new.as_ptr(), c_old.as_ptr()) }
        .syscall_result()?;
    Ok(())
}

/// Unmount a filesystem. Uses `MNT_DETACH` for lazy unmount to avoid
/// `EBUSY`.
pub fn umount(target: &Path) -> io::Result<()> {
    let c_target = path_to_cstring(target)?;

    // SAFETY: umount2() with a valid NUL-terminated path pointer.
    unsafe { libc::umount2(c_target.as_ptr(), libc::MNT_DETACH) }.syscall_result()?;
    Ok(())
}

/// Convert a `Path` to a `CString` for use with libc, mapping embedded
/// NUL bytes to an [`io::Error`].
fn path_to_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "null byte in path"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use test_with_tracing::test;

    #[test]
    fn mount_operations_work_in_namespace() {
        let exe = std::env::current_exe().expect("failed to get test executable path");
        let output = Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "--pid", "--fork"])
            .arg(exe)
            .args([
                "--exact",
                "unix::mount_namespace::tests::helper_mount_operations",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .expect("failed to spawn mount namespace helper");

        assert!(
            output.status.success(),
            "mount namespace helper failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn mount_namespace_is_isolated_from_parent() {
        let mountpoint = format!("/tmp/sandbox-mount-test-{}", std::process::id());
        fs_err::create_dir_all(&mountpoint).expect("failed to create parent mountpoint");
        let exe = std::env::current_exe().expect("failed to get test executable path");
        let output = Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount"])
            .env("SANDBOX_MOUNT_TEST_PATH", &mountpoint)
            .arg(exe)
            .args([
                "--exact",
                "unix::mount_namespace::tests::helper_isolated_mount",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .expect("failed to spawn isolated mount helper");
        assert!(
            output.status.success(),
            "isolated mount helper failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        let mounts =
            fs_err::read_to_string("/proc/self/mountinfo").expect("failed to read mountinfo");
        assert!(!mounts.contains(&mountpoint));
        fs_err::remove_dir(&mountpoint).expect("failed to remove parent mountpoint");
    }

    #[test]
    #[ignore]
    fn helper_isolated_mount() {
        let mountpoint = std::env::var_os("SANDBOX_MOUNT_TEST_PATH")
            .map(PathBuf::from)
            .expect("missing mount test path");

        make_root_private().expect("failed to make root private");
        mount_tmpfs(&mountpoint).expect("failed to mount tmpfs");
        fs_err::write(mountpoint.join("marker"), "isolated").expect("failed to write marker");
    }

    #[test]
    #[ignore]
    fn helper_mount_operations() {
        make_root_private().expect("failed to make root private");

        let base = PathBuf::from(format!("/tmp/sandbox-mount-{}", std::process::id()));
        fs_err::create_dir_all(&base).expect("failed to create tmpfs mountpoint");
        mount_tmpfs(&base).expect("failed to mount tmpfs");
        fs_err::write(base.join("tmpfs-marker"), "tmpfs").expect("failed to write to tmpfs");

        let source = base.join("source");
        let child = source.join("child");
        fs_err::create_dir_all(&child).expect("failed to create bind source");
        mount_tmpfs(&child).expect("failed to mount child tmpfs");
        for directory in [&source, &child] {
            let probe = directory.join("probe");
            fs_err::write(&probe, "#!/bin/sh\nexit 0\n").unwrap();
            fs_err::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(Command::new(&probe).status().unwrap().success());
        }

        let baseline = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV;
        for (name, attributes, writable, executable) in [
            (
                "read",
                baseline | libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOEXEC,
                false,
                false,
            ),
            ("read-exec", baseline | libc::MOUNT_ATTR_RDONLY, false, true),
            (
                "read-write",
                baseline | libc::MOUNT_ATTR_NOEXEC,
                true,
                false,
            ),
        ] {
            let target = base.join(name);
            bind_mount(&source, &target, attributes).expect("failed to bind mount");
            let mounts = fs_err::read_to_string("/proc/self/mountinfo").unwrap();
            for directory in [&target, &target.join("child")] {
                let probe = directory.join("probe");
                assert_eq!(
                    fs_err::read_to_string(&probe).unwrap(),
                    "#!/bin/sh\nexit 0\n",
                );
                assert_eq!(
                    fs_err::write(directory.join("new-file"), "probe").is_ok(),
                    writable,
                    "unexpected write permission on {}",
                    directory.display(),
                );
                if executable {
                    assert!(Command::new(&probe).status().unwrap().success());
                } else {
                    assert_eq!(
                        Command::new(&probe).status().unwrap_err().kind(),
                        io::ErrorKind::PermissionDenied,
                        "execution was not denied on {}",
                        directory.display(),
                    );
                }
                let options = mounts
                    .lines()
                    .find_map(|line| {
                        let mut fields = line.split_whitespace();
                        let mountpoint = fields.nth(4)?;
                        let options = fields.next()?;
                        (mountpoint == directory.to_str().unwrap()).then_some(options)
                    })
                    .expect("bind mount missing from mountinfo");
                for flag in ["nosuid", "nodev"] {
                    assert!(
                        options.split(',').any(|option| option == flag),
                        "{flag} missing on {}",
                        directory.display(),
                    );
                }
            }
            umount(&target).expect("failed to unmount bind target");
        }

        let file = source.join("probe");
        let target = base.join("file");
        let attributes = baseline | libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOEXEC;
        bind_mount(&file, &target, attributes).unwrap();
        bind_mount(&file, &target, attributes).unwrap();
        assert_eq!(
            fs_err::read_to_string(&target).unwrap(),
            "#!/bin/sh\nexit 0\n"
        );
        assert!(fs_err::write(&target, "modified").is_err());
        umount(&target).unwrap();
        umount(&target).unwrap();

        let link = base.join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(
            bind_mount(&file, &link, attributes).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );

        let proc_target = base.join("proc");
        mount_proc(&proc_target).expect("failed to mount proc");
        assert!(proc_target.join("self/status").exists());
        umount(&proc_target).expect("failed to unmount proc");
        umount(&child).expect("failed to unmount child tmpfs");
        umount(&base).expect("failed to unmount tmpfs");

        let new_root = PathBuf::from(format!("/tmp/sandbox-root-{}", std::process::id()));
        fs_err::create_dir_all(&new_root).expect("failed to create new root");
        mount_tmpfs(&new_root).expect("failed to mount new root");
        fs_err::write(new_root.join("marker"), "new root").expect("failed to populate new root");
        let old_root = new_root.join(".old_root");
        fs_err::create_dir(&old_root).expect("failed to create old-root directory");

        pivot_root(&new_root, &old_root).expect("failed to pivot root");
        std::env::set_current_dir("/").expect("failed to change to new root");
        assert_eq!(
            fs_err::read_to_string("/marker").expect("new root marker is inaccessible"),
            "new root"
        );
        assert!(Path::new("/.old_root/proc/self/status").exists());
        umount(Path::new("/.old_root")).expect("failed to detach old root");
        assert!(!Path::new("/.old_root/proc/self/status").exists());
    }
}
