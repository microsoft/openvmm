// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Linux backend for [`apply`](crate::apply) and [`tighten`](crate::tighten).
//!
//! On Linux the worker starts inside the user, mount, and network namespaces
//! requested by [`prepare`](crate::prepare), then configures the namespaces
//! selected by the profile here: a `pivot_root` onto a purpose-built root,
//! capability drop and securebits lock, process-hardening `prctl`s`,
//! `no_new_privs`, and an optional seccomp filter. The composition order is
//! load-bearing and mirrors the order validated against Bubblewrap, `runc`,
//! systemd, Firejail, and the Chromium sandbox.
//!
//! # Failure policy
//!
//! Every primitive this backend applies is *required*: if it fails, the worker
//! aborts via [`Error`] rather than run with a weaker sandbox than its profile
//! declares (fail-closed). Namespaces, the `pivot_root` scaffold, capability
//! drop, securebits, filesystem grants, `PR_SET_DUMPABLE`, and `no_new_privs`
//! all abort on failure.
//!
//! The two deliberate exceptions are *supplementary* layers whose failure
//! policy differs. **Landlock** (an FS-isolation supplement to the mount
//! namespace, ABI-degraded on older kernels per D11/D16) logs and continues on
//! failure; the mount namespace remains the primary FS boundary. **Seccomp** is
//! *opt-in* per worker class (D13) — the deny-all default installs no filter —
//! but once a profile opts in with [`Syscalls::Deny`], applying it is required
//! and a failure aborts.

use crate::Error;
use crate::profile::Access;
use crate::profile::FsGrant;
use crate::profile::Network;
use crate::profile::Profile;
use crate::profile::Restrictions;
use crate::profile::Syscalls;
use crate::unix::hardening;
use crate::unix::mount_namespace;
use crate::unix::network_namespace;
use crate::unix::seccomp;
use fs_err::os::unix::fs::OpenOptionsExt;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::path::PathBuf;

/// Staging path for the tmpfs that becomes `/` after `pivot_root`.
///
/// It exists only inside our fresh mount namespace, so it can never collide
/// with another process's view of the filesystem.
const STAGING_ROOT: &str = "/tmp/.sandbox-root";

/// Apply `profile` to the current process. See the module docs for ordering
/// and failure policy.
pub fn apply(profile: &Profile) -> Result<(), Error> {
    let p = &profile.inner;

    // Step 1 — detach propagation in the mount namespace created with the
    // child. Even a profile with no filesystem grants pivots to a minimal
    // tmpfs root; an empty grant set must deny rather than preserve the host
    // filesystem view.
    mount_namespace::make_root_private().map_err(required("mount_namespace"))?;

    // Step 2 — configure an isolated network namespace, when requested, before
    // filesystem setup so any FS step that could reach the network is already
    // contained.
    apply_network(p.network)?;

    // Step 3 — filesystem: tmpfs root, per-grant binds, /proc, /tmp,
    // pivot_root, then opportunistic Landlock.
    apply_filesystem(&p.fs)?;

    // Step 4 — lock securebits *before* the capability drop, while we still
    // hold CAP_SETPCAP. Required (design §9): a worker that can regain root-like
    // authority across an execve is not sandboxed.
    hardening::lock_securebits().map_err(required("securebits"))?;

    // Step 5 — drop every capability. Required: this is the deny-all base.
    drop_all_capabilities()?;

    hardening::set_dumpable_off().map_err(required("dumpable"))?;

    // Step 6 — no_new_privs. Required, and a prerequisite for unprivileged
    // seccomp below.
    hardening::set_no_new_privs().map_err(required("no_new_privs"))?;

    // Step 7 — seccomp, *opt-in* per worker class (design D13). The deny-all
    // default (`Syscalls::Unfiltered`) installs no filter; an explicit
    // `Syscalls::Deny` installs the dangerous-syscall baseline plus any extras.
    if let Syscalls::Deny(names) = &p.syscalls {
        let names = names.iter().map(String::as_str).collect::<Vec<_>>();
        seccomp::apply_denylist(&names, seccomp_deny_action()).map_err(required("seccomp"))?;
    }

    tracing::debug!(profile = p.name, "sandbox applied");
    Ok(())
}

/// Stack additional, strictly-narrowing restrictions onto an already-applied
/// sandbox. Runs after the worker has initialized and may be multi-threaded.
pub fn tighten(restrictions: &Restrictions) -> Result<(), Error> {
    let r = &restrictions.inner;

    // Path revocation via a stacked Landlock ruleset needs the base profile's
    // full grant set to re-express (Landlock layers intersect), which is not
    // available here. Rather than silently fail open, reject it (R-S12).
    if !r.revoke_paths.is_empty() {
        return Err(Error::RequiredPrimitiveUnavailable(
            "path revocation via tighten is not supported in v1",
        ));
    }

    // Stack an additional syscall denylist across all threads using TSYNC.
    // Filters compose; the kernel takes the most restrictive verdict.
    if let Some(names) = &r.syscalls {
        seccomp::apply_denylist(names, seccomp_deny_action()).map_err(required("seccomp"))?;
    }

    Ok(())
}

/// Configure the network namespace to match `scope`. Required: a failure
/// aborts.
fn apply_network(scope: Network) -> Result<(), Error> {
    match scope {
        Network::None | Network::Unrestricted => Ok(()),
        Network::Loopback => network_namespace::bring_up_loopback(),
    }
    .map_err(required("network_namespace"))
}

/// Real filesystem setup: tmpfs staging root, per-grant bind mounts,
/// `pivot_root`, old-root cleanup, and layered Landlock enforcement.
fn apply_filesystem(grants: &[FsGrant]) -> Result<(), Error> {
    let grants = filesystem_mount_plan(grants)?;
    let staging = Path::new(STAGING_ROOT);

    fs_err::create_dir_all(staging).map_err(fs_required)?;
    mount_namespace::mount_tmpfs(staging).map_err(fs_required)?;

    // Install the writable tmpfs before grants so explicit paths beneath
    // /tmp (such as a resolver symlink target) remain visible.
    let tmp_target = staging.join("tmp");
    fs_err::create_dir_all(&tmp_target).map_err(fs_required)?;
    mount_namespace::mount_tmpfs(&tmp_target).map_err(fs_required)?;

    // Bind each grant into the staging root at the same path
    // (source `/bin` => `staging/bin`).
    for grant in &grants {
        let source = grant.path.as_path();
        if !source.exists() {
            return Err(Error::ApplyFailed {
                primitive: "filesystem",
                source: io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "filesystem grant source does not exist: {}",
                        source.display()
                    ),
                ),
            });
        }
        let device = if grant.access == Access::Device {
            let device = fs_err::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(source)
                .map_err(fs_required)?;
            let file_type = device.metadata().map_err(fs_required)?.file_type();
            if !file_type.is_char_device() && !file_type.is_block_device() {
                return Err(fs_required(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "device grant source is not a device node: {}",
                        source.display()
                    ),
                )));
            }
            Some(device)
        } else {
            None
        };
        let target = join_under(staging, source);
        let attributes = mount_attrs_for_access(grant.access);
        // Pin a validated device inode so replacement of its pathname cannot
        // turn a single-device exception into a directory grant.
        let pinned = device
            .as_ref()
            .map(|device| PathBuf::from(format!("/proc/self/fd/{}", device.as_raw_fd())));
        let source = pinned.as_deref().unwrap_or(source);
        mount_namespace::bind_mount(source, &target, attributes).map_err(fs_required)?;
    }

    // Best-effort `/proc`: works cleanly in a PID namespace and is still useful
    // for `/proc/self` when we don't have one.
    let proc_target = staging.join("proc");
    if mount_namespace::mount_proc(&proc_target).is_err() {
        let _ = mount_namespace::bind_mount(
            Path::new("/proc"),
            &proc_target,
            libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV,
        );
    }

    // Pivot root: staging becomes `/`, old root moves to `staging/.old_root`.
    let old_root = staging.join(".old_root");
    fs_err::create_dir_all(&old_root).map_err(fs_required)?;
    mount_namespace::pivot_root(staging, &old_root).map_err(fs_required)?;
    std::env::set_current_dir("/").map_err(fs_required)?;

    // Detach and remove the old root so nothing leaks through.
    mount_namespace::umount(Path::new("/.old_root")).map_err(fs_required)?;
    let _ = fs_err::remove_dir("/.old_root");

    // Landlock is a supplementary, opportunistic layer (design D11/D16): a
    // failure degrades rather than aborts.
    if let Err(e) = apply_landlock(&grants) {
        tracing::warn!(
            error = &e as &dyn std::error::Error,
            "could not apply Landlock ruleset; relying on mount namespace only",
        );
    }

    Ok(())
}

fn filesystem_mount_plan(grants: &[FsGrant]) -> Result<Vec<FsGrant>, Error> {
    let mut ordered = grants.to_vec();
    ordered.sort_by(|left, right| left.path.cmp(&right.path));

    let mut plan: Vec<FsGrant> = Vec::new();
    for mut grant in ordered {
        // Path ordering puts ancestors first. Preserve their permissions in
        // descendant mounts and combine repeated grants before mounting.
        for parent in &plan {
            if grant.path.starts_with(&parent.path) {
                grant.access = match (parent.access, grant.access) {
                    (Access::Read, access) | (access, Access::Read) => access,
                    (Access::ReadExec, Access::ReadExec) => Access::ReadExec,
                    (Access::ReadWrite, Access::ReadWrite) => Access::ReadWrite,
                    (Access::Device, Access::Device | Access::ReadWrite)
                    | (Access::ReadWrite, Access::Device) => Access::Device,
                    _ => {
                        return Err(fs_required(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "overlapping filesystem grants require both write and execute access: {} and {}",
                                parent.path.display(),
                                grant.path.display(),
                            ),
                        )));
                    }
                };
            }
        }
        if let Some(previous) = plan.last_mut().filter(|last| last.path == grant.path) {
            previous.access = grant.access;
        } else {
            plan.push(grant);
        }
    }
    Ok(plan)
}

/// Translate a policy [`Access`] into `MOUNT_ATTR_*` restrictions.
///
/// All mounts are `NOSUID`; ordinary grants are also `NODEV`.
/// Only explicit, single-device grants permit device-node access.
fn mount_attrs_for_access(access: Access) -> u64 {
    const BASE: u64 = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV;
    match access {
        Access::Read => BASE | libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOEXEC,
        Access::ReadExec => BASE | libc::MOUNT_ATTR_RDONLY,
        Access::ReadWrite => BASE | libc::MOUNT_ATTR_NOEXEC,
        Access::Device => libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NOEXEC,
    }
}

/// Compose `root/relative(path)`: `Path::join` returns `path` unchanged when it
/// is absolute, so strip the leading separator first.
fn join_under(root: &Path, path: &Path) -> PathBuf {
    let stripped = path.strip_prefix("/").unwrap_or(path);
    root.join(stripped)
}

/// Install a Landlock ruleset covering grants and implicit filesystem access.
///
/// Requests the highest known Landlock ABI and lets the `landlock` crate
/// downgrade automatically on older kernels.
fn apply_landlock(grants: &[FsGrant]) -> io::Result<landlock::RestrictionStatus> {
    use landlock::ABI;
    use landlock::Access as _;
    use landlock::AccessFs;
    use landlock::Ruleset;
    use landlock::RulesetAttr;
    use landlock::RulesetCreatedAttr;
    use landlock::path_beneath_rules;

    let abi = ABI::V5;

    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(landlock_err)?
        .create()
        .map_err(landlock_err)?;

    let from_read = AccessFs::from_read(abi);
    let from_write = AccessFs::from_write(abi);

    for grant in grants {
        let source = grant.path.as_path();
        if !source.exists() {
            continue;
        }
        let rights = match grant.access {
            Access::Read => from_read & !AccessFs::Execute,
            Access::ReadExec => from_read,
            Access::ReadWrite => (from_read & !AccessFs::Execute) | from_write,
            Access::Device => (from_read & !AccessFs::Execute) | from_write | AccessFs::IoctlDev,
        };
        ruleset = ruleset
            .add_rules(path_beneath_rules(&[source], rights))
            .map_err(landlock_err)?;
    }

    for (path, rights) in [
        ("/proc/self/maps", AccessFs::ReadFile.into()),
        (
            "/proc/self/comm",
            // fs::write uses O_TRUNC when setting the worker name.
            AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate,
        ),
        ("/proc/sys/vm/overcommit_memory", AccessFs::ReadFile.into()),
    ] {
        let path = Path::new(path);
        if !path.exists() {
            continue;
        }
        ruleset = ruleset
            .add_rules(path_beneath_rules(&[path], rights))
            .map_err(landlock_err)?;
    }

    let tmp_rights = (from_read & !AccessFs::Execute) | from_write;
    ruleset = ruleset
        .add_rules(path_beneath_rules(&[Path::new("/tmp")], tmp_rights))
        .map_err(landlock_err)?;

    let status = ruleset.restrict_self().map_err(landlock_err)?;
    tracing::debug!(
        ruleset = ?status.ruleset,
        no_new_privs = status.no_new_privs,
        "applied Landlock ruleset",
    );
    Ok(status)
}

fn landlock_err(e: landlock::RulesetError) -> io::Error {
    io::Error::other(format!("landlock: {e}"))
}

/// Drop every OS capability from the current thread's Ambient, Bounding,
/// Inheritable, Effective, and Permitted sets.
///
/// Order matters: Ambient first (independent), then Bounding (needs
/// `CAP_SETPCAP`, still in Effective), then Inheritable, then Effective, then
/// Permitted. Clearing Effective before Permitted preserves the kernel's
/// `Effective ⊆ Permitted` invariant.
fn drop_all_capabilities() -> Result<(), Error> {
    for set in [
        caps::CapSet::Ambient,
        caps::CapSet::Bounding,
        caps::CapSet::Inheritable,
        caps::CapSet::Effective,
        caps::CapSet::Permitted,
    ] {
        caps::clear(None, set).map_err(|e| Error::ApplyFailed {
            primitive: "capabilities",
            source: io::Error::other(format!("caps::clear({set:?}): {e}")),
        })?;
    }
    Ok(())
}

/// The seccomp deny action: kill the process in production (design D13), log
/// (allow + record) in dev so the tuning loop can observe missing syscalls.
/// The action taken when a denied syscall is invoked.
///
/// Release builds `KillProcess` — the denied set is a fixed, intentional list
/// of escape vectors, so hitting one is a hard fault. Debug builds return
/// `EPERM` instead: still enforced (unlike `Log`, which allows the call), but
/// survivable, so a developer sees the `errno` rather than an opaque kill.
fn seccomp_deny_action() -> seccompiler::SeccompAction {
    if cfg!(debug_assertions) {
        seccompiler::SeccompAction::Errno(libc::EPERM as u32)
    } else {
        seccompiler::SeccompAction::KillProcess
    }
}

/// Build a closure mapping an [`io::Error`] to a required-primitive
/// [`Error::ApplyFailed`].
fn required(primitive: &'static str) -> impl FnOnce(io::Error) -> Error {
    move |source| Error::ApplyFailed { primitive, source }
}

/// Shorthand for the required filesystem-scaffold steps.
fn fs_required(source: io::Error) -> Error {
    Error::ApplyFailed {
        primitive: "filesystem",
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::ToSocketAddrs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixDatagram;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::sync::mpsc;
    use test_with_tracing::test;

    #[test]
    fn tighten_restricts_existing_threads() {
        let output = Command::new(std::env::current_exe().unwrap())
            .env_remove(crate::DISABLE_ENV)
            .args([
                "--exact",
                "linux::tests::helper_tighten_existing_thread",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        // Debug builds return EPERM; release builds kill the process when the
        // existing thread attempts the newly denied syscall.
        let expected_status = if cfg!(debug_assertions) {
            output.status.success()
        } else {
            output.status.signal() == Some(libc::SIGSYS)
        };
        assert!(
            expected_status,
            "tighten helper failed ({}):\nstdout: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore]
    fn helper_tighten_existing_thread() {
        hardening::set_no_new_privs().unwrap();
        seccomp::apply_denylist(&[], seccomp_deny_action()).unwrap();

        let (ready_tx, ready_rx) = mpsc::channel();
        let (tightened_tx, tightened_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            drop(UnixDatagram::unbound().unwrap());
            ready_tx.send(()).unwrap();
            tightened_rx.recv().unwrap();
            assert_eq!(
                UnixDatagram::unbound().unwrap_err().raw_os_error(),
                Some(libc::EPERM),
            );
        });

        ready_rx.recv().unwrap();
        let restrictions = Restrictions::none().syscalls(&["socket"]).build();
        crate::tighten(&restrictions).unwrap();
        tightened_tx.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn landlock_allows_implicit_proc_access() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "linux::tests::helper_implicit_proc_access",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "implicit proc helper failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore]
    fn helper_implicit_proc_access() {
        fs_err::read("/proc/self/status").unwrap();
        hardening::set_no_new_privs().unwrap();
        let status = apply_landlock(&[]).unwrap();
        println!("Landlock enforcement: {:?}", status.ruleset);
        if status.ruleset == landlock::RulesetStatus::NotEnforced {
            return;
        }

        assert_eq!(
            fs_err::read("/proc/self/status").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
        );
        for path in ["/proc/self/maps", "/proc/sys/vm/overcommit_memory"] {
            assert!(!fs_err::read(path).unwrap().is_empty());
        }
        fs_err::write("/proc/self/comm", "sandbox-proc").unwrap();
        assert_eq!(
            fs_err::read_to_string("/proc/self/comm").unwrap(),
            "sandbox-proc\n",
        );
    }

    #[test]
    fn filesystem_grants_enforce_access_modes() {
        for order in ["child-first", "parent-first"] {
            let output = Command::new("unshare")
                .args(["--user", "--map-root-user", "--mount"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "linux::tests::helper_filesystem_grants",
                    "--ignored",
                    "--nocapture",
                ])
                .env("SANDBOX_GRANT_ORDER", order)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{order} filesystem grant helper failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    #[test]
    #[ignore]
    fn helper_filesystem_grants() {
        mount_namespace::make_root_private().unwrap();
        mount_namespace::mount_tmpfs(Path::new("/run")).unwrap();

        let parent_first = std::env::var("SANDBOX_GRANT_ORDER").unwrap() == "parent-first";
        let mut profile = Profile::deny_all();
        if parent_first {
            profile = profile
                .read("/run/grants")
                .read("/run/grants/read-write")
                .read("/run/grants/read-exec");
        }
        profile = profile
            .read("/run/grants/read")
            .read_exec("/run/grants/read-exec")
            .read_write("/run/grants/read-write")
            .read("/run/grants/read-write/nested")
            .read("/run/grants/read-exec/nested")
            .read("/sys/devices/system/cpu");
        if !parent_first {
            profile = profile
                .read("/run/grants/read-write")
                .read("/run/grants/read-exec")
                .read("/run/grants");
        }
        for path in ["/bin", "/lib", "/lib64", "/usr/lib"] {
            if Path::new(path).exists() {
                profile = profile.read_exec(path);
            }
        }
        let script = "#!/bin/sh\nexit 0\n";
        let cases = [
            ("/run/grants/read-write", true, false),
            ("/run/grants/read-write/nested", true, false),
            ("/run/grants/read-exec", false, true),
            ("/run/grants/read-exec/nested", false, true),
            ("/run/grants/read", false, false),
            ("/run/grants", false, false),
        ];
        for (directory, _, _) in cases {
            fs_err::create_dir_all(directory).unwrap();
            let path = Path::new(directory).join("probe");
            fs_err::write(&path, script).unwrap();
            fs_err::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        hardening::set_no_new_privs().unwrap();
        apply_filesystem(&profile.build().inner.fs).unwrap();

        assert!(
            !fs_err::read_to_string("/sys/devices/system/cpu/online")
                .unwrap()
                .is_empty()
        );
        for (directory, writable, executable) in cases {
            let directory = Path::new(directory);
            let path = directory.join("probe");
            assert_eq!(fs_err::read_to_string(&path).unwrap(), script);
            assert_eq!(
                fs_err::write(directory.join("new-file"), "probe").is_ok(),
                writable,
                "unexpected write permission on {}",
                directory.display(),
            );
            if executable {
                assert!(Command::new(&path).status().unwrap().success());
            } else {
                assert_eq!(
                    Command::new(&path).status().unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied,
                    "execution was not denied on {}",
                    directory.display(),
                );
            }
        }
    }

    #[test]
    fn filesystem_grants_reject_write_execute_overlaps() {
        for child in ["/run/grants", "/run/grants/child"] {
            for (parent_access, child_access) in [
                (Access::ReadWrite, Access::ReadExec),
                (Access::ReadExec, Access::ReadWrite),
            ] {
                for reverse in [false, true] {
                    let mut grants = vec![
                        FsGrant {
                            path: PathBuf::from("/run/grants"),
                            access: parent_access,
                        },
                        FsGrant {
                            path: PathBuf::from(child),
                            access: child_access,
                        },
                    ];
                    if reverse {
                        grants.reverse();
                    }
                    assert!(matches!(
                        filesystem_mount_plan(&grants),
                        Err(Error::ApplyFailed { source, .. })
                            if source.kind() == io::ErrorKind::InvalidInput
                    ));
                }
            }
        }
    }

    #[test]
    fn worker_profiles_preserve_host_resources() {
        for scenario in [
            "resolver",
            "resolver-chain",
            "resolver-parent",
            "resolver-relative",
            "resolver-tmp",
            "resolver-local",
            "resolver-missing",
            "devices",
            "invalid-device",
            "pci",
            "etc",
            "etc-missing",
            "tpm-etc",
            "tpm-etc-missing",
        ] {
            let output = Command::new("unshare")
                .args(["--user", "--map-root-user", "--mount"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "linux::tests::helper_worker_profile_resources",
                    "--ignored",
                    "--nocapture",
                ])
                .env("SANDBOX_RESOURCE_SCENARIO", scenario)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{scenario} helper failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    #[test]
    #[ignore]
    fn helper_worker_profile_resources() {
        let scenario = std::env::var("SANDBOX_RESOURCE_SCENARIO").unwrap();
        let null = std::fs::File::options()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        // UNSAFETY: an ioctl probe is needed to verify Landlock device rights.
        #[expect(unsafe_code)]
        let ioctl_error = |file: &std::fs::File| {
            // SAFETY: this argument-free probe targets only our /dev/null
            // fixture, whose driver rejects unsupported commands.
            assert_eq!(unsafe { libc::ioctl(file.as_raw_fd(), 0x5800, 0) }, -1);
            io::Error::last_os_error().raw_os_error()
        };
        let baseline_ioctl = ioctl_error(&null);
        mount_namespace::make_root_private().unwrap();
        for path in ["/etc", "/run", "/sys", "/dev"] {
            mount_namespace::mount_tmpfs(Path::new(path)).unwrap();
        }
        fs_err::create_dir_all("/run/resolve").unwrap();
        fs_err::write("/run/resolve/resolv.conf", "nameserver 192.0.2.1\n").unwrap();
        fs_err::write("/run/resolve/unrelated", "not granted").unwrap();
        std::os::unix::fs::symlink("/run/resolve/resolv.conf", "/etc/resolv.conf").unwrap();
        match scenario.as_str() {
            "resolver-chain" => {
                fs_err::rename("/run/resolve/resolv.conf", "/run/resolve/actual.conf").unwrap();
                std::os::unix::fs::symlink("actual.conf", "/run/resolve/resolv.conf").unwrap();
            }
            "resolver-parent" => {
                fs_err::remove_file("/etc/resolv.conf").unwrap();
                std::os::unix::fs::symlink("/run/resolve", "/etc/resolve").unwrap();
                std::os::unix::fs::symlink("resolve/resolv.conf", "/etc/resolv.conf").unwrap();
            }
            "resolver-relative" => {
                fs_err::remove_file("/etc/resolv.conf").unwrap();
                std::os::unix::fs::symlink("../../run/resolve/resolv.conf", "/etc/resolv.conf")
                    .unwrap();
            }
            "resolver-tmp" => {
                fs_err::create_dir("/run/resolver-tmp").unwrap();
                mount_namespace::mount_tmpfs(Path::new("/run/resolver-tmp")).unwrap();
                mount_namespace::bind_mount(Path::new("/run/resolver-tmp"), Path::new("/tmp"), 0)
                    .unwrap();
                fs_err::write("/tmp/resolv.conf", "nameserver 192.0.2.1\n").unwrap();
                fs_err::remove_file("/etc/resolv.conf").unwrap();
                std::os::unix::fs::symlink("/tmp/resolv.conf", "/etc/resolv.conf").unwrap();
            }
            "resolver-missing" => {
                fs_err::remove_file("/run/resolve/resolv.conf").unwrap();
            }
            "resolver-local" => {
                fs_err::remove_file("/etc/resolv.conf").unwrap();
                fs_err::write("/etc/resolv.conf", "nameserver 192.0.2.1\n").unwrap();
            }
            _ => {}
        }

        let configuration_files = [
            ("/etc/ld.so.cache", "cache fixture\n"),
            ("/etc/hosts", "192.0.2.2 sandbox.invalid\n"),
            ("/etc/host.conf", "multi on\n"),
            ("/etc/gai.conf", "# configuration fixture\n"),
            ("/etc/nsswitch.conf", "hosts: files dns\n"),
            ("/etc/ssl/openssl.cnf", "# OpenSSL fixture\n"),
            ("/etc/pki/tls/openssl.cnf", "# OpenSSL fixture\n"),
        ];
        let private_files = [
            "/etc/shadow",
            "/etc/ssh/ssh_host_ed25519_key",
            "/etc/ssl/private/key.pem",
            "/etc/pki/tls/private/key.pem",
        ];
        if scenario.contains("etc") {
            if !scenario.ends_with("-missing") {
                for (path, contents) in configuration_files {
                    fs_err::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
                    fs_err::write(path, contents).unwrap();
                }
            }
            for path in private_files {
                fs_err::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
                fs_err::write(path, "private fixture\n").unwrap();
                fs_err::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
                assert_eq!(fs_err::read_to_string(path).unwrap(), "private fixture\n");
            }
        }
        let local_ssl_alias = scenario.contains("etc") && Path::new("/usr/local").is_dir();
        if local_ssl_alias {
            mount_namespace::mount_tmpfs(Path::new("/usr/local")).unwrap();
            std::os::unix::fs::symlink("/etc/ssl", "/usr/local/ssl").unwrap();
        }

        fs_err::create_dir_all("/sys/devices/system/cpu").unwrap();
        fs_err::write("/sys/devices/system/cpu/online", "0").unwrap();
        fs_err::create_dir_all("/sys/devices/pci0000:00/0000:00:01.0").unwrap();
        fs_err::write(
            "/sys/devices/pci0000:00/0000:00:01.0/resource",
            "0x1000 0x1fff 0x200\n",
        )
        .unwrap();
        fs_err::create_dir_all("/sys/bus/pci/devices").unwrap();
        std::os::unix::fs::symlink(
            "../../../devices/pci0000:00/0000:00:01.0",
            "/sys/bus/pci/devices/0000:00:01.0",
        )
        .unwrap();

        let mut profile = if scenario.starts_with("tpm-") {
            crate::profiles::tpm_worker()
        } else {
            crate::profiles::vm_worker()
        };
        if scenario == "devices" {
            let source = PathBuf::from(format!("/proc/self/fd/{}", null.as_raw_fd()));
            for path in [
                "/dev/sev",
                "/dev/vfio/vfio",
                "/dev/vfio/1",
                "/run/ordinary-device",
            ] {
                mount_namespace::bind_mount(&source, Path::new(path), 0).unwrap();
            }
            profile = crate::profiles::vm_worker()
                .read("/dev/sev")
                .read_write("/run/ordinary-device");
        }
        if scenario == "invalid-device" {
            profile = profile.device("/run/resolve");
        }
        let profile = profile.build();
        hardening::set_no_new_privs().unwrap();
        if scenario == "invalid-device" {
            assert!(matches!(
                apply_filesystem(&profile.inner.fs),
                Err(Error::ApplyFailed { source, .. })
                    if source.kind() == io::ErrorKind::InvalidInput
            ));
            return;
        }
        apply_filesystem(&profile.inner.fs).unwrap();
        apply_landlock(&profile.inner.fs).unwrap();
        drop_all_capabilities().unwrap();

        match scenario.as_str() {
            "etc" | "etc-missing" | "tpm-etc" | "tpm-etc-missing" => {
                if local_ssl_alias {
                    assert!(
                        fs_err::read("/usr/local/ssl/private/key.pem").is_err(),
                        "private keys should not be exposed through /usr/local/ssl",
                    );
                    let config = fs_err::read_to_string("/usr/local/ssl/openssl.cnf");
                    if scenario == "tpm-etc" {
                        assert_eq!(config.unwrap(), "# OpenSSL fixture\n");
                        assert!(fs_err::write("/usr/local/ssl/openssl.cnf", "modified").is_err());
                    } else {
                        assert!(config.is_err());
                    }
                }
                for (path, contents) in configuration_files {
                    let tpm_configuration = path.ends_with("/openssl.cnf");
                    let allowed = !scenario.ends_with("-missing")
                        && (path == "/etc/ld.so.cache"
                            || tpm_configuration == scenario.starts_with("tpm-"));
                    if allowed {
                        assert_eq!(fs_err::read_to_string(path).unwrap(), contents);
                        assert!(fs_err::write(path, "modified").is_err());
                    } else {
                        assert!(fs_err::read(path).is_err(), "{path} should not be exposed");
                    }
                }
                for path in private_files {
                    assert!(fs_err::read(path).is_err(), "{path} should not be exposed");
                }
                if scenario.starts_with("tpm-") {
                    assert!(fs_err::read("/etc/resolv.conf").is_err());
                } else {
                    assert_eq!(
                        fs_err::read_to_string("/etc/resolv.conf").unwrap(),
                        "nameserver 192.0.2.1\n",
                    );
                    if scenario == "etc" {
                        assert!(
                            ("sandbox.invalid", 0)
                                .to_socket_addrs()
                                .unwrap()
                                .any(|address| address.ip()
                                    == std::net::IpAddr::from([192, 0, 2, 2]))
                        );
                    }
                }
            }
            "resolver-missing" => {
                assert!(!Path::new("/etc/resolv.conf").exists());
            }
            scenario if scenario.starts_with("resolver") => {
                assert_eq!(
                    fs_err::read_to_string("/etc/resolv.conf").unwrap(),
                    "nameserver 192.0.2.1\n",
                );
                assert!(fs_err::write("/etc/resolv.conf", "modified").is_err());
                assert!(!Path::new("/run/resolve/resolv.conf").exists());
                assert!(!Path::new("/run/resolve/unrelated").exists());
            }
            "devices" => {
                for path in ["/dev/sev", "/dev/vfio/vfio"] {
                    let mut device = std::fs::File::options()
                        .read(true)
                        .write(true)
                        .open(path)
                        .unwrap();
                    io::Write::write_all(&mut device, b"probe").unwrap();
                    assert_eq!(ioctl_error(&device), baseline_ioctl);
                }
                assert!(!Path::new("/dev/vfio/1").exists());
                assert!(
                    std::fs::File::options()
                        .read(true)
                        .write(true)
                        .open("/run/ordinary-device")
                        .is_err(),
                );
            }
            "pci" => {
                let path = "/sys/bus/pci/devices/0000:00:01.0/resource";
                assert_eq!(
                    fs_err::read_to_string(path).unwrap(),
                    "0x1000 0x1fff 0x200\n"
                );
                assert!(fs_err::write(path, "modified").is_err());
            }
            _ => panic!("unknown resource scenario"),
        }
    }
}
