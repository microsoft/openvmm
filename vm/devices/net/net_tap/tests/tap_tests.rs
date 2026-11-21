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

    fn disable_ipv6(name: &str) {
        std::fs::write(format!("/proc/sys/net/ipv6/conf/{name}/disable_ipv6"), b"1")
            .expect("failed to disable IPv6 on TAP interface");
    }

    /// Create a buffer pool and guest memory following the pattern from net_backend tests.
    fn make_pool() -> (net_backend::tests::Bufs, guestmem::GuestMemory) {
        let layout = net_backend::tests::test_layout();
        let mem = guestmem::GuestMemory::allocate(layout.end_of_ram() as usize);
        let pool = net_backend::tests::Bufs::new(mem.clone());
        (pool, mem)
    }

    fn send_ethernet_frame(name: &str, frame: &[u8]) {
        const ETH_P_ALL: u16 = 0x0003;

        // SAFETY: Creating an AF_PACKET socket; the test namespace grants
        // CAP_NET_RAW.
        let fd = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW,
                i32::from(ETH_P_ALL.to_be()),
            )
        };
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        // SAFETY: fd is a valid, newly created file descriptor.
        let socket = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };

        let name = std::ffi::CString::new(name).unwrap();
        // SAFETY: name is a valid, nul-terminated interface name.
        let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
        assert_ne!(
            ifindex,
            0,
            "if_nametoindex: {}",
            std::io::Error::last_os_error()
        );

        // SAFETY: All-zero is a valid sockaddr_ll starting state.
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = ETH_P_ALL.to_be();
        addr.sll_ifindex = ifindex as i32;

        // SAFETY: socket, frame, and sockaddr_ll are valid for sendto().
        let sent = unsafe {
            libc::sendto(
                socket.as_raw_fd(),
                frame.as_ptr().cast(),
                frame.len(),
                0,
                std::ptr::from_ref(&addr).cast(),
                size_of_val(&addr) as libc::socklen_t,
            )
        };
        assert_eq!(
            sent,
            frame.len() as isize,
            "sendto: {}",
            std::io::Error::last_os_error()
        );
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
            enabled_rx_offloads: Default::default(),
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
            enabled_rx_offloads: Default::default(),
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

    /// A packet socket on a tap's host side, reading what the kernel made of a tap write.
    ///
    /// It sees a frame after `__netif_receive_skb_core` popped its tag, so the tag comes back in
    /// the auxdata and the vnet header describes the untagged bytes (`__packet_rcv_vnet` passes
    /// no VLAN length). A tag written in the wrong place, or offsets not moved past it, shows up
    /// as a difference from the same frame sent without one.
    struct HostSide {
        socket: std::os::fd::OwnedFd,
    }

    /// One frame as [`HostSide`] read it.
    #[derive(Debug, PartialEq, Eq)]
    struct HostFrame {
        flags: u8,
        gso_type: u8,
        hdr_len: u16,
        gso_size: u16,
        csum_start: u16,
        csum_offset: u16,
        /// TCI from the auxdata, when the kernel saw a tag.
        tci: Option<u16>,
        frame: Vec<u8>,
    }

    impl HostSide {
        fn bind(name: &str) -> Self {
            const ETH_P_ALL: u16 = 0x0003;
            const PACKET_VNET_HDR: libc::c_int = 15;

            // SAFETY: Creating an AF_PACKET socket; the test namespace grants CAP_NET_RAW.
            let fd = unsafe {
                libc::socket(
                    libc::AF_PACKET,
                    libc::SOCK_RAW,
                    i32::from(ETH_P_ALL.to_be()),
                )
            };
            assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
            // SAFETY: fd is a valid, newly created file descriptor.
            let socket = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };

            let enable: libc::c_int = 1;
            for option in [libc::PACKET_AUXDATA, PACKET_VNET_HDR] {
                // SAFETY: the option is an int, passed with its size.
                let ret = unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_PACKET,
                        option,
                        std::ptr::from_ref(&enable).cast(),
                        size_of::<libc::c_int>() as libc::socklen_t,
                    )
                };
                assert_eq!(ret, 0, "setsockopt: {}", std::io::Error::last_os_error());
            }
            let timeout = libc::timeval {
                tv_sec: 5,
                tv_usec: 0,
            };
            // SAFETY: the option is a timeval, passed with its size.
            let ret = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    std::ptr::from_ref(&timeout).cast(),
                    size_of::<libc::timeval>() as libc::socklen_t,
                )
            };
            assert_eq!(ret, 0, "SO_RCVTIMEO: {}", std::io::Error::last_os_error());

            let name = std::ffi::CString::new(name).unwrap();
            // SAFETY: name is a valid, nul-terminated interface name.
            let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
            assert_ne!(ifindex, 0);
            // SAFETY: All-zero is a valid `sockaddr_ll`.
            let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
            addr.sll_family = libc::AF_PACKET as u16;
            addr.sll_protocol = ETH_P_ALL.to_be();
            addr.sll_ifindex = ifindex as i32;
            // SAFETY: addr is a valid `sockaddr_ll`, passed with its size.
            let ret = unsafe {
                libc::bind(
                    fd,
                    std::ptr::from_ref(&addr).cast(),
                    size_of::<libc::sockaddr_ll>() as libc::socklen_t,
                )
            };
            assert_eq!(ret, 0, "bind: {}", std::io::Error::last_os_error());
            Self { socket }
        }

        /// The next frame from `src`, skipping whatever the kernel itself sends on the device.
        fn read_from(&self, src: [u8; 6]) -> HostFrame {
            loop {
                let frame = self.read();
                if frame.frame[6..12] == src {
                    return frame;
                }
            }
        }

        fn read(&self) -> HostFrame {
            const VNET_HDR_LEN: usize = 10;
            let mut buf = vec![0u8; 70000];
            let mut control = [0u64; 16];
            let mut iov = libc::iovec {
                iov_base: buf.as_mut_ptr().cast(),
                iov_len: buf.len(),
            };
            // SAFETY: All-zero is a valid `msghdr`.
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = size_of_val(&control) as _;
            // SAFETY: msg points at the iovec and control buffer above, which outlive the call.
            let len = unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut msg, 0) };
            assert!(len >= 0, "recvmsg: {}", std::io::Error::last_os_error());
            let len = len as usize;
            assert!(len >= VNET_HDR_LEN, "short read: {len}");

            let mut tci = None;
            // SAFETY: msg was filled in by recvmsg; the walk stays inside its control buffer.
            let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
            while !cmsg.is_null() {
                // SAFETY: cmsg is non-null, so it points at a header inside the control buffer.
                let header = unsafe { &*cmsg };
                if header.cmsg_level == libc::SOL_PACKET && header.cmsg_type == libc::PACKET_AUXDATA
                {
                    // SAFETY: a PACKET_AUXDATA message holds a `tpacket_auxdata`; its data need
                    // not be aligned for the struct, hence the unaligned read.
                    let aux: libc::tpacket_auxdata = unsafe {
                        libc::CMSG_DATA(cmsg)
                            .cast::<libc::tpacket_auxdata>()
                            .read_unaligned()
                    };
                    if aux.tp_status & libc::TP_STATUS_VLAN_VALID != 0 {
                        tci = Some(aux.tp_vlan_tci);
                    }
                }
                // SAFETY: msg and cmsg are as above.
                cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
            }

            let u16_at = |offset: usize| u16::from_le_bytes([buf[offset], buf[offset + 1]]);
            HostFrame {
                flags: buf[0],
                gso_type: buf[1],
                hdr_len: u16_at(2),
                gso_size: u16_at(4),
                csum_start: u16_at(6),
                csum_offset: u16_at(8),
                tci,
                frame: buf[VNET_HDR_LEN..len].to_vec(),
            }
        }
    }

    /// The source MAC of every frame sent by the tap transmit trials.
    const TX_SRC_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x77];

    /// An IPv4 or IPv6 frame with no tag, `payload_len` bytes after an L4 header of `l4_len`.
    fn ip_frame(ipv6: bool, proto: u8, l4_len: usize, payload_len: usize) -> Vec<u8> {
        let mut frame = vec![0x02, 0, 0, 0, 0, 0x88];
        frame.extend_from_slice(&TX_SRC_MAC);
        let l4_total = u16::try_from(l4_len + payload_len).unwrap();
        if ipv6 {
            frame.extend_from_slice(&[0x86, 0xdd, 0x60, 0, 0, 0]);
            frame.extend_from_slice(&l4_total.to_be_bytes());
            frame.extend_from_slice(&[proto, 64]);
            frame.extend_from_slice(&[0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
            frame.extend_from_slice(&[0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        } else {
            frame.extend_from_slice(&[0x08, 0x00, 0x45, 0]);
            frame.extend_from_slice(&(20 + l4_total).to_be_bytes());
            frame.extend_from_slice(&[0, 1, 0x40, 0, 64, proto, 0, 0]);
            frame.extend_from_slice(&[10, 0, 0, 2, 10, 0, 0, 3]);
        }
        // Ports, then a TCP data offset where a TCP header has one.
        let mut l4 = vec![0u8; l4_len];
        l4[..4].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        if proto == TCP {
            l4[12] = ((l4_len / 4) as u8) << 4;
        }
        frame.extend_from_slice(&l4);
        frame.extend((0..payload_len).map(|i| (i % 251) as u8));
        frame
    }

    const TCP: u8 = 6;
    const UDP: u8 = 17;

    /// A transmit offload shape the tag insert has to get right.
    struct TxShape {
        name: &'static str,
        proto: u8,
        l4_len: usize,
        payload_len: usize,
        flags: TxFlags,
        /// `csum_offset` the kernel reports, 0 when nothing is left partial.
        csum_offset: u16,
    }

    fn tx_shapes() -> Vec<TxShape> {
        // Past a page, so `tun_get_user` sizes the linear part from `hdr_len`, which is how
        // that field becomes visible on the far side.
        const LARGE: usize = 8000;
        vec![
            TxShape {
                name: "no offload",
                proto: TCP,
                l4_len: 20,
                payload_len: 200,
                flags: TxFlags::new(),
                csum_offset: 0,
            },
            TxShape {
                name: "tcp checksum",
                proto: TCP,
                l4_len: 20,
                payload_len: 200,
                flags: TxFlags::new().with_offload_tcp_checksum(true),
                csum_offset: 16,
            },
            TxShape {
                name: "udp checksum",
                proto: UDP,
                l4_len: 8,
                payload_len: 200,
                flags: TxFlags::new().with_offload_udp_checksum(true),
                csum_offset: 6,
            },
            TxShape {
                name: "tso",
                proto: TCP,
                l4_len: 20,
                payload_len: LARGE,
                flags: TxFlags::new()
                    .with_offload_tcp_checksum(true)
                    .with_offload_tcp_segmentation(true),
                csum_offset: 16,
            },
            // TCP options. The kernel pulls a bare TCP header itself whatever `hdr_len` says
            // (`__virtio_net_hdr_to_skb`), so only a longer one makes the field matter.
            TxShape {
                name: "tso with tcp options",
                proto: TCP,
                l4_len: 32,
                payload_len: LARGE,
                flags: TxFlags::new()
                    .with_offload_tcp_checksum(true)
                    .with_offload_tcp_segmentation(true),
                csum_offset: 16,
            },
            TxShape {
                name: "uso",
                proto: UDP,
                l4_len: 8,
                payload_len: LARGE,
                flags: TxFlags::new()
                    .with_offload_udp_checksum(true)
                    .with_offload_udp_segmentation(true),
                csum_offset: 6,
            },
        ]
    }

    /// Send one frame through the queue the way a frontend describes it, and read what the kernel
    /// took from it.
    fn tx_and_read(
        queue: &mut dyn net_backend::Queue,
        pool: &mut net_backend::tests::Bufs,
        mem: &guestmem::GuestMemory,
        host: &HostSide,
        frame: &[u8],
        meta: TxMetadata,
    ) -> HostFrame {
        mem.write_at(0, frame).unwrap();
        let segments = [TxSegment {
            ty: TxSegmentType::Head(TxMetadata {
                segment_count: 1,
                len: frame.len() as u32,
                ..meta
            }),
            gpa: 0,
            len: frame.len() as u32,
        }];
        queue.tx_avail(pool, &segments).unwrap();
        host.read_from(TX_SRC_MAC)
    }

    /// The frame with the checksum field a segmentation send recomputes zeroed: only that field
    /// may differ from what the guest wrote.
    fn with_seed_cleared(mut frame: Vec<u8>, seed_field: Option<usize>) -> Vec<u8> {
        if let Some(field) = seed_field {
            frame[field..field + 2].fill(0);
        }
        frame
    }

    /// Everything a frontend can ask for, in both IP versions: the kernel must end up with the
    /// tag from the metadata, and otherwise with the same frame and offsets as with no tag.
    async fn test_tap_tx_inserts_the_vlan_tag(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");
        disable_ipv6("tap0");
        let host = HostSide::bind("tap0");
        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
            enabled_rx_offloads: Default::default(),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = queues[0].as_mut();

        // A distinct value in each field, so a swapped pair fails.
        let vlan = net_backend::VlanMetadata::new()
            .with_priority(5)
            .with_drop_eligible_indicator(true)
            .with_vlan_id(0x123);
        const TCI: u16 = (5 << 13) | (1 << 12) | 0x123;

        for ipv6 in [false, true] {
            let l3_len: u16 = if ipv6 { 40 } else { 20 };
            for shape in tx_shapes() {
                let what = format!("{} ipv6={ipv6}", shape.name);
                let frame = ip_frame(ipv6, shape.proto, shape.l4_len, shape.payload_len);
                let segmenting = shape.flags.offload_tcp_segmentation()
                    || shape.flags.offload_udp_segmentation();
                let meta = TxMetadata {
                    flags: shape.flags.with_is_ipv4(!ipv6).with_is_ipv6(ipv6),
                    l2_len: 14,
                    l3_len,
                    l4_len: shape.l4_len as u8,
                    max_segment_size: if segmenting { 1000 } else { 0 },
                    ..Default::default()
                };

                let tagged = tx_and_read(
                    queue,
                    &mut pool,
                    &mem,
                    &host,
                    &frame,
                    TxMetadata {
                        vlan: Some(vlan),
                        ..meta.clone()
                    },
                );
                let plain = tx_and_read(queue, &mut pool, &mem, &host, &frame, meta);

                assert_eq!(tagged.tci, Some(TCI), "{what}");
                assert_eq!(plain.tci, None, "{what}");
                assert_eq!(tagged.frame, plain.frame, "{what}");

                // The kernel's own reading of the offsets, with no tag left in the frame.
                let l4_start = 14 + l3_len;
                let seed_field = (shape.csum_offset != 0 && segmenting)
                    .then(|| usize::from(l4_start + shape.csum_offset));
                assert_eq!(
                    with_seed_cleared(tagged.frame.clone(), seed_field),
                    with_seed_cleared(frame.clone(), seed_field),
                    "{what}"
                );
                assert_eq!(tagged.csum_offset, shape.csum_offset, "{what}");
                if shape.csum_offset != 0 {
                    assert_eq!(tagged.csum_start, l4_start, "{what}");
                }
                assert_eq!(tagged.gso_size, meta_gso(segmenting), "{what}");
                assert_eq!(tagged.gso_type, plain.gso_type, "{what}");
                assert_eq!(tagged.flags, plain.flags, "{what}");
                if segmenting {
                    assert_eq!(
                        tagged.hdr_len,
                        l4_start + shape.l4_len as u16,
                        "{what}: the linear part the kernel kept"
                    );
                }
            }
        }
    }

    fn meta_gso(segmenting: bool) -> u16 {
        if segmenting { 1000 } else { 0 }
    }

    /// A priority-only tag says something too, so VID 0 is not "no tag".
    async fn test_tap_tx_inserts_a_priority_only_tag(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");
        disable_ipv6("tap0");
        let host = HostSide::bind("tap0");
        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
            enabled_rx_offloads: Default::default(),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let frame = ip_frame(false, UDP, 8, 64);
        let got = tx_and_read(
            queues[0].as_mut(),
            &mut pool,
            &mem,
            &host,
            &frame,
            TxMetadata {
                vlan: Some(net_backend::VlanMetadata::new().with_priority(3)),
                ..Default::default()
            },
        );
        assert_eq!(got.tci, Some(3 << 13));
        assert_eq!(got.frame, frame);
    }

    /// A guest-supplied frame shorter than an Ethernet header cannot take a tag, and the transmit
    /// path must survive it: this is reachable from a guest.
    async fn test_tap_tx_survives_a_tagged_runt(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");
        disable_ipv6("tap0");
        let host = HostSide::bind("tap0");
        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
            enabled_rx_offloads: Default::default(),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = queues[0].as_mut();
        let vlan = net_backend::VlanMetadata::new().with_vlan_id(5);
        // 14 is the shortest frame that takes the tag. Tagged it is 18 bytes, which the
        // kernel frees in `skb_vlan_untag` for having no bytes past the EtherType, so the
        // host never sees it. Sent untagged, it would be the first thing read below.
        let frame = ip_frame(false, UDP, 8, 64);
        for len in [1u32, 6, 11, 12, 13, 14] {
            mem.write_at(0, &frame[..16]).unwrap();
            let segments = [TxSegment {
                ty: TxSegmentType::Head(TxMetadata {
                    segment_count: 1,
                    len,
                    vlan: Some(vlan),
                    ..Default::default()
                }),
                gpa: 0,
                len,
            }];
            queue.tx_avail(&mut pool, &segments).unwrap();
        }
        // The queue still works, and nothing of the runts arrived.
        let got = tx_and_read(
            queue,
            &mut pool,
            &mem,
            &host,
            &frame,
            TxMetadata {
                vlan: Some(vlan),
                ..Default::default()
            },
        );
        assert_eq!(got.frame, frame);
        assert_eq!(got.tci, Some(5));
    }

    fn fold_checksum(mut sum: u32) -> u16 {
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        sum as u16
    }

    fn sum_words(bytes: &[u8]) -> u32 {
        bytes
            .chunks_exact(2)
            .map(|c| u32::from(u16::from_be_bytes([c[0], c[1]])))
            .sum()
    }

    /// A tagged TSO send as Linux's mana driver makes it, with `l2_len` 14.
    ///
    /// `mana_start_xmit` zeroes `tot_len` and `check` and leaves the tag in the OOB. IP
    /// identification 0x07xx matters: read as an IP header 4 bytes late, its 0x07 is a valid
    /// IHL, and the repair then writes the length over the flags and the checksum over the
    /// source address, leaving the real fields zero.
    async fn test_tap_tx_repairs_a_mana_tso_header_with_an_injected_tag(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");
        disable_ipv6("tap0");
        let host = HostSide::bind("tap0");
        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
            enabled_rx_offloads: Default::default(),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();

        let mut frame = ip_frame(false, TCP, 20, 8000);
        frame[16..18].fill(0);
        frame[18] = 0x07;
        frame[24..26].fill(0);
        let got = tx_and_read(
            queues[0].as_mut(),
            &mut pool,
            &mem,
            &host,
            &frame,
            TxMetadata {
                flags: TxFlags::new()
                    .with_is_ipv4(true)
                    .with_offload_ip_header_checksum(true)
                    .with_offload_tcp_checksum(true)
                    .with_offload_tcp_segmentation(true),
                l2_len: 14,
                l3_len: 20,
                l4_len: 20,
                max_segment_size: 1448,
                vlan: Some(net_backend::VlanMetadata::new().with_vlan_id(201)),
                ..Default::default()
            },
        );

        assert_eq!(got.tci, Some(201));
        let ip = &got.frame[14..34];
        assert_eq!(u16::from_be_bytes([ip[2], ip[3]]), 20 + 20 + 8000);
        assert_eq!(fold_checksum(sum_words(ip)), 0xffff, "header checksum");
        assert_eq!(ip[6..8], frame[20..22], "flags and fragment offset");
        assert_eq!(ip[12..20], frame[26..34], "addresses");
        assert_eq!(
            u16::from_be_bytes([got.frame[50], got.frame[51]]),
            fold_checksum(sum_words(&frame[26..34]) + u32::from(TCP) + 20 + 8000),
            "the TCP seed the kernel's segmentation expects"
        );
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
            enabled_rx_offloads: Default::default(),
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

    /// Validates that an undersized receive buffer remains at the front until
    /// a later frame fits, preserving receive completion order.
    async fn test_tap_rx_reuses_undersized_buffer(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        disable_ipv6("tap0");
        configure_tap("tap0", "10.0.0.1/24");

        let (mut pool, mem) = make_pool();
        let undersized = RxId(1);
        let usable = RxId(2);
        pool.set_capacity(undersized, 64);

        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
            enabled_rx_offloads: Default::default(),
        }];
        let mut queues = Vec::new();
        endpoint
            .get_queues(config, None, &mut queues)
            .await
            .unwrap();
        let queue = &mut queues[0];
        queue.rx_avail(&mut pool, &[undersized, usable]);

        let mut oversized_frame = vec![0xaa; 128];
        oversized_frame[..14].copy_from_slice(&[
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x88, 0xb5,
        ]);
        let mut fitting_frame = vec![0xbb; 60];
        fitting_frame[..14].copy_from_slice(&[
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x88, 0xb5,
        ]);

        send_ethernet_frame("tap0", &oversized_frame);
        send_ethernet_frame("tap0", &fitting_frame);
        poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;

        let mut packets = [RxId(0); 2];
        let n = queue.rx_poll(&mut pool, &mut packets).unwrap();
        assert_eq!(n, 1);
        assert_eq!(packets[0].0, undersized.0);
        assert_eq!(
            pool.rx_metadata(undersized).map(|metadata| metadata.len),
            Some(fitting_frame.len())
        );
        assert!(pool.rx_metadata(usable).is_none());

        let mut readback = vec![0; fitting_frame.len()];
        mem.read_at(undersized.0 as u64 * 2048, &mut readback)
            .unwrap();
        assert_eq!(readback, fitting_frame);

        send_ethernet_frame("tap0", &fitting_frame);
        poll_fn(|cx| queue.poll_ready(cx, &mut pool)).await;
        let n = queue.rx_poll(&mut pool, &mut packets).unwrap();
        assert_eq!(n, 1);
        assert_eq!(packets[0].0, usable.0);
    }

    /// Validates that flooding tx_avail doesn't panic or error — packets are
    /// silently dropped when the kernel buffer fills up.
    async fn test_tap_tx_wouldblock_drops(driver: DefaultDriver) {
        let mut endpoint = new_endpoint("tap0").unwrap();
        configure_tap("tap0", "10.0.0.1/24");

        let (mut pool, mem) = make_pool();
        let config = vec![QueueConfig {
            driver: Box::new(driver.clone()),
            enabled_rx_offloads: Default::default(),
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
            enabled_rx_offloads: Default::default(),
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
            enabled_rx_offloads: Default::default(),
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
            async_trial(
                "tap_tx_inserts_the_vlan_tag",
                test_tap_tx_inserts_the_vlan_tag,
            ),
            async_trial(
                "tap_tx_inserts_a_priority_only_tag",
                test_tap_tx_inserts_a_priority_only_tag,
            ),
            async_trial(
                "tap_tx_survives_a_tagged_runt",
                test_tap_tx_survives_a_tagged_runt,
            ),
            async_trial(
                "tap_tx_repairs_a_mana_tso_header_with_an_injected_tag",
                test_tap_tx_repairs_a_mana_tso_header_with_an_injected_tag,
            ),
            async_trial("tap_rx_receives_packet", test_tap_rx_receives_packet),
            async_trial(
                "tap_rx_reuses_undersized_buffer",
                test_tap_rx_reuses_undersized_buffer,
            ),
            async_trial("tap_tx_wouldblock_drops", test_tap_tx_wouldblock_drops),
            async_trial("tap_tx_with_offloads", test_tap_tx_with_offloads),
            async_trial("tap_tso_ipv4_checksum", test_tap_tso_ipv4_checksum),
        ]
        .map(|t| t.with_ignored_flag(ignored))
        .into();

        libtest_mimic::run(&args, tests).exit();
    }
} // mod tap_tests
