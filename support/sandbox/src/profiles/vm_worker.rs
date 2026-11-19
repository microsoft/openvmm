// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM VM worker policy.

use crate::Builder;
use crate::Network;
use crate::Profile;
use crate::Syscalls;

/// Build the OpenVMM VM worker policy.
///
/// Preserves the caller's network namespace and leaves `mbind` available for
/// host NUMA placement of guest RAM.
///
/// On Linux, grants individual loader and resolver/NSS configuration files,
/// rather than all of `/etc`. It also grants `/dev/sev` and `/dev/vfio/vfio`
/// for existing worker-side opens; other device handles should be supplied
/// explicitly. These device exceptions are not conditional on VM configuration.
///
/// Read-only `/sys/devices` and `/sys/bus/pci/devices` grants expose hardware
/// metadata beyond the assigned devices so PCI aliases and BAR metadata can
/// resolve. Optional configuration, device, and alias paths must exist when
/// the profile is built. Paths created later require rebuilding the profile.
pub fn vm_worker() -> Builder {
    let profile = Profile::deny_all()
        .name("vm_worker")
        .read("/sys/devices")
        .read_exec("/usr/lib")
        .syscalls(Syscalls::deny([
            "bpf",
            "chroot",
            "clone3",
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            "create_module",
            "delete_module",
            "execve",
            "finit_module",
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            "fork",
            "fsconfig",
            "fsmount",
            "fsopen",
            "fspick",
            "init_module",
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            "ioperm",
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            "iopl",
            "kexec_file_load",
            "kexec_load",
            "mount",
            "mount_setattr",
            "move_mount",
            "open_tree",
            "pivot_root",
            "process_vm_readv",
            "process_vm_writev",
            "ptrace",
            "reboot",
            "setns",
            "umount2",
            "unshare",
            "userfaultfd",
            "waitid",
            "acct",
            "add_key",
            "kcmp",
            "keyctl",
            "request_key",
        ]))
        .network(Network::Unrestricted);

    #[cfg(target_os = "linux")]
    let profile = {
        let mut profile = profile;
        for path in [
            "/etc/ld.so.cache",
            "/etc/hosts",
            "/etc/host.conf",
            "/etc/gai.conf",
            "/etc/nsswitch.conf",
            "/etc/resolv.conf",
        ] {
            if std::path::Path::new(path).exists() {
                profile = profile.read(path);
            }
        }
        if std::path::Path::new("/sys/bus/pci/devices").exists() {
            profile = profile.read("/sys/bus/pci/devices");
        }
        for path in ["/dev/sev", "/dev/vfio/vfio"] {
            if std::path::Path::new(path).exists() {
                profile = profile.device(path);
            }
        }
        profile
    };

    profile
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn syscall_names_resolve() {
        let profile = vm_worker().build();
        let Syscalls::Deny(names) = profile.inner.syscalls else {
            panic!("VM worker must use syscall filtering");
        };
        for name in names {
            assert!(
                crate::nr_for_name(&name).is_some(),
                "VM worker syscall does not resolve: {name}",
            );
        }
    }
}
