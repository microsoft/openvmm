// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux syscall denial policy metadata.

use crate::DeniedSyscall;
use crate::Enforcement;

const MANDATORY: Enforcement = Enforcement::Mandatory;
const OPTIONAL: Enforcement = Enforcement::Optional;

pub(crate) const SYSCALL_DENYLIST_MANDATORY: &[DeniedSyscall] = &[
    DeniedSyscall {
        name: "adjtimex",
        enforcement: MANDATORY,
        reason: Some("Changes the host kernel clock discipline."),
    },
    DeniedSyscall {
        name: "bpf",
        enforcement: MANDATORY,
        reason: Some(
            "Loads eBPF programs and maps, exposing a historically CVE-rich privilege-escalation and information-leak surface.",
        ),
    },
    DeniedSyscall {
        name: "capset",
        enforcement: MANDATORY,
        reason: Some("Changes process capability sets after the sandbox capability drop."),
    },
    DeniedSyscall {
        name: "chroot",
        enforcement: MANDATORY,
        reason: Some("Facilitates sandbox escape."),
    },
    DeniedSyscall {
        name: "clone3",
        enforcement: MANDATORY,
        reason: Some(
            "Can hide CLONE_NEW namespace flags behind a pointer that seccomp cannot inspect.",
        ),
    },
    DeniedSyscall {
        name: "clock_adjtime",
        enforcement: MANDATORY,
        reason: Some("Adjusts the host kernel clock."),
    },
    DeniedSyscall {
        name: "clock_settime",
        enforcement: MANDATORY,
        reason: Some("Sets the host kernel clock."),
    },
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    DeniedSyscall {
        name: "create_module",
        enforcement: MANDATORY,
        reason: Some("Creates kernel module memory, enabling arbitrary kernel-code execution."),
    },
    DeniedSyscall {
        name: "delete_module",
        enforcement: MANDATORY,
        reason: Some("Can remove security modules or trigger module-unload races."),
    },
    DeniedSyscall {
        name: "execve",
        enforcement: MANDATORY,
        reason: None,
    },
    DeniedSyscall {
        name: "fanotify_init",
        enforcement: MANDATORY,
        reason: Some("Creates privileged filesystem-monitoring groups."),
    },
    DeniedSyscall {
        name: "finit_module",
        enforcement: MANDATORY,
        reason: Some("Loads a kernel module, enabling arbitrary kernel-code execution."),
    },
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    DeniedSyscall {
        name: "fork",
        enforcement: MANDATORY,
        reason: None,
    },
    DeniedSyscall {
        name: "fsconfig",
        enforcement: MANDATORY,
        reason: Some(
            "Configures new mount API filesystem contexts with power equivalent to mount.",
        ),
    },
    DeniedSyscall {
        name: "fsmount",
        enforcement: MANDATORY,
        reason: Some("Creates mounts through the new mount API, bypassing a mount-only denial."),
    },
    DeniedSyscall {
        name: "fsopen",
        enforcement: MANDATORY,
        reason: Some(
            "Creates filesystem contexts for the new mount API, bypassing a mount-only denial.",
        ),
    },
    DeniedSyscall {
        name: "fspick",
        enforcement: MANDATORY,
        reason: Some("Selects existing mounts for reconfiguration through the new mount API."),
    },
    DeniedSyscall {
        name: "init_module",
        enforcement: MANDATORY,
        reason: Some("Loads a kernel module, enabling arbitrary kernel-code execution."),
    },
    DeniedSyscall {
        name: "io_uring_enter",
        enforcement: MANDATORY,
        reason: Some(
            "Executes io_uring operations that can bypass syscall-level file and network filtering.",
        ),
    },
    DeniedSyscall {
        name: "io_uring_register",
        enforcement: MANDATORY,
        reason: Some(
            "Registers io_uring resources through a young, historically CVE-rich kernel subsystem.",
        ),
    },
    DeniedSyscall {
        name: "io_uring_setup",
        enforcement: MANDATORY,
        reason: Some(
            "Creates io_uring instances that expose a young, historically CVE-rich kernel subsystem.",
        ),
    },
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    DeniedSyscall {
        name: "ioperm",
        enforcement: MANDATORY,
        reason: Some(
            "Grants direct x86 I/O-port access that bypasses normal kernel device mediation.",
        ),
    },
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    DeniedSyscall {
        name: "iopl",
        enforcement: MANDATORY,
        reason: Some(
            "Raises x86 I/O privilege, allowing direct hardware access outside normal kernel mediation.",
        ),
    },
    DeniedSyscall {
        name: "ioprio_set",
        enforcement: MANDATORY,
        reason: Some("Changes process I/O scheduling policy."),
    },
    DeniedSyscall {
        name: "kexec_file_load",
        enforcement: MANDATORY,
        reason: Some("Loads a replacement kernel image, enabling complete host takeover."),
    },
    DeniedSyscall {
        name: "kexec_load",
        enforcement: MANDATORY,
        reason: Some("Loads a replacement kernel image, enabling complete host takeover."),
    },
    DeniedSyscall {
        name: "mbind",
        enforcement: MANDATORY,
        reason: Some("Changes memory placement policy across NUMA nodes."),
    },
    DeniedSyscall {
        name: "migrate_pages",
        enforcement: MANDATORY,
        reason: Some("Moves another process's pages across NUMA nodes."),
    },
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    DeniedSyscall {
        name: "modify_ldt",
        enforcement: MANDATORY,
        reason: Some("Exposes the historically vulnerable x86 local descriptor table."),
    },
    DeniedSyscall {
        name: "mount",
        enforcement: MANDATORY,
        reason: Some(
            "Can remount host filesystems or strip protection flags, defeating filesystem confinement.",
        ),
    },
    DeniedSyscall {
        name: "mount_setattr",
        enforcement: MANDATORY,
        reason: Some("Can clear read-only, nosuid, or nodev attributes from sandbox mounts."),
    },
    DeniedSyscall {
        name: "move_mount",
        enforcement: MANDATORY,
        reason: Some(
            "Relocates mounts through the new mount API and exposes the same escape surface as mount.",
        ),
    },
    DeniedSyscall {
        name: "move_pages",
        enforcement: MANDATORY,
        reason: Some("Moves or queries another process's pages across NUMA nodes."),
    },
    DeniedSyscall {
        name: "name_to_handle_at",
        enforcement: MANDATORY,
        reason: Some("Creates filesystem handles that can bypass path-based confinement."),
    },
    DeniedSyscall {
        name: "open_by_handle_at",
        enforcement: MANDATORY,
        reason: Some("Opens filesystem objects by handle, bypassing path-based confinement."),
    },
    DeniedSyscall {
        name: "open_tree",
        enforcement: MANDATORY,
        reason: Some(
            "Clones mount trees into file descriptors that can be detached and reattached.",
        ),
    },
    DeniedSyscall {
        name: "perf_event_open",
        enforcement: MANDATORY,
        reason: Some("Exposes a historically CVE-rich kernel performance-monitoring surface."),
    },
    DeniedSyscall {
        name: "personality",
        enforcement: MANDATORY,
        reason: Some("Changes process execution-domain behavior."),
    },
    DeniedSyscall {
        name: "pidfd_getfd",
        enforcement: MANDATORY,
        reason: Some("Duplicates file descriptors from another process."),
    },
    DeniedSyscall {
        name: "pivot_root",
        enforcement: MANDATORY,
        reason: Some(
            "Repoints the process root filesystem and can undo the sandbox root boundary.",
        ),
    },
    DeniedSyscall {
        name: "process_vm_readv",
        enforcement: MANDATORY,
        reason: Some("Reads another process's address space directly, enabling secret theft."),
    },
    DeniedSyscall {
        name: "process_vm_writev",
        enforcement: MANDATORY,
        reason: Some("Writes another process's address space directly, enabling code injection."),
    },
    DeniedSyscall {
        name: "ptrace",
        enforcement: MANDATORY,
        reason: Some("Controls other processes and can read or modify their memory and registers."),
    },
    DeniedSyscall {
        name: "quotactl",
        enforcement: MANDATORY,
        reason: Some("Manipulates filesystem quota configuration."),
    },
    DeniedSyscall {
        name: "reboot",
        enforcement: MANDATORY,
        reason: Some("Can reboot or halt the host, causing whole-machine denial of service."),
    },
    DeniedSyscall {
        name: "setns",
        enforcement: MANDATORY,
        reason: Some(
            "Joins an existing namespace and could re-enter host mount, network, or PID namespaces.",
        ),
    },
    DeniedSyscall {
        name: "set_mempolicy",
        enforcement: MANDATORY,
        reason: Some("Changes process memory placement policy across NUMA nodes."),
    },
    DeniedSyscall {
        name: "setdomainname",
        enforcement: MANDATORY,
        reason: Some("Changes the UTS namespace domain name."),
    },
    DeniedSyscall {
        name: "sethostname",
        enforcement: MANDATORY,
        reason: Some("Changes the UTS namespace host name."),
    },
    DeniedSyscall {
        name: "settimeofday",
        enforcement: MANDATORY,
        reason: Some("Sets the host kernel clock."),
    },
    DeniedSyscall {
        name: "swapoff",
        enforcement: MANDATORY,
        reason: Some("Disables host swap areas."),
    },
    DeniedSyscall {
        name: "swapon",
        enforcement: MANDATORY,
        reason: Some("Enables files or devices as host swap areas."),
    },
    DeniedSyscall {
        name: "sysfs",
        enforcement: MANDATORY,
        reason: Some("Accesses the obsolete kernel filesystem-type interface."),
    },
    DeniedSyscall {
        name: "syslog",
        enforcement: MANDATORY,
        reason: Some("Reads or controls the kernel log."),
    },
    DeniedSyscall {
        name: "umount2",
        enforcement: MANDATORY,
        reason: Some("Can detach protective mounts and expose underlying host filesystem content."),
    },
    DeniedSyscall {
        name: "unshare",
        enforcement: MANDATORY,
        reason: Some(
            "Creates new namespaces, including user namespaces that grant capabilities within the new namespace.",
        ),
    },
    DeniedSyscall {
        name: "userfaultfd",
        enforcement: MANDATORY,
        reason: Some(
            "Provides userspace page-fault handling used to widen race windows and groom kernel memory.",
        ),
    },
    DeniedSyscall {
        name: "vhangup",
        enforcement: MANDATORY,
        reason: Some("Revokes access to the controlling terminal."),
    },
    DeniedSyscall {
        name: "vmsplice",
        enforcement: MANDATORY,
        reason: Some("Exposes a historically vulnerable zero-copy pipe interface."),
    },
    DeniedSyscall {
        name: "waitid",
        enforcement: MANDATORY,
        reason: None,
    },
];

pub(crate) const SYSCALL_DENYLIST_OPTIONAL: &[DeniedSyscall] = &[
    DeniedSyscall {
        name: "acct",
        enforcement: OPTIONAL,
        reason: Some("Requires CAP_SYS_PACCT."),
    },
    DeniedSyscall {
        name: "add_key",
        enforcement: OPTIONAL,
        reason: Some("Accepts attacker-controlled variable-size payloads."),
    },
    DeniedSyscall {
        name: "kcmp",
        enforcement: OPTIONAL,
        reason: Some(
            "Compares kernel resources across processes and can leak information about shared resources.",
        ),
    },
    DeniedSyscall {
        name: "keyctl",
        enforcement: OPTIONAL,
        reason: Some("Accepts attacker-controlled variable-size payloads."),
    },
    DeniedSyscall {
        name: "request_key",
        enforcement: OPTIONAL,
        reason: Some(
            "Accesses kernel keyring management, a historically UAF- and refcount-bug-prone surface.",
        ),
    },
];
