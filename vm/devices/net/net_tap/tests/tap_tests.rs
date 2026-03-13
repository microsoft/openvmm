// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for net_tap using a custom test harness (libtest-mimic)
//! so that no threads are spawned before we call `unshare(CLONE_NEWUSER)`.

// TAP interfaces and user namespaces are Linux-only. Provide a no-op main for
// other targets since this binary uses `harness = false`.
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    tap_tests::main();
}

#[cfg(target_os = "linux")]
mod tap_tests {
    // UNSAFETY: Calling libc functions for namespace setup and network interface
    // configuration (ioctls for link-up, address, netmask).
    #![expect(unsafe_code)]

    use libtest_mimic::Arguments;
    use libtest_mimic::Trial;
    use net_backend::Endpoint;
    use net_backend::QueueConfig;
    use net_backend::RxId;
    use net_backend::TxId;
    use net_backend::TxMetadata;
    use net_backend::TxSegment;
    use net_backend::TxSegmentType;
    use net_tap::TapEndpoint;
    use pal_async::DefaultDriver;
    use std::future::poll_fn;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;

    /// Enter an isolated user + network namespace. This gives us CAP_NET_ADMIN
    /// without requiring root privileges on the host. Must be called while the
    /// process is still single-threaded.
    ///
    /// Returns `Ok(())` on success, or `Err` with a message if unprivileged
    /// user namespaces are not available (EPERM). Panics on unexpected errors.
    fn enter_test_netns() -> Result<(), String> {
        // SAFETY: unshare() with CLONE_NEWUSER | CLONE_NEWNET is safe — it only
        // affects the calling process's namespace membership.
        let ret = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // EPERM means unprivileged user namespaces are disabled (e.g.,
            // kernel.unprivileged_userns_clone=0 or restricted by LSM).
            // Skip tests gracefully in that case; panic on anything else.
            if err.raw_os_error() == Some(libc::EPERM) {
                return Err(format!(
                    "unshare(CLONE_NEWUSER | CLONE_NEWNET) failed: {err}",
                ));
            }
            panic!("unshare(CLONE_NEWUSER | CLONE_NEWNET) failed unexpectedly: {err}");
        }
        // Note: we intentionally skip writing /proc/self/{setgroups,uid_map,gid_map}.
        // The unshare call alone grants full capabilities (including CAP_NET_ADMIN)
        // inside the new user namespace — UID/GID mapping only affects how
        // ownership appears and is not needed for TAP device operations.

        // Verify that TAP devices actually work inside the namespace. Some CI
        // environments allow user namespaces but restrict /dev/net/tun access.
        if let Err(e) = TapEndpoint::new("tap_probe") {
            return Err(format!("TAP not available in namespace: {e}"));
        }

        Ok(())
    }

    /// Build a zeroed `ifreq` with the interface name filled in.
    fn new_ifreq(name: &str) -> libc::ifreq {
        // SAFETY: All-zero is a valid `ifreq`.
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        let bytes = name.as_bytes();
        assert!(bytes.len() < libc::IF_NAMESIZE, "interface name too long");
        for (i, &b) in bytes.iter().enumerate() {
            ifr.ifr_name[i] = b as libc::c_char;
        }
        ifr
    }

    /// Create a `sockaddr` from an IPv4 address (for use in `ifreq` unions).
    fn sockaddr_in4(addr: std::net::Ipv4Addr) -> libc::sockaddr {
        // SAFETY: All-zero is a valid `sockaddr_in`.
        let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sa.sin_family = libc::AF_INET as libc::sa_family_t;
        sa.sin_addr.s_addr = u32::from(addr).to_be();
        // SAFETY: `sockaddr_in` and `sockaddr` have compatible layout — this is
        // the standard C idiom for socket address casting.
        unsafe { std::ptr::from_ref(&sa).cast::<libc::sockaddr>().read() }
    }

    /// Bring up a TAP interface and assign an IP address using ioctls.
    /// Only works inside the namespace where we have CAP_NET_ADMIN.
    fn configure_tap(name: &str, cidr: &str) {
        // Parse CIDR notation.
        let (addr_str, prefix_str) = cidr.split_once('/').expect("CIDR must contain '/'");
        let addr: std::net::Ipv4Addr = addr_str.parse().expect("invalid IPv4 address");
        let prefix_len: u32 = prefix_str.parse().expect("invalid prefix length");
        assert!(prefix_len <= 32);
        let netmask = if prefix_len == 0 {
            0u32
        } else {
            !0u32 << (32 - prefix_len)
        };

        // SAFETY: Creating an AF_INET/SOCK_DGRAM socket for ioctls.
        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        assert!(
            sock >= 0,
            "socket() failed: {}",
            std::io::Error::last_os_error()
        );
        // Wrap in OwnedFd so the socket is closed even on panic.
        // SAFETY: `sock` is a valid, newly created file descriptor.
        let sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(sock) };
        let fd = sock.as_raw_fd();

        let mut ifr = new_ifreq(name);

        // SAFETY: SIOCGIFFLAGS / SIOCSIFFLAGS are standard Linux ioctls that
        // read/write the `ifru_flags` field of an `ifreq`.
        unsafe {
            assert_eq!(
                libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut ifr),
                0,
                "SIOCGIFFLAGS: {}",
                std::io::Error::last_os_error()
            );
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            assert_eq!(
                libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr),
                0,
                "SIOCSIFFLAGS: {}",
                std::io::Error::last_os_error()
            );
        }

        // SAFETY: SIOCSIFADDR writes the `ifru_addr` field of an `ifreq`.
        unsafe {
            ifr.ifr_ifru.ifru_addr = sockaddr_in4(addr);
            assert_eq!(
                libc::ioctl(fd, libc::SIOCSIFADDR as _, &ifr),
                0,
                "SIOCSIFADDR: {}",
                std::io::Error::last_os_error()
            );
        }

        // SAFETY: SIOCSIFNETMASK writes the `ifru_netmask` field of an `ifreq`.
        unsafe {
            ifr.ifr_ifru.ifru_netmask = sockaddr_in4(std::net::Ipv4Addr::from(netmask));
            assert_eq!(
                libc::ioctl(fd, libc::SIOCSIFNETMASK as _, &ifr),
                0,
                "SIOCSIFNETMASK: {}",
                std::io::Error::last_os_error()
            );
        }

        // `sock` is dropped here, closing the file descriptor.
    }

    /// Create a buffer pool and guest memory following the pattern from net_backend tests.
    fn make_pool() -> (net_backend::tests::Bufs, guestmem::GuestMemory) {
        let layout = net_backend::tests::test_layout();
        let mem = guestmem::GuestMemory::allocate(layout.end_of_ram() as usize);
        let pool = net_backend::tests::Bufs::new(mem.clone());
        (pool, mem)
    }

    /// Wrap an async test function into a [`Trial`] that runs on a
    /// single-threaded event loop.
    fn async_trial(name: &str, f: impl AsyncFnOnce(DefaultDriver) + Send + 'static) -> Trial {
        Trial::test(name, move || {
            pal_async::DefaultPool::run_with(f);
            Ok(())
        })
    }

    // ---------------------------------------------------------------------------
    // Test implementations
    // ---------------------------------------------------------------------------

    /// Validates that creating a TAP endpoint succeeds inside a user namespace.
    fn test_tap_create() -> Result<(), libtest_mimic::Failed> {
        TapEndpoint::new("tap0").map_err(|e| format!("TapEndpoint::new failed: {e}"))?;
        Ok(())
    }

    /// Validates that `get_queues` returns exactly one queue.
    async fn test_tap_get_queues(driver: DefaultDriver) {
        let mut endpoint = TapEndpoint::new("tap0").unwrap();
        let (pool, _mem) = make_pool();
        let initial_rx: Vec<_> = (1..128).map(RxId).collect();
        let config = vec![QueueConfig {
            pool: Box::new(pool),
            initial_rx: &initial_rx,
            driver: Box::new(driver.clone()),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        assert_eq!(
            queues.len(),
            1,
            "TAP endpoint should return exactly one queue"
        );
    }

    /// Validates that transmitting a frame through the TAP queue succeeds.
    async fn test_tap_tx_sends_frame(driver: DefaultDriver) {
        let mut endpoint = TapEndpoint::new("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (pool, mem) = make_pool();
        let initial_rx: Vec<_> = (1..128).map(RxId).collect();
        let config = vec![QueueConfig {
            pool: Box::new(pool),
            initial_rx: &initial_rx,
            driver: Box::new(driver.clone()),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = &mut queues[0];

        // Build a minimal Ethernet frame:
        // dst MAC (6) + src MAC (6) + ethertype (2) + payload
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // dst: broadcast
        frame.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]); // src
        frame.extend_from_slice(&[0x08, 0x00]); // ethertype: IPv4
        frame.extend_from_slice(&[0u8; 46]); // payload (minimum Ethernet frame size)
        let frame_len = frame.len() as u32;

        // Write frame into guest memory at GPA 0
        mem.write_at(0, &frame).unwrap();

        let segments = [TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                id: TxId(0),
                segment_count: 1,
                len: frame_len,
                ..Default::default()
            }),
            gpa: 0,
            len: frame_len,
        }];

        let (completed, count) = queue.tx_avail(&segments).unwrap();
        assert!(completed, "tx should complete synchronously");
        assert_eq!(count, 1, "should have processed 1 segment");
    }

    /// Validates that a packet sent into the network triggers an ARP request
    /// that can be received on the TAP queue.
    async fn test_tap_rx_receives_packet(driver: DefaultDriver) {
        let mut endpoint = TapEndpoint::new("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (pool, mem) = make_pool();
        let initial_rx: Vec<_> = (1..128).map(RxId).collect();
        let config = vec![QueueConfig {
            pool: Box::new(pool),
            initial_rx: &initial_rx,
            driver: Box::new(driver.clone()),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = &mut queues[0];

        // Send a UDP datagram to an address on the TAP subnet. The kernel will
        // generate an ARP request out the TAP interface.
        let sock = std::net::UdpSocket::bind("10.0.0.1:0").unwrap();
        sock.send_to(b"hello", "10.0.0.2:12345").unwrap();

        // Poll until a packet arrives, then scan for an ARP frame. The kernel
        // may emit other L2 traffic (e.g., IPv6 Neighbor Discovery) before the
        // ARP request we triggered, so we cannot assume it is the first packet.
        poll_fn(|cx| queue.poll_ready(cx)).await;

        let mut packets = [RxId(0); 128];
        let n = queue.rx_poll(&mut packets).unwrap();
        assert!(n >= 1, "should have received at least one packet");

        let mut found_arp = false;
        for &rx_id in &packets[..n] {
            let gpa = rx_id.0 as u64 * 2048;
            let mut buf = [0u8; 2048];
            mem.read_at(gpa, &mut buf).unwrap();

            // Ethertype is at bytes 12-13 in the Ethernet frame.
            let ethertype = u16::from_be_bytes([buf[12], buf[13]]);
            if ethertype == 0x0806 {
                found_arp = true;
                break;
            }
        }
        assert!(
            found_arp,
            "expected at least one ARP packet (ethertype 0x0806)"
        );
    }

    /// Validates that flooding tx_avail doesn't panic or error — packets are
    /// silently dropped when the kernel buffer fills up.
    async fn test_tap_tx_wouldblock_drops(driver: DefaultDriver) {
        let mut endpoint = TapEndpoint::new("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (pool, mem) = make_pool();
        let initial_rx: Vec<_> = (1..128).map(RxId).collect();
        let config = vec![QueueConfig {
            pool: Box::new(pool),
            initial_rx: &initial_rx,
            driver: Box::new(driver.clone()),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = &mut queues[0];

        // Build a 1500-byte frame
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // dst: broadcast
        frame.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]); // src
        frame.extend_from_slice(&[0x08, 0x00]); // ethertype: IPv4
        frame.resize(1500, 0xAA); // pad to 1500 bytes
        let frame_len = frame.len() as u32;

        // Write frame into guest memory at GPA 0
        mem.write_at(0, &frame).unwrap();

        let segments = [TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                id: TxId(0),
                segment_count: 1,
                len: frame_len,
                ..Default::default()
            }),
            gpa: 0,
            len: frame_len,
        }];

        // Flood with packets — should never error.
        for _ in 0..10000 {
            let (completed, count) = queue.tx_avail(&segments).unwrap();
            assert!(completed, "tx should always complete synchronously");
            assert_eq!(count, 1);
        }
    }

    // ---------------------------------------------------------------------------
    // Harness
    // ---------------------------------------------------------------------------

    pub(crate) fn main() {
        let args = Arguments::from_args();

        // Only enter the namespace when actually running tests—not when
        // nextest calls `--list` to discover them.
        let ns_available = if args.list {
            true // assume available; will be checked on actual run
        } else {
            match enter_test_netns() {
                Ok(()) => true,
                Err(msg) => {
                    eprintln!("note: skipping TAP tests — {msg}");
                    false
                }
            }
        };

        let ignored = !ns_available;
        let tests = [
            Trial::test("tap_create", test_tap_create),
            async_trial("tap_get_queues", test_tap_get_queues),
            async_trial("tap_tx_sends_frame", test_tap_tx_sends_frame),
            async_trial("tap_rx_receives_packet", test_tap_rx_receives_packet),
            async_trial("tap_tx_wouldblock_drops", test_tap_tx_wouldblock_drops),
        ]
        .map(|t| t.with_ignored_flag(ignored))
        .into();

        libtest_mimic::run(&args, tests).exit();
    }
} // mod tap_tests
