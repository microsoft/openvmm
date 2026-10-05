// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-thread filesystem credentials.
//!
//! Linux keeps credentials per thread. The raw `setfsuid`, `setfsgid`, `setgroups`, and `capset`
//! system calls change only the calling thread, whereas the C library wrappers for `setgroups` and
//! the `set*id` family change every thread in the process. This module therefore issues the raw
//! system calls, so that one worker thread can perform an operation as another identity without
//! affecting the rest of the process.

use super::util;

/// The ID that no user or group has. `setfsuid` and `setfsgid` report the current ID without
/// changing it when passed this value.
const INVALID_ID: u32 = u32::MAX;

/// `_LINUX_CAPABILITY_VERSION_3` from `linux/capability.h`.
const CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// `struct __user_cap_header_struct` from `linux/capability.h`.
#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: libc::c_int,
}

/// `struct __user_cap_data_struct` from `linux/capability.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The 64 capability bits of version 3, in two 32-bit words.
type Capabilities = [CapabilityData; 2];

/// A host filesystem identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsIdentity {
    /// The filesystem user ID.
    pub uid: lx::uid_t,
    /// The filesystem group ID.
    pub gid: lx::gid_t,
}

impl FsIdentity {
    /// Returns the filesystem identity of the calling thread.
    pub fn current() -> Self {
        Self {
            uid: set_fsuid(INVALID_ID),
            gid: set_fsgid(INVALID_ID),
        }
    }
}

/// Runs `operation` with the calling thread's filesystem identity set to `identity`, and then
/// restores the thread's credentials.
///
/// The operation runs without effective capabilities and without supplementary groups other
/// than the GID of `identity`, so that it gains no access through the privileges or groups of
/// the thread's original identity. Assuming a different identity, or dropping supplementary
/// groups, requires `CAP_SETUID` and `CAP_SETGID`. A thread that already has `identity` and no
/// other supplementary groups needs no privilege.
///
/// # Errors
///
/// Returns `EPERM` without running `operation` if the thread cannot assume `identity` or drop
/// its other supplementary groups. The operation never falls back to the thread's original
/// identity.
///
/// # Panics
///
/// Panics if the thread's original credentials cannot be restored, because the thread would
/// otherwise run unrelated work under the wrong identity.
pub fn with_fs_identity<T>(identity: FsIdentity, operation: impl FnOnce() -> T) -> lx::Result<T> {
    let credentials = SwitchedCredentials::switch(identity).map_err(|_| lx::Error::EPERM)?;
    let result = operation();
    drop(credentials);
    Ok(result)
}

/// The original credentials of a thread whose filesystem identity was switched.
///
/// Dropping the value restores them on the same thread. It is created and dropped within
/// [`with_fs_identity`], so it never moves to another thread.
struct SwitchedCredentials {
    original: FsIdentity,
    capabilities: Capabilities,
    /// The supplementary groups, if they were cleared.
    groups: Option<Vec<lx::gid_t>>,
    /// Whether the filesystem UID and GID may have changed.
    switched_identity: bool,
    /// Whether the effective capabilities were cleared.
    cleared_capabilities: bool,
}

impl SwitchedCredentials {
    fn switch(identity: FsIdentity) -> lx::Result<Self> {
        if identity.uid == INVALID_ID || identity.gid == INVALID_ID {
            return Err(lx::Error::EPERM);
        }
        let mut credentials = Self {
            original: FsIdentity::current(),
            capabilities: capabilities()?,
            groups: None,
            switched_identity: false,
            cleared_capabilities: false,
        };
        // On an error below, dropping `credentials` restores whatever was already changed.
        let groups = groups()?;
        if groups.iter().any(|&group| group != identity.gid) {
            set_groups(&[])?;
            credentials.groups = Some(groups);
        }
        if identity != credentials.original {
            credentials.switched_identity = true;
            set_fsgid(identity.gid);
            set_fsuid(identity.uid);
            // Neither call reports failure, so read back the identity actually in effect.
            if FsIdentity::current() != identity {
                return Err(lx::Error::EPERM);
            }
        }
        if credentials
            .capabilities
            .iter()
            .any(|data| data.effective != 0)
        {
            let cleared = credentials.capabilities.map(|data| CapabilityData {
                effective: 0,
                ..data
            });
            set_capabilities(&cleared)?;
            credentials.cleared_capabilities = true;
        }
        Ok(credentials)
    }
}

impl Drop for SwitchedCredentials {
    fn drop(&mut self) {
        // Restore the capabilities first, because resetting the groups and the identity may need
        // CAP_SETGID and CAP_SETUID.
        if self.cleared_capabilities || self.switched_identity || self.groups.is_some() {
            set_capabilities(&self.capabilities)
                .expect("failed to restore the thread's effective capabilities");
        }
        if self.switched_identity {
            set_fsgid(self.original.gid);
            set_fsuid(self.original.uid);
            assert_eq!(
                FsIdentity::current(),
                self.original,
                "failed to restore the thread's filesystem identity"
            );
        }
        if let Some(groups) = &self.groups {
            set_groups(groups).expect("failed to restore the thread's supplementary groups");
        }
        if self.switched_identity {
            // Changing the filesystem UID to or from 0 adjusts the effective capabilities.
            set_capabilities(&self.capabilities)
                .expect("failed to restore the thread's effective capabilities");
        }
    }
}

/// Sets the calling thread's filesystem UID and returns the previous one.
fn set_fsuid(uid: lx::uid_t) -> lx::uid_t {
    // SAFETY: setfsuid takes an integer and changes only the calling thread's credentials.
    let previous = unsafe { libc::syscall(libc::SYS_setfsuid, libc::c_long::from(uid)) };
    previous as lx::uid_t
}

/// Sets the calling thread's filesystem GID and returns the previous one.
fn set_fsgid(gid: lx::gid_t) -> lx::gid_t {
    // SAFETY: setfsgid takes an integer and changes only the calling thread's credentials.
    let previous = unsafe { libc::syscall(libc::SYS_setfsgid, libc::c_long::from(gid)) };
    previous as lx::gid_t
}

/// Returns the calling thread's supplementary groups.
fn groups() -> lx::Result<Vec<lx::gid_t>> {
    // SAFETY: A zero size only queries the number of groups.
    let count = util::check_lx_errno(unsafe { libc::getgroups(0, std::ptr::null_mut()) })?;
    let mut groups = vec![0; count as usize];
    // SAFETY: `groups` has room for `count` IDs.
    let count = util::check_lx_errno(unsafe { libc::getgroups(count, groups.as_mut_ptr()) })?;
    groups.truncate(count as usize);
    Ok(groups)
}

/// Sets the calling thread's supplementary groups.
fn set_groups(groups: &[lx::gid_t]) -> lx::Result<()> {
    let count = libc::c_long::try_from(groups.len()).map_err(|_| lx::Error::EINVAL)?;
    // SAFETY: `groups` holds `count` IDs. The raw system call changes only the calling thread.
    util::check_lx_errno(unsafe { libc::syscall(libc::SYS_setgroups, count, groups.as_ptr()) })?;
    Ok(())
}

/// Returns the calling thread's capabilities.
fn capabilities() -> lx::Result<Capabilities> {
    let mut header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = Capabilities::default();
    // SAFETY: The header and both data structures are valid for writes.
    util::check_lx_errno(unsafe {
        libc::syscall(libc::SYS_capget, &raw mut header, data.as_mut_ptr())
    })?;
    Ok(data)
}

/// Sets the calling thread's capabilities.
fn set_capabilities(data: &Capabilities) -> lx::Result<()> {
    let mut header = CapabilityHeader {
        version: CAPABILITY_VERSION_3,
        pid: 0,
    };
    // SAFETY: The header and both data structures are valid. A zero PID selects the calling
    // thread, the only one whose capabilities capset changes.
    util::check_lx_errno(unsafe {
        libc::syscall(libc::SYS_capset, &raw mut header, data.as_ptr())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    const CAP_SETGID: u32 = 6;
    const CAP_SETUID: u32 = 7;

    fn has_capability(capability: u32) -> bool {
        let data = capabilities().unwrap();
        data[(capability / 32) as usize].effective & (1 << (capability % 32)) != 0
    }

    /// Returns an identity that differs from every ID of the calling thread.
    fn foreign_identity() -> FsIdentity {
        // SAFETY: These calls have no preconditions.
        let ids = unsafe {
            [
                libc::getuid(),
                libc::geteuid(),
                libc::getgid(),
                libc::getegid(),
            ]
        };
        let current = FsIdentity::current();
        let unused = |mut candidate: u32| {
            while ids.contains(&candidate) || candidate == current.uid || candidate == current.gid {
                candidate += 1;
            }
            candidate
        };
        FsIdentity {
            uid: unused(4242),
            gid: unused(4343),
        }
    }

    fn assert_restored(original: FsIdentity, groups_before: &[u32], before: &Capabilities) {
        assert_eq!(FsIdentity::current(), original);
        assert_eq!(groups().unwrap(), groups_before);
        assert_eq!(&capabilities().unwrap(), before);
    }

    /// Returns whether the calling thread has supplementary groups other than `gid`.
    fn has_other_groups(gid: u32) -> bool {
        groups().unwrap().iter().any(|&group| group != gid)
    }

    /// Runs `test` on a new thread that has no effective capabilities, after giving that thread
    /// exactly the supplementary `groups` if it may.
    fn without_capabilities(groups: Vec<u32>, test: impl FnOnce() + Send + 'static) {
        std::thread::spawn(move || {
            if has_capability(CAP_SETGID) {
                set_groups(&groups).unwrap();
            }
            let mut data = capabilities().unwrap();
            for word in &mut data {
                word.effective = 0;
            }
            set_capabilities(&data).unwrap();
            test();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn current_identity_is_the_effective_identity() {
        // SAFETY: These calls have no preconditions.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        assert_eq!(FsIdentity::current(), FsIdentity { uid, gid });
    }

    #[test]
    fn same_identity_without_other_groups_needs_no_privilege() {
        without_capabilities(Vec::new(), || {
            let original = FsIdentity::current();
            if has_other_groups(original.gid) {
                // An unprivileged thread cannot drop the groups that it was given.
                return;
            }
            let directory = tempfile::tempdir().unwrap();
            let groups_before = groups().unwrap();
            let before = capabilities().unwrap();
            let path = directory.path().join("file");
            let (identity, groups_inside, inside) = with_fs_identity(original, || {
                std::fs::write(&path, b"data").unwrap();
                (
                    FsIdentity::current(),
                    groups().unwrap(),
                    capabilities().unwrap(),
                )
            })
            .unwrap();

            assert_eq!(identity, original);
            assert_eq!(groups_inside, groups_before);
            assert!(inside.iter().all(|data| data.effective == 0));
            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!(metadata.uid(), original.uid);
            assert_restored(original, &groups_before, &before);
        });
    }

    #[test]
    fn other_groups_fail_closed_without_privilege() {
        // A group that the thread is given only if the test process is privileged.
        const OTHER_GROUP: u32 = 4444;
        // SAFETY: getegid has no preconditions.
        let gid = unsafe { libc::getegid() };
        without_capabilities(vec![gid, OTHER_GROUP], || {
            let original = FsIdentity::current();
            if !has_other_groups(original.gid) {
                // An unprivileged thread without other groups cannot be given one.
                return;
            }
            let directory = tempfile::tempdir().unwrap();
            let groups_before = groups().unwrap();
            let before = capabilities().unwrap();
            let ran = AtomicBool::new(false);
            let path = directory.path().join("file");
            let result = with_fs_identity(original, || {
                ran.store(true, Ordering::Relaxed);
                std::fs::write(&path, b"data")
            });

            assert_eq!(result.unwrap_err(), lx::Error::EPERM);
            assert!(!ran.load(Ordering::Relaxed));
            assert!(!path.exists());
            assert_restored(original, &groups_before, &before);
        });
    }

    #[test]
    fn foreign_identity_fails_closed_without_privilege() {
        // Capabilities are per thread, so drop them on a dedicated thread. The test then behaves
        // the same whether or not the test process is privileged.
        std::thread::spawn(|| {
            let mut data = capabilities().unwrap();
            for word in &mut data {
                word.effective = 0;
            }
            set_capabilities(&data).unwrap();

            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777))
                .unwrap();
            let original = FsIdentity::current();
            let groups_before = groups().unwrap();
            let before = capabilities().unwrap();
            let ran = AtomicBool::new(false);
            let path = directory.path().join("file");
            let result = with_fs_identity(foreign_identity(), || {
                ran.store(true, Ordering::Relaxed);
                std::fs::write(&path, b"data")
            });

            assert_eq!(result.unwrap_err(), lx::Error::EPERM);
            assert!(!ran.load(Ordering::Relaxed));
            assert!(!path.exists());
            assert_restored(original, &groups_before, &before);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn invalid_identity_fails_closed() {
        let original = FsIdentity::current();
        for identity in [
            FsIdentity {
                uid: INVALID_ID,
                gid: original.gid,
            },
            FsIdentity {
                uid: original.uid,
                gid: INVALID_ID,
            },
        ] {
            let ran = AtomicBool::new(false);
            let result = with_fs_identity(identity, || ran.store(true, Ordering::Relaxed));
            assert_eq!(result.unwrap_err(), lx::Error::EPERM);
            assert!(!ran.load(Ordering::Relaxed));
            assert_eq!(FsIdentity::current(), original);
        }
    }

    /// Run with `CAP_SETUID` and `CAP_SETGID`, for example as root.
    #[test]
    #[ignore = "requires CAP_SETUID and CAP_SETGID"]
    fn foreign_identity_switches_one_thread_with_privilege() {
        assert!(
            has_capability(CAP_SETUID) && has_capability(CAP_SETGID),
            "this test requires CAP_SETUID and CAP_SETGID"
        );
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let original = FsIdentity::current();
        let groups_before = groups().unwrap();
        let before = capabilities().unwrap();
        let identity = foreign_identity();

        // Another thread keeps its identity while this one is switched.
        let (request, requests) = std::sync::mpsc::channel::<()>();
        let (reply, replies) = std::sync::mpsc::channel();
        let observer = std::thread::spawn(move || {
            for () in requests {
                reply.send(FsIdentity::current()).unwrap();
            }
        });

        let (inside, groups_inside, capabilities_inside, other_thread) =
            with_fs_identity(identity, || {
                std::fs::write(directory.path().join("file"), b"data").unwrap();
                std::fs::create_dir(directory.path().join("directory")).unwrap();
                std::fs::write(directory.path().join("directory/nested"), b"data").unwrap();
                request.send(()).unwrap();
                (
                    FsIdentity::current(),
                    groups().unwrap(),
                    capabilities().unwrap(),
                    replies.recv().unwrap(),
                )
            })
            .unwrap();
        drop(request);
        observer.join().unwrap();

        assert_eq!(inside, identity);
        assert!(groups_inside.is_empty());
        assert!(capabilities_inside.iter().all(|data| data.effective == 0));
        assert_eq!(other_thread, original);
        for name in ["file", "directory", "directory/nested"] {
            let metadata = std::fs::symlink_metadata(directory.path().join(name)).unwrap();
            assert_eq!(
                (metadata.uid(), metadata.gid()),
                (identity.uid, identity.gid),
                "{name}"
            );
        }
        assert_restored(original, &groups_before, &before);

        // The switched identity is subject to ordinary permission checks.
        let private = tempfile::tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let denied = with_fs_identity(identity, || {
            std::fs::write(private.path().join("file"), b"data")
        })
        .unwrap();
        assert_eq!(
            denied.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_restored(original, &groups_before, &before);

        // Only the switched identity can empty the directory it created.
        with_fs_identity(identity, || {
            std::fs::remove_dir_all(directory.path().join("directory"))
        })
        .unwrap()
        .unwrap();
    }

    /// Run with `CAP_SETGID`, for example as root.
    #[test]
    #[ignore = "requires CAP_SETGID"]
    fn same_identity_drops_other_groups_with_privilege() {
        assert!(has_capability(CAP_SETGID), "this test requires CAP_SETGID");
        let other_group = foreign_identity().gid;
        // Only this thread's groups change.
        std::thread::spawn(move || {
            let original = FsIdentity::current();
            set_groups(&[original.gid, other_group]).unwrap();
            let groups_before = groups().unwrap();
            let before = capabilities().unwrap();
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("file");
            std::fs::write(&path, b"data").unwrap();

            let (identity, groups_inside, inside, chgrp) = with_fs_identity(original, || {
                (
                    FsIdentity::current(),
                    groups().unwrap(),
                    capabilities().unwrap(),
                    std::os::unix::fs::chown(&path, None, Some(other_group)),
                )
            })
            .unwrap();

            assert_eq!(identity, original);
            assert!(groups_inside.iter().all(|&group| group == original.gid));
            assert!(inside.iter().all(|data| data.effective == 0));
            // The operation cannot give a file a group that only the thread has.
            assert_eq!(
                chgrp.unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_restored(original, &groups_before, &before);
            std::os::unix::fs::chown(&path, None, Some(other_group)).unwrap();
        })
        .join()
        .unwrap();
    }
}
