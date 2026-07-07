// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! iperf3 helper subprocess for network throughput tests.
//!
//! This module implements a child process that serves iperf3 server
//! requests via mesh RPC. Both the Consomme and TAP backends use this
//! same helper — the TAP variant just does `unshare` before connecting
//! to mesh (see `run_tap_helper`).

// UNSAFETY: Calling libc functions for namespace setup (unshare) and
// network interface configuration (socket, ioctls).
#![cfg_attr(target_os = "linux", expect(unsafe_code))]

use mesh::MeshPayload;
use mesh::rpc::FailableRpc;

/// Initial message sent from parent to the helper via mesh.
#[derive(MeshPayload)]
pub struct IperfHelperInit {
    pub ready: mesh::OneshotSender<Result<IperfHelperReady, String>>,
}

/// Sent from helper to parent after it's ready to serve requests.
#[derive(MeshPayload)]
pub struct IperfHelperReady {
    pub requests: mesh::Sender<IperfRequest>,
}

/// Request from parent to the iperf3 helper.
#[derive(MeshPayload)]
pub enum IperfRequest {
    /// Spawn iperf3 server, run until client disconnects, return JSON output.
    RunIperf3(FailableRpc<Iperf3Args, String>),
    /// Create a TAP device in the helper's (namespaced) network stack and
    /// return the fd. Only valid when the helper was started with
    /// `run_tap_helper`.
    #[cfg(target_os = "linux")]
    SetupTap(FailableRpc<TapConfig, std::os::fd::OwnedFd>),
    /// Shut down the helper.
    Stop,
}

/// Arguments for an iperf3 invocation.
#[derive(MeshPayload)]
pub struct Iperf3Args {
    pub args: Vec<String>,
}

/// Configuration for creating a TAP device.
#[cfg(target_os = "linux")]
#[derive(MeshPayload)]
pub struct TapConfig {
    /// TAP device name (e.g., "tap0").
    pub name: String,
    /// CIDR for the host side of the TAP (e.g., "192.168.100.1/24").
    pub cidr: String,
}

/// Entry point for the plain iperf3 helper (no namespace).
///
/// Can be called at any point — no single-threaded requirement.
pub fn run_helper() {
    if let Err(e) = mesh_process::try_run_mesh_host("burette", async |init: IperfHelperInit| {
        run_helper_inner(init).await;
        Ok(())
    }) {
        eprintln!("iperf helper failed: {e}");
        std::process::exit(1);
    }
}

async fn run_helper_inner(init: IperfHelperInit) {
    let (req_send, req_recv) = mesh::channel();
    init.ready.send(Ok(IperfHelperReady { requests: req_send }));
    serve_requests(req_recv).await;
}

/// Serve iperf3 (and optionally TAP setup) requests until the channel
/// is closed or a Stop request is received.
async fn serve_requests(mut recv: mesh::Receiver<IperfRequest>) {
    while let Ok(req) = recv.recv().await {
        match req {
            IperfRequest::RunIperf3(rpc) => {
                rpc.handle_failable(async |args| -> Result<String, String> {
                    let iperf3 = std::env::var("IPERF3").unwrap_or_else(|_| "iperf3".into());
                    let output = std::process::Command::new(&iperf3)
                        .args(&args.args)
                        .output()
                        .map_err(|e| format!("failed to spawn iperf3: {e}"))?;

                    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

                    if !output.status.success() {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if stdout.is_empty() {
                            return Err(format!(
                                "iperf3 exited with {} and produced no output: {}",
                                output.status,
                                stderr.trim()
                            ));
                        }
                        tracing::warn!(
                            status = %output.status,
                            stderr = %stderr.trim(),
                            "iperf3 server exited non-zero (may still have valid JSON)"
                        );
                    }

                    Ok(stdout)
                })
                .await;
            }
            #[cfg(target_os = "linux")]
            IperfRequest::SetupTap(rpc) => {
                rpc.handle_failable(async |config| {
                    linux::setup_tap_device(&config.name, &config.cidr)
                })
                .await;
            }
            IperfRequest::Stop => break,
        }
    }
}

#[cfg(target_os = "linux")]
pub mod linux {
    use anyhow::Context as _;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;

    /// Entry point for the TAP namespace helper.
    ///
    /// This MUST be called before any threads are spawned (before clap
    /// parsing, before pal_async pool creation). The `unshare()` syscall
    /// requires a single-threaded process.
    pub fn run_tap_helper() {
        // SAFETY: unshare() with CLONE_NEWUSER | CLONE_NEWNET is safe — it only
        // affects the calling process's namespace membership.
        let ret = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // Print a detailed message so the parent can diagnose the failure.
            // The parent will see "iperf helper did not respond" — this
            // stderr line is the only clue about the real cause.
            eprintln!(
                "unshare(CLONE_NEWUSER | CLONE_NEWNET) failed: {err}\n\
                 hint: unprivileged user namespaces may be disabled \
                 (sysctl kernel.unprivileged_userns_clone=1)"
            );
            std::process::exit(1);
        }

        // Now join mesh and serve — same as the plain helper.
        super::run_helper();
    }

    /// Create and configure a TAP device. Returns the TAP fd.
    pub fn setup_tap_device(name: &str, cidr: &str) -> anyhow::Result<std::os::fd::OwnedFd> {
        let tap_fd = net_tap::tap::open_tap(name).context("failed to create TAP device")?;
        configure_tap_interface(name, cidr).context("failed to configure TAP interface")?;
        Ok(tap_fd)
    }

    /// Bring up a TAP interface and assign an IP address using ioctls.
    fn configure_tap_interface(name: &str, cidr: &str) -> anyhow::Result<()> {
        let (addr_str, prefix_str) = cidr.split_once('/').context("CIDR must contain '/'")?;
        let addr: std::net::Ipv4Addr = addr_str.parse().context("invalid IPv4 address")?;
        let prefix_len: u32 = prefix_str.parse().context("invalid prefix length")?;
        anyhow::ensure!(prefix_len <= 32, "prefix length {prefix_len} > 32");
        let netmask = if prefix_len == 0 {
            0u32
        } else {
            !0u32 << (32 - prefix_len)
        };

        // SAFETY: Creating an AF_INET/SOCK_DGRAM socket for ioctls.
        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        anyhow::ensure!(
            sock >= 0,
            "socket() failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: `sock` is a valid, newly created file descriptor.
        let sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(sock) };
        let fd = sock.as_raw_fd();

        let mut ifr = new_ifreq(name)?;

        // SAFETY: SIOCGIFFLAGS / SIOCSIFFLAGS are standard Linux ioctls.
        unsafe {
            anyhow::ensure!(
                libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut ifr) == 0,
                "SIOCGIFFLAGS: {}",
                std::io::Error::last_os_error()
            );
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            anyhow::ensure!(
                libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr) == 0,
                "SIOCSIFFLAGS: {}",
                std::io::Error::last_os_error()
            );
        }

        // SAFETY: SIOCSIFADDR writes the `ifru_addr` field of an `ifreq`.
        unsafe {
            ifr.ifr_ifru.ifru_addr = sockaddr_in4(addr);
            anyhow::ensure!(
                libc::ioctl(fd, libc::SIOCSIFADDR as _, &ifr) == 0,
                "SIOCSIFADDR: {}",
                std::io::Error::last_os_error()
            );
        }

        // SAFETY: SIOCSIFNETMASK writes the `ifru_netmask` field of an `ifreq`.
        unsafe {
            ifr.ifr_ifru.ifru_netmask = sockaddr_in4(std::net::Ipv4Addr::from(netmask));
            anyhow::ensure!(
                libc::ioctl(fd, libc::SIOCSIFNETMASK as _, &ifr) == 0,
                "SIOCSIFNETMASK: {}",
                std::io::Error::last_os_error()
            );
        }

        Ok(())
    }

    fn new_ifreq(name: &str) -> anyhow::Result<libc::ifreq> {
        // SAFETY: All-zero is a valid `ifreq`.
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        let bytes = name.as_bytes();
        anyhow::ensure!(
            bytes.len() < libc::IF_NAMESIZE,
            "interface name too long: {name:?}"
        );
        for (i, &b) in bytes.iter().enumerate() {
            ifr.ifr_name[i] = b as libc::c_char;
        }
        Ok(ifr)
    }

    fn sockaddr_in4(addr: std::net::Ipv4Addr) -> libc::sockaddr {
        // SAFETY: All-zero is a valid `sockaddr_in`.
        let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sa.sin_family = libc::AF_INET as libc::sa_family_t;
        sa.sin_addr.s_addr = u32::from(addr).to_be();
        // SAFETY: `sockaddr_in` and `sockaddr` have compatible layout.
        unsafe { std::ptr::from_ref(&sa).cast::<libc::sockaddr>().read() }
    }
}
