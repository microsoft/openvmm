// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use seccompiler::BpfProgram;
use seccompiler::SeccompAction;
use seccompiler::SeccompCmpArgLen;
use seccompiler::SeccompCmpOp;
use seccompiler::SeccompCondition;
use seccompiler::SeccompFilter;
use seccompiler::SeccompRule;
use seccompiler::TargetArch;
use std::collections::BTreeMap;
use std::io;

// ---- Local constants (not exposed by `libc`) ----

/// `TIOCLINUX` ioctl request code. See `include/uapi/linux/tiocl.h`.
const TIOCLINUX: u64 = 0x541C;

/// `PR_SET_MM` prctl operation. See `include/uapi/linux/prctl.h`.
const PR_SET_MM: u64 = 35;

/// All `CLONE_NEW*` namespace-creation bits OR'd together. Matches
/// Docker's default profile mask (`2114060288` = `0x7E020000`).
///
/// Individual bits:
/// - `CLONE_NEWNS`     = 0x0002_0000 (mount)
/// - `CLONE_NEWCGROUP` = 0x0200_0000
/// - `CLONE_NEWUTS`    = 0x0400_0000
/// - `CLONE_NEWIPC`    = 0x0800_0000
/// - `CLONE_NEWUSER`   = 0x1000_0000
/// - `CLONE_NEWPID`    = 0x2000_0000
/// - `CLONE_NEWNET`    = 0x4000_0000
const CLONE_NEW_MASK: u64 = 0x7E02_0000;

/// The individual `CLONE_NEW*` namespace-creation bits, OR-folding to
/// [`CLONE_NEW_MASK`]. The denylist filter can't express "any bit in a mask is
/// set" with a single masked-equality condition, so it denies `clone` with one
/// rule per bit (any match ⇒ deny). See [`clone_deny_rules`].
const CLONE_NEW_BITS: [u64; 7] = [
    0x0002_0000, // CLONE_NEWNS (mount)
    0x0200_0000, // CLONE_NEWCGROUP
    0x0400_0000, // CLONE_NEWUTS
    0x0800_0000, // CLONE_NEWIPC
    0x1000_0000, // CLONE_NEWUSER
    0x2000_0000, // CLONE_NEWPID
    0x4000_0000, // CLONE_NEWNET
];

/// Compile-time proof that the per-bit list denied by [`clone_deny_rules`]
/// covers exactly [`CLONE_NEW_MASK`], so the two representations can't drift.
const _: () = {
    let mut folded = 0u64;
    let mut i = 0;
    while i < CLONE_NEW_BITS.len() {
        folded |= CLONE_NEW_BITS[i];
        i += 1;
    }
    assert!(folded == CLONE_NEW_MASK);
};

/// Resolve the mandatory syscall policy into architecture-native numbers.
///
/// `clone3` is mandatory but deliberately omitted here because
/// [`build_clone3_enosys_filter`] must return `ENOSYS` rather than the normal
/// deny action so libc can fall back to the argument-filtered `clone`.
pub fn mandatory_deny_syscall_nrs() -> io::Result<Vec<i64>> {
    crate::platform_syscall_denylist()
        .filter(|entry| entry.enforcement == crate::Enforcement::Mandatory)
        .filter(|entry| entry.name != "clone3")
        .map(|entry| {
            nr_for_name(entry.name).ok_or_else(|| {
                io::Error::other(format!(
                    "mandatory syscall policy does not resolve on this architecture: {:?}",
                    entry.name
                ))
            })
        })
        .collect()
}

/// Build the `clone3`-to-`ENOSYS` overlay filter.
///
/// `clone3` takes a pointer to `struct clone_args`, so its flags can't
/// be inspected by seccomp at the argument level. Returning `ENOSYS`
/// (rather than the default `EPERM`) lets glibc/musl's `pthread_create`
/// treat `clone3` as unsupported and fall back to `clone`, which the
/// main baseline filter validates against `CLONE_NEW*` bits. This
/// mirrors Docker's default profile.
///
/// The filter's `mismatch_action` is `Allow` so it doesn't affect any
/// syscall other than `clone3`; the main baseline filter's decisions
/// stand for everything else, and Linux seccomp uses the most
/// restrictive action across all installed filters.
pub fn build_clone3_enosys_filter() -> io::Result<BpfProgram> {
    let arch = target_arch()?;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    // Match any invocation of clone3 — use MaskedEq(0) which reduces
    // to `(arg0 & 0) == 0` and is therefore always true.
    let always_match = SeccompRule::new(vec![
        SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::MaskedEq(0), 0)
            .map_err(|e| io::Error::other(format!("seccomp condition: {e}")))?,
    ])
    .map_err(|e| io::Error::other(format!("seccomp rule: {e}")))?;
    rules.insert(libc::SYS_clone3, vec![always_match]);

    finalize_filter(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
}

/// Apply a pre-built BPF filter to the current thread.
///
/// Requires `PR_SET_NO_NEW_PRIVS` to be set on the calling thread when
/// the process lacks `CAP_SYS_ADMIN`.
pub fn apply_filter(bpf: &BpfProgram) -> io::Result<()> {
    seccompiler::apply_filter(bpf)
        .map_err(|e| io::Error::other(format!("seccomp apply failed: {e}")))
}

fn target_arch() -> io::Result<TargetArch> {
    std::env::consts::ARCH
        .try_into()
        .map_err(|e| io::Error::other(format!("unsupported arch for seccomp: {e:?}")))
}

/// The minimal syscalls a process needs to unwind and exit. In the denylist
/// model these are always allowed (they are never on the deny list); this set
/// exists so [`apply_denylist`] can reject any attempt to *additionally* deny
/// one of them, which would trap the worker with no way to exit.
fn survival_syscall_nrs() -> [i64; 4] {
    [
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_rt_sigreturn,
        libc::SYS_restart_syscall,
    ]
}

/// Resolve a syscall *name* — as written in a profile's
/// [`Syscalls::Deny`](crate::Syscalls::Deny) list — to its
/// architecture-native syscall number.
///
/// Returns `None` for any name this crate does not recognize. Callers **must**
/// treat an unknown name as a hard error rather than silently dropping it: a
/// dropped deny entry makes the installed filter *less* restrictive than the
/// author intended, silently leaving a syscall reachable (R-S12, fail-loud).
///
/// The recognized set is intentionally limited to syscalls that exist on every
/// Linux architecture this project targets, expressed in their modern spelling
/// (`openat` not `open`, `ppoll` not `poll`, `dup3` not `dup2`, `newfstatat`
/// not `stat`). Legacy aliases can be added behind an architecture `cfg` when a
/// worker genuinely needs them.
#[allow(deprecated)]
pub fn nr_for_name(name: &str) -> Option<i64> {
    let nr = match name {
        // Process lifecycle
        "exit" => libc::SYS_exit,
        "exit_group" => libc::SYS_exit_group,
        "rt_sigreturn" => libc::SYS_rt_sigreturn,
        "rt_sigaction" => libc::SYS_rt_sigaction,
        "rt_sigprocmask" => libc::SYS_rt_sigprocmask,
        "restart_syscall" => libc::SYS_restart_syscall,
        "sigaltstack" => libc::SYS_sigaltstack,
        "execve" => libc::SYS_execve,
        "wait4" => libc::SYS_wait4,
        "kill" => libc::SYS_kill,
        "tkill" => libc::SYS_tkill,
        "tgkill" => libc::SYS_tgkill,
        // Memory management
        "brk" => libc::SYS_brk,
        "mmap" => libc::SYS_mmap,
        "munmap" => libc::SYS_munmap,
        "mprotect" => libc::SYS_mprotect,
        "madvise" => libc::SYS_madvise,
        "mremap" => libc::SYS_mremap,
        // File I/O
        "read" => libc::SYS_read,
        "readv" => libc::SYS_readv,
        "pread64" => libc::SYS_pread64,
        "write" => libc::SYS_write,
        "writev" => libc::SYS_writev,
        "pwrite64" => libc::SYS_pwrite64,
        "close" => libc::SYS_close,
        "openat" => libc::SYS_openat,
        "fstat" => libc::SYS_fstat,
        "newfstatat" => libc::SYS_newfstatat,
        "statx" => libc::SYS_statx,
        "lseek" => libc::SYS_lseek,
        "ioctl" => libc::SYS_ioctl,
        "fcntl" => libc::SYS_fcntl,
        "dup" => libc::SYS_dup,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        "dup2" => libc::SYS_dup2,
        "dup3" => libc::SYS_dup3,
        "pipe2" => libc::SYS_pipe2,
        "mkdirat" => libc::SYS_mkdirat,
        "unlinkat" => libc::SYS_unlinkat,
        "renameat2" => libc::SYS_renameat2,
        // Event polling
        "epoll_create1" => libc::SYS_epoll_create1,
        "epoll_ctl" => libc::SYS_epoll_ctl,
        "epoll_pwait" => libc::SYS_epoll_pwait,
        "ppoll" => libc::SYS_ppoll,
        "pselect6" => libc::SYS_pselect6,
        // Directory / metadata
        "getdents64" => libc::SYS_getdents64,
        "getcwd" => libc::SYS_getcwd,
        "readlinkat" => libc::SYS_readlinkat,
        "faccessat" => libc::SYS_faccessat,
        "faccessat2" => libc::SYS_faccessat2,
        // Futex / threading
        "futex" => libc::SYS_futex,
        "set_robust_list" => libc::SYS_set_robust_list,
        "get_robust_list" => libc::SYS_get_robust_list,
        "clone" => libc::SYS_clone,
        "sched_yield" => libc::SYS_sched_yield,
        "sched_getaffinity" => libc::SYS_sched_getaffinity,
        "set_tid_address" => libc::SYS_set_tid_address,
        "rseq" => libc::SYS_rseq,
        // Time
        "clock_gettime" => libc::SYS_clock_gettime,
        "clock_getres" => libc::SYS_clock_getres,
        "clock_nanosleep" => libc::SYS_clock_nanosleep,
        "nanosleep" => libc::SYS_nanosleep,
        "gettimeofday" => libc::SYS_gettimeofday,
        // Identity (read-only)
        "getuid" => libc::SYS_getuid,
        "geteuid" => libc::SYS_geteuid,
        "getgid" => libc::SYS_getgid,
        "getegid" => libc::SYS_getegid,
        "getpid" => libc::SYS_getpid,
        "gettid" => libc::SYS_gettid,
        "getppid" => libc::SYS_getppid,
        // Misc
        "getrandom" => libc::SYS_getrandom,
        "prctl" => libc::SYS_prctl,
        // Network
        "socket" => libc::SYS_socket,
        "socketpair" => libc::SYS_socketpair,
        "connect" => libc::SYS_connect,
        "bind" => libc::SYS_bind,
        "listen" => libc::SYS_listen,
        "accept4" => libc::SYS_accept4,
        "getsockname" => libc::SYS_getsockname,
        "getpeername" => libc::SYS_getpeername,
        "sendto" => libc::SYS_sendto,
        "recvfrom" => libc::SYS_recvfrom,
        "sendmsg" => libc::SYS_sendmsg,
        "recvmsg" => libc::SYS_recvmsg,
        "shutdown" => libc::SYS_shutdown,
        "setsockopt" => libc::SYS_setsockopt,
        "getsockopt" => libc::SYS_getsockopt,
        // Configurable sandbox denylist
        "acct" => libc::SYS_acct,
        "add_key" => libc::SYS_add_key,
        "adjtimex" => libc::SYS_adjtimex,
        "bpf" => libc::SYS_bpf,
        "capset" => libc::SYS_capset,
        "chroot" => libc::SYS_chroot,
        "clone3" => libc::SYS_clone3,
        "clock_adjtime" => libc::SYS_clock_adjtime,
        "clock_settime" => libc::SYS_clock_settime,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        "create_module" => libc::SYS_create_module,
        "delete_module" => libc::SYS_delete_module,
        "fanotify_init" => libc::SYS_fanotify_init,
        "finit_module" => libc::SYS_finit_module,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        "fork" => libc::SYS_fork,
        "fsconfig" => libc::SYS_fsconfig,
        "fsmount" => libc::SYS_fsmount,
        "fsopen" => libc::SYS_fsopen,
        "fspick" => libc::SYS_fspick,
        "init_module" => libc::SYS_init_module,
        "io_uring_enter" => libc::SYS_io_uring_enter,
        "io_uring_register" => libc::SYS_io_uring_register,
        "io_uring_setup" => libc::SYS_io_uring_setup,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        "ioperm" => libc::SYS_ioperm,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        "iopl" => libc::SYS_iopl,
        "ioprio_set" => libc::SYS_ioprio_set,
        "kcmp" => libc::SYS_kcmp,
        "kexec_file_load" => libc::SYS_kexec_file_load,
        "kexec_load" => libc::SYS_kexec_load,
        "keyctl" => libc::SYS_keyctl,
        "mbind" => libc::SYS_mbind,
        "migrate_pages" => libc::SYS_migrate_pages,
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        "modify_ldt" => libc::SYS_modify_ldt,
        "mount" => libc::SYS_mount,
        "mount_setattr" => libc::SYS_mount_setattr,
        "move_mount" => libc::SYS_move_mount,
        "move_pages" => libc::SYS_move_pages,
        "name_to_handle_at" => libc::SYS_name_to_handle_at,
        "open_by_handle_at" => libc::SYS_open_by_handle_at,
        "open_tree" => libc::SYS_open_tree,
        "perf_event_open" => libc::SYS_perf_event_open,
        "personality" => libc::SYS_personality,
        "pidfd_getfd" => libc::SYS_pidfd_getfd,
        "pivot_root" => libc::SYS_pivot_root,
        "process_vm_readv" => libc::SYS_process_vm_readv,
        "process_vm_writev" => libc::SYS_process_vm_writev,
        "ptrace" => libc::SYS_ptrace,
        "quotactl" => libc::SYS_quotactl,
        "reboot" => libc::SYS_reboot,
        "request_key" => libc::SYS_request_key,
        "set_mempolicy" => libc::SYS_set_mempolicy,
        "setdomainname" => libc::SYS_setdomainname,
        "sethostname" => libc::SYS_sethostname,
        "setns" => libc::SYS_setns,
        "settimeofday" => libc::SYS_settimeofday,
        "swapoff" => libc::SYS_swapoff,
        "swapon" => libc::SYS_swapon,
        "sysfs" => libc::SYS_sysfs,
        "syslog" => libc::SYS_syslog,
        "umount2" => libc::SYS_umount2,
        "unshare" => libc::SYS_unshare,
        "userfaultfd" => libc::SYS_userfaultfd,
        "vhangup" => libc::SYS_vhangup,
        "vmsplice" => libc::SYS_vmsplice,
        "waitid" => libc::SYS_waitid,
        // Architecture-specific: `arch_prctl` is x86-only.
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        "arch_prctl" => libc::SYS_arch_prctl,
        _ => return None,
    };
    Some(nr)
}

/// Build a default-allow denylist filter that blocks the
/// [mandatory-deny list](mandatory_deny_syscall_nrs) plus `extra_denied_nrs`
/// with `deny_action`, and installs the argument-level guards on `ioctl`,
/// `clone`, `prctl`, and `socket`.
///
/// The filter's `mismatch_action` is `Allow`: any syscall not named here (and
/// not caught by an argument-level guard) passes through. The complementary
/// `clone3`-to-`ENOSYS` overlay is installed separately by [`apply_denylist`].
pub fn build_denylist_filter(
    extra_denied_nrs: &[i64],
    deny_action: SeccompAction,
) -> io::Result<BpfProgram> {
    let arch = target_arch()?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // Argument-level guards first: block only the dangerous cases and leave the
    // rest of each syscall allowed by the default `mismatch_action`.
    rules.insert(libc::SYS_ioctl, ioctl_deny_rules()?);
    rules.insert(libc::SYS_clone, clone_deny_rules()?);
    rules.insert(libc::SYS_prctl, prctl_deny_rules()?);
    rules.insert(libc::SYS_socket, socket_deny_rules()?);

    // Blanket-deny the baseline set. It never overlaps the argument-guarded
    // syscalls above, so `or_default` leaves their rules intact.
    for nr in mandatory_deny_syscall_nrs()? {
        rules.entry(nr).or_default();
    }
    // Profile-specified extras are unconditional denies. An extra that names an
    // argument-guarded syscall (e.g. `socket`) overrides its guard with a full
    // deny — stricter, which is exactly what an explicit request means.
    for nr in extra_denied_nrs.iter().copied() {
        rules.insert(nr, Vec::new());
    }

    finalize_filter(rules, SeccompAction::Allow, deny_action, arch)
}

/// Resolve `extra_deny_names` into syscall numbers, build the denylist filter
/// (baseline + extras + argument-level guards), and apply it to the current
/// thread along with the `clone3`-to-`ENOSYS` overlay.
///
/// Every name must resolve via [`nr_for_name`]; an unknown name is a hard
/// error (R-S12 fail-loud) rather than a silent omission. A name on the
/// [survival set](survival_syscall_nrs) is rejected, since blanket-denying a
/// syscall the process needs to unwind and exit would trap it.
///
/// Because seccomp filters stack and the kernel takes the most restrictive
/// verdict, this can be called both from `apply` (the profile's opt-in filter)
/// and from `tighten` (an additional stacked filter that denies still more).
pub fn apply_denylist(extra_deny_names: &[&str], deny_action: SeccompAction) -> io::Result<()> {
    let survival = survival_syscall_nrs();
    let mut extra = Vec::with_capacity(extra_deny_names.len());
    for name in extra_deny_names {
        // clone3 is always handled by the ENOSYS overlay below. Adding it to
        // the normal filter would prevent libc's fallback to clone.
        if *name == "clone3" {
            continue;
        }
        let nr = nr_for_name(name).ok_or_else(|| {
            io::Error::other(format!("unknown syscall name in denylist: {name:?}"))
        })?;
        if survival.contains(&nr) {
            return Err(io::Error::other(format!(
                "syscall {name:?} is required for a process to exit and cannot be denied"
            )));
        }
        extra.push(nr);
    }

    let filter = build_denylist_filter(&extra, deny_action)?;
    apply_filter(&filter)?;
    // Force clone3 to ENOSYS so the libc thread path falls back to the
    // argument-filtered `clone`; clone3's flags live behind a pointer seccomp
    // can't inspect.
    let clone3 = build_clone3_enosys_filter()?;
    apply_filter(&clone3)
}

fn finalize_filter(
    rules: BTreeMap<i64, Vec<SeccompRule>>,
    mismatch_action: SeccompAction,
    match_action: SeccompAction,
    arch: TargetArch,
) -> io::Result<BpfProgram> {
    let filter = SeccompFilter::new(rules, mismatch_action, match_action, arch)
        .map_err(|e| io::Error::other(format!("seccomp filter creation failed: {e}")))?;
    filter
        .try_into()
        .map_err(|e| io::Error::other(format!("seccomp BPF compilation failed: {e}")))
}

// ---- Per-syscall argument rule builders ----

/// `ioctl` — deny the `TIOCSTI` and `TIOCLINUX` request codes; every other
/// request code falls through to the default allow.
///
/// Uses 32-bit (`Dword`) comparison so an attacker can't bypass by setting the
/// upper 32 bits of arg1 (the kernel ignores them for ioctl request codes).
/// The two codes are emitted as separate rules (rules are OR'd, so either one
/// matching denies).
fn ioctl_deny_rules() -> io::Result<Vec<SeccompRule>> {
    Ok(vec![
        SeccompRule::new(vec![
            SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, libc::TIOCSTI as _)
                .map_err(|e| io::Error::other(format!("seccomp condition: {e}")))?,
        ])
        .map_err(|e| io::Error::other(format!("seccomp rule: {e}")))?,
        SeccompRule::new(vec![
            SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, TIOCLINUX)
                .map_err(|e| io::Error::other(format!("seccomp condition: {e}")))?,
        ])
        .map_err(|e| io::Error::other(format!("seccomp rule: {e}")))?,
    ])
}

/// `clone` — deny when any `CLONE_NEW*` namespace bit is set in arg0; a plain
/// `clone` for threads (no such bit) falls through to the default allow.
///
/// seccomp can't express "any bit in a mask is set" in one condition, so this
/// emits one rule per bit in [`CLONE_NEW_BITS`] (rules are OR'd, so any match
/// denies). Matches the effect of Docker's default profile filter.
fn clone_deny_rules() -> io::Result<Vec<SeccompRule>> {
    let mut rules = Vec::with_capacity(CLONE_NEW_BITS.len());
    for bit in CLONE_NEW_BITS {
        rules.push(
            SeccompRule::new(vec![
                SeccompCondition::new(0, SeccompCmpArgLen::Qword, SeccompCmpOp::MaskedEq(bit), bit)
                    .map_err(|e| io::Error::other(format!("seccomp condition: {e}")))?,
            ])
            .map_err(|e| io::Error::other(format!("seccomp rule: {e}")))?,
        );
    }
    Ok(rules)
}

/// `prctl` — deny the `PR_SET_MM` subcommand; every other subcommand falls
/// through to the default allow.
fn prctl_deny_rules() -> io::Result<Vec<SeccompRule>> {
    Ok(vec![
        SeccompRule::new(vec![
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, PR_SET_MM)
                .map_err(|e| io::Error::other(format!("seccomp condition: {e}")))?,
        ])
        .map_err(|e| io::Error::other(format!("seccomp rule: {e}")))?,
    ])
}

/// `socket` — deny the `AF_PACKET` address family; every other family falls
/// through to the default allow. Stricter than Docker's default; motivated by
/// CVE-2017-7308.
fn socket_deny_rules() -> io::Result<Vec<SeccompRule>> {
    Ok(vec![
        SeccompRule::new(vec![
            SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_PACKET as u64,
            )
            .map_err(|e| io::Error::other(format!("seccomp condition: {e}")))?,
        ])
        .map_err(|e| io::Error::other(format!("seccomp rule: {e}")))?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn mandatory_deny_disjoint_from_survival() {
        // The baseline denylist must be non-empty and must never contain a
        // survival syscall — `apply_denylist` relies on that invariant so a
        // profile can't blanket-deny a syscall the worker needs to exit.
        let denied = mandatory_deny_syscall_nrs().expect("mandatory policy must resolve");
        assert!(!denied.is_empty());
        for nr in survival_syscall_nrs() {
            assert!(
                !denied.contains(&nr),
                "survival syscall {nr} must not be on the mandatory-deny list",
            );
        }
    }

    #[test]
    fn clone3_enosys_filter_builds() {
        let bpf = build_clone3_enosys_filter().expect("should build clone3 ENOSYS filter");
        assert!(!bpf.is_empty());
    }

    #[test]
    fn configured_syscall_policy_is_unique_and_resolvable() {
        let mut names = std::collections::BTreeSet::new();
        for entry in crate::platform_syscall_denylist() {
            assert!(
                names.insert(entry.name),
                "duplicate policy for {}",
                entry.name
            );
            assert!(
                nr_for_name(entry.name).is_some(),
                "policy syscall does not resolve: {}",
                entry.name
            );
        }
    }

    #[test]
    fn optional_syscalls_are_not_in_mandatory_baseline() {
        let mandatory = mandatory_deny_syscall_nrs().expect("mandatory policy must resolve");
        for entry in crate::platform_syscall_denylist()
            .filter(|entry| entry.enforcement == crate::Enforcement::Optional)
        {
            let nr = nr_for_name(entry.name).expect("optional policy must resolve");
            assert!(
                !mandatory.contains(&nr),
                "optional syscall is mandatory: {}",
                entry.name
            );
        }
    }

    #[test]
    fn argument_filter_rules_build() {
        // Confirms all per-syscall arg-filter helpers produce valid
        // conditions. Runtime enforcement is exercised by higher-level
        // sandbox integration tests in a subprocess.
        ioctl_deny_rules().expect("ioctl rules");
        clone_deny_rules().expect("clone rules");
        prctl_deny_rules().expect("prctl rules");
        socket_deny_rules().expect("socket rules");
    }

    #[test]
    fn nr_for_name_resolves_known_and_rejects_unknown() {
        assert_eq!(nr_for_name("read"), Some(libc::SYS_read));
        assert_eq!(nr_for_name("openat"), Some(libc::SYS_openat));
        assert_eq!(nr_for_name("kill"), Some(libc::SYS_kill));
        assert_eq!(nr_for_name("tkill"), Some(libc::SYS_tkill));
        assert_eq!(nr_for_name("tgkill"), Some(libc::SYS_tgkill));
        assert_eq!(nr_for_name("exit_group"), Some(libc::SYS_exit_group));
        assert_eq!(nr_for_name("definitely_not_a_syscall"), None);
        // Legacy spellings are intentionally not recognized.
        assert_eq!(nr_for_name("open"), None);
        assert_eq!(nr_for_name("poll"), None);
    }

    #[test]
    fn denylist_filter_builds_with_extras() {
        let nrs = &[libc::SYS_execve, libc::SYS_connect];
        let bpf = build_denylist_filter(nrs, SeccompAction::Errno(libc::EPERM as u32))
            .expect("should build denylist filter");
        assert!(!bpf.is_empty());
    }
}
