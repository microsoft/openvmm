// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM TPM worker policy.

use crate::Builder;
use crate::Network;
use crate::Profile;
use crate::Syscalls;

/// Build the OpenVMM TPM worker policy with an isolated network namespace.
///
/// Includes individual loader and OpenSSL configuration files only when
/// present, without exposing their parent directories or private-key trees.
/// Nonstandard OpenSSL paths and additional configuration includes require
/// explicit grants.
pub fn tpm_worker() -> Builder {
    let mut profile = Profile::deny_all().name("tpm_worker").read_exec("/usr/lib");

    for path in [
        "/etc/ld.so.cache",
        "/etc/ssl/openssl.cnf",
        "/etc/pki/tls/openssl.cnf",
        "/usr/local/ssl/openssl.cnf",
    ] {
        if std::path::Path::new(path).exists() {
            profile = profile.read(path);
        }
    }

    profile
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
            "mbind",
            "request_key",
        ]))
        .network(Network::None)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn syscall_names_resolve() {
        let profile = tpm_worker().build();
        let Syscalls::Deny(names) = profile.inner.syscalls else {
            panic!("TPM worker must use syscall filtering");
        };
        for name in names {
            assert!(
                crate::nr_for_name(&name).is_some(),
                "TPM worker syscall does not resolve: {name}",
            );
        }
    }
}
