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
    use net_backend::TxFlags;
    use net_backend::TxId;
    use net_backend::TxMetadata;
    use net_backend::TxSegment;
    use net_backend::TxSegmentType;
    use net_tap::TapEndpoint;
    use net_tap::tap;
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
        if let Err(e) = new_endpoint("tap_probe") {
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

    /// Create a TapEndpoint by name, opening the TAP device with default
    /// configuration.
    fn new_endpoint(name: &str) -> Result<TapEndpoint, tap::Error> {
        let fd = tap::open_tap(name)?;
        let tap = tap::Tap::new(fd)?;
        TapEndpoint::new(tap)
    }

    // ---------------------------------------------------------------------------
    // Test implementations
    // ---------------------------------------------------------------------------

    /// Validates that creating a TAP endpoint succeeds inside a user namespace.
    fn test_tap_create() -> Result<(), libtest_mimic::Failed> {
        new_endpoint("tap0").map_err(|e| format!("TapEndpoint::new failed: {e}"))?;
        Ok(())
    }

    /// Validates that `get_queues` returns exactly one queue.
    async fn test_tap_get_queues(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        let config = vec![QueueConfig {
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
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
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

        let (completed, count) = queue.tx_avail(&mut pool, &segments).unwrap();
        assert!(completed, "tx should complete synchronously");
        assert_eq!(count, 1, "should have processed 1 segment");
    }

    /// Validates that a packet sent into the network triggers an ARP request
    /// that can be received on the TAP queue.
    async fn test_tap_rx_receives_packet(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (mut pool, mem) = make_pool();
        let initial_rx: Vec<_> = (1..128).map(RxId).collect();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = &mut queues[0];

        // Post initial RX buffers.
        queue.rx_avail(&mut pool, &initial_rx);

        // Send a UDP datagram to an address on the TAP subnet. The kernel will
        // generate an ARP request out the TAP interface.
        let sock = std::net::UdpSocket::bind("10.0.0.1:0").unwrap();
        sock.send_to(b"hello", "10.0.0.2:12345").unwrap();

        // Poll until a packet arrives, then scan for an ARP frame. The kernel
        // may emit other L2 traffic (e.g., IPv6 Neighbor Discovery) before the
        // ARP request we triggered, so we cannot assume it is the first packet.
        poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;

        let mut packets = [RxId(0); 128];
        let n = queue.rx_poll(&mut pool, &mut packets).unwrap();
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
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
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
            let (completed, count) = queue.tx_avail(&mut pool, &segments).unwrap();
            assert!(completed, "tx should always complete synchronously");
            assert_eq!(count, 1);
        }
    }

    /// Validates that transmitting with offloads succeeds through to the kernel.
    async fn test_tap_tx_with_offloads(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = &mut queues[0];

        // Build a minimal TCP/IPv4 packet:
        // Ethernet (14) + IPv4 (20) + TCP (20) + payload
        let mut frame = Vec::new();
        frame.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // dst: broadcast
        frame.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]); // src
        frame.extend_from_slice(&[0x08, 0x00]); // ethertype: IPv4
        // Minimal IPv4 header (20 bytes)
        frame.extend_from_slice(&[
            0x45, 0x00, 0x00, 0x28, // version/IHL, DSCP, total length (40)
            0x00, 0x00, 0x00, 0x00, // id, flags, fragment offset
            0x40, 0x06, 0x00, 0x00, // TTL, protocol (TCP), checksum
            0x0a, 0x00, 0x00, 0x01, // src: 10.0.0.1
            0x0a, 0x00, 0x00, 0x02, // dst: 10.0.0.2
        ]);
        // Minimal TCP header (20 bytes)
        frame.extend_from_slice(&[
            0x00, 0x50, 0x00, 0x51, // src port 80, dst port 81
            0x00, 0x00, 0x00, 0x00, // seq
            0x00, 0x00, 0x00, 0x00, // ack
            0x50, 0x02, 0x00, 0x00, // data offset, flags (SYN)
            0x00, 0x00, 0x00, 0x00, // checksum, urgent pointer
        ]);
        let frame_len = frame.len() as u32;

        mem.write_at(0, &frame).unwrap();

        let segments = [TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                id: TxId(0),
                segment_count: 1,
                flags: TxFlags::new()
                    .with_offload_tcp_checksum(true)
                    .with_offload_ip_header_checksum(true)
                    .with_is_ipv4(true),
                len: frame_len,
                l2_len: 14,
                l3_len: 20,
                l4_len: 20,
                ..Default::default()
            }),
            gpa: 0,
            len: frame_len,
        }];

        let (completed, count) = queue.tx_avail(&mut pool, &segments).unwrap();
        assert!(completed, "tx should complete synchronously");
        assert_eq!(count, 1, "should have processed 1 segment");
    }

    /// Validates that TSO packets with zeroed IPv4 header checksums (the NDIS
    /// LSO convention) are delivered correctly through the kernel's IP stack.
    ///
    /// NDIS Large Send Offload packets arrive from hv_netvsc with the IPv4
    /// header checksum set to zero, because NDIS expects the NIC hardware to
    /// recompute it per-segment during TCP segmentation. When net_tap writes
    /// these packets to the TAP fd, the kernel's `ip_rcv_core` validates the
    /// IPv4 header checksum and silently drops packets that fail.
    ///
    /// This test sends a TSO packet with zeroed IPv4 header checksum through
    /// `tx_avail` and verifies via a raw socket that the kernel actually
    /// accepted it (i.e., the checksum was fixed up before writing to TAP).
    async fn test_tap_tso_ipv4_checksum(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = &mut queues[0];

        // Open a raw socket to observe whether the kernel's IP stack accepts
        // the packet. SOCK_RAW + IPPROTO_TCP delivers a copy of every inbound
        // TCP packet that passes ip_rcv_core (which validates the IPv4 header
        // checksum). If the checksum is invalid, the packet is silently
        // dropped and recv() returns EAGAIN.
        //
        // SAFETY: Creating a raw socket; we have CAP_NET_RAW inside the user
        // namespace created by enter_test_netns().
        let raw_fd = unsafe {
            libc::socket(
                libc::AF_INET,
                libc::SOCK_RAW | libc::SOCK_NONBLOCK,
                libc::IPPROTO_TCP,
            )
        };
        assert!(
            raw_fd >= 0,
            "raw socket: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: raw_fd is a valid, newly created file descriptor.
        let raw_sock = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) };

        // Set a receive timeout so we don't hang forever if the packet is
        // dropped (the expected behavior before the fix).
        let tv = libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        };
        // SAFETY: setsockopt with SO_RCVTIMEO and a valid timeval.
        unsafe {
            let ret = libc::setsockopt(
                raw_sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                std::ptr::from_ref(&tv).cast::<libc::c_void>(),
                size_of_val(&tv) as libc::socklen_t,
            );
            assert_eq!(ret, 0, "SO_RCVTIMEO: {}", std::io::Error::last_os_error());
        }

        // Build a TSO packet: Ethernet (14) + IPv4 (20) + TCP (20) + 2920
        // bytes of payload (2 * MSS). The IPv4 header checksum is zeroed
        // per NDIS LSO convention.
        let mut frame = Vec::new();
        // Ethernet header
        frame.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]); // dst: broadcast
        frame.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]); // src
        frame.extend_from_slice(&[0x08, 0x00]); // ethertype: IPv4
        // IPv4 header (20 bytes) — checksum = 0 (NDIS LSO convention)
        let ip_total_len: u16 = 20 + 20 + 2920;
        let tl = ip_total_len.to_be_bytes();
        frame.extend_from_slice(&[
            0x45, 0x00, tl[0], tl[1], // IHL=5, total_len
            0x00, 0x01, 0x40, 0x00, // id=1, DF
            0x40, 0x06, 0x00, 0x00, // TTL=64, proto=TCP, checksum=0
            0x0a, 0x00, 0x00, 0x02, // src: 10.0.0.2
            0x0a, 0x00, 0x00, 0x01, // dst: 10.0.0.1 (our TAP IP)
        ]);
        // TCP header (20 bytes) — use a distinctive src port for matching.
        // The pseudo-header checksum is pre-filled in the TCP checksum field
        // as required by VIRTIO_NET_HDR_F_NEEDS_CSUM.
        let tcp_len: u32 = 20 + 2920;
        let pseudo_sum: u32 = 0x0a000002u32.wrapping_shr(16) // src hi
            + (0x0a000002u32 & 0xffff)             // src lo
            + 0x0a000001u32.wrapping_shr(16)        // dst hi
            + (0x0a000001u32 & 0xffff)              // dst lo
            + 6u32                                          // protocol (TCP)
            + tcp_len; // TCP length
        let mut pseudo_sum = pseudo_sum;
        while pseudo_sum >> 16 != 0 {
            pseudo_sum = (pseudo_sum & 0xffff) + (pseudo_sum >> 16);
        }
        let pseudo_csum = (pseudo_sum as u16).to_be_bytes();
        frame.extend_from_slice(&[
            0xab,
            0xcd,
            0x00,
            0x50, // src port 0xABCD, dst port 80
            0x00,
            0x00,
            0x00,
            0x01, // seq = 1
            0x00,
            0x00,
            0x00,
            0x00, // ack = 0
            0x50,
            0x02,
            0x20,
            0x00, // data offset=5, SYN, window=8192
            pseudo_csum[0],
            pseudo_csum[1], // checksum = pseudo-header sum
            0x00,
            0x00, // urgent = 0
        ]);
        // Payload: 2 MSS worth of data.
        frame.extend_from_slice(&[0x42; 2920]);

        let frame_len = frame.len() as u32;
        mem.write_at(0, &frame).unwrap();

        let segments = [TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                id: TxId(0),
                segment_count: 1,
                flags: TxFlags::new()
                    .with_offload_tcp_segmentation(true)
                    .with_offload_tcp_checksum(true)
                    .with_offload_ip_header_checksum(true)
                    .with_is_ipv4(true),
                len: frame_len,
                l2_len: 14,
                l3_len: 20,
                l4_len: 20,
                transport_header_offset: 34,
                max_segment_size: 1460,
                vlan: None,
            }),
            gpa: 0,
            len: frame_len,
        }];

        let (completed, count) = queue.tx_avail(&mut pool, &segments).unwrap();
        assert!(completed, "tx should complete synchronously");
        assert_eq!(count, 1);

        // Try to receive on the raw socket. If the IPv4 header checksum
        // was not fixed up, the kernel drops the packet in ip_rcv_core
        // and recv() returns EAGAIN/EWOULDBLOCK after the timeout.
        let mut buf = [0u8; 4096];
        // SAFETY: recv on a valid raw socket fd with a valid buffer.
        let n = unsafe {
            libc::recv(
                raw_sock.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                0,
            )
        };
        assert!(
            n > 0,
            "expected to receive TCP segment(s) from TSO packet; \
             kernel likely dropped the packet due to invalid IPv4 header \
             checksum (recv returned {n}, errno={})",
            std::io::Error::last_os_error()
        );
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
            async_trial("tap_tx_with_offloads", test_tap_tx_with_offloads),
            async_trial("tap_tso_ipv4_checksum", test_tap_tso_ipv4_checksum),
        ]
        .map(|t| t.with_ignored_flag(ignored))
        .into();

        libtest_mimic::run(&args, tests).exit();
    }
} // mod tap_tests
