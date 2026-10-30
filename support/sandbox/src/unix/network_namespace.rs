// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Network namespaces (`CLONE_NEWNET`).

use crate::unix::SyscallResult;
use std::io;

/// Bring the loopback interface (`lo`) up in the current network
/// namespace.
///
/// A fresh network namespace has `lo` present but administratively down,
/// so loopback traffic (127.0.0.0/8, ::1) is not deliverable. This
/// helper toggles `IFF_UP` on `lo` using `ioctl(SIOCSIFFLAGS)` — the
/// same approach `ip link set lo up` uses under the hood, minus the
/// netlink round trip.
///
/// Requires `CAP_NET_ADMIN` in the current network namespace, which
/// user-namespace root inherently has.
pub fn bring_up_loopback() -> io::Result<()> {
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    // SAFETY: socket(2) with valid domain/type/protocol.
    let raw_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) }.syscall_result()?;
    // SAFETY: raw_fd is a fresh, non-negative fd from socket(2); we
    // take ownership so it is closed on drop.
    let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let raw_fd = std::os::fd::AsRawFd::as_raw_fd(&fd);

    // Set up an ifreq referencing "lo".
    //
    // SAFETY: ifreq is C-layout POD; zero-initialization is valid.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (i, &b) in b"lo\0".iter().enumerate() {
        ifr.ifr_name[i] = b as libc::c_char;
    }

    // Read current flags.
    // SAFETY: SIOCGIFFLAGS with a valid ifreq pointer; the kernel fills
    // in ifr_ifru.ifru_flags on success.
    unsafe { libc::ioctl(raw_fd, libc::SIOCGIFFLAGS as _, &mut ifr) }.syscall_result()?;

    // OR in IFF_UP.
    //
    // SAFETY: after SIOCGIFFLAGS, ifr_ifru.ifru_flags is the active
    // union variant.
    unsafe {
        ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
    }

    // Write the flags back.
    // SAFETY: SIOCSIFFLAGS with a valid ifreq pointer.
    unsafe { libc::ioctl(raw_fd, libc::SIOCSIFFLAGS as _, &ifr) }.syscall_result()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::net::TcpListener;
    use std::process::Command;
    use test_with_tracing::test;

    #[test]
    fn network_operations_work_in_namespace() {
        let exe = std::env::current_exe().expect("failed to get test executable path");
        let output = Command::new("unshare")
            .args(["--user", "--map-root-user", "--net"])
            .arg(exe)
            .args([
                "--exact",
                "unix::network_namespace::tests::helper_network_operations",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .expect("failed to spawn network namespace helper");

        assert!(
            output.status.success(),
            "network namespace helper failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore]
    fn helper_network_operations() {
        let interfaces = interface_names();
        assert_eq!(interfaces, ["lo"]);

        bring_up_loopback().expect("failed to bring up loopback");
        TcpListener::bind(("127.0.0.1", 0)).expect("loopback is not usable after being enabled");
    }

    fn interface_names() -> Vec<String> {
        // SAFETY: if_nameindex returns a sentinel-terminated array that remains
        // valid until passed to if_freenameindex.
        let interfaces = unsafe { libc::if_nameindex() };
        assert!(!interfaces.is_null(), "if_nameindex failed");
        let mut names = Vec::new();
        let mut interface = interfaces;
        // SAFETY: The array is terminated by an entry with a zero index and
        // null name, as required by if_nameindex(3).
        unsafe {
            while (*interface).if_index != 0 || !(*interface).if_name.is_null() {
                names.push(
                    CStr::from_ptr((*interface).if_name)
                        .to_string_lossy()
                        .into_owned(),
                );
                interface = interface.add(1);
            }
            libc::if_freenameindex(interfaces);
        }
        names
    }
}
