// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Credentials of the current process and of Unix-socket peers.

#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::io::Error;
#[cfg(target_os = "linux")]
use std::os::unix::prelude::*;

/// Returns the effective user ID of the current process.
pub fn effective_user_id() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Returns the effective user ID of the peer of a connected Unix socket.
///
/// The kernel captures the peer's credentials once: for a socket returned by
/// `accept(2)`, those the client had when it called `connect(2)`; for a socket
/// that connected to a listener, those the listener had when it called
/// `listen(2)`; and for either socket of a `socketpair(2)` pair, those of the
/// process that created the pair. The result is therefore a snapshot, not
/// necessarily the peer's current identity: it does not change when the peer
/// later changes its credentials or passes its descriptor to another process.
///
/// Authenticate a client only with a connected socket. A listening socket
/// reports the credentials of the process that called `listen(2)`, so this
/// function succeeds for it without identifying any client. It fails with
/// [`io::ErrorKind::InvalidData`] for a socket that records no peer
/// credentials, such as one that is neither connected nor listening, or a
/// datagram socket connected with `connect(2)`.
#[cfg(target_os = "linux")]
pub fn unix_socket_peer_user_id(socket: BorrowedFd<'_>) -> io::Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `socket` is valid for the call and the output pointers refer to
    // writable objects with the supplied sizes.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(Error::last_os_error());
    }
    if length as usize != size_of::<libc::ucred>() {
        return Err(Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an invalid credential length",
        ));
    }
    if credentials.uid == libc::uid_t::MAX {
        return Err(Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned no peer user ID",
        ));
    }
    Ok(credentials.uid)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::io::Write;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;
    use std::os::unix::net::UnixDatagram;
    use std::os::unix::net::UnixStream;
    use test_with_tracing::test;

    #[test]
    fn stream_pair_peers_report_the_effective_user_id() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let user_id = effective_user_id();
        assert_eq!(unix_socket_peer_user_id(left.as_fd()).unwrap(), user_id);
        assert_eq!(unix_socket_peer_user_id(right.as_fd()).unwrap(), user_id);

        // The lookups only borrow the descriptors, so both ends still work.
        let mut buffer = [0; 4];
        left.write_all(b"ping").unwrap();
        right.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"ping");
        right.write_all(b"pong").unwrap();
        left.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"pong");
    }

    #[test]
    fn connected_datagram_without_peer_credentials_is_rejected() {
        // Use an abstract address so that the test does not depend on the
        // filesystem or on the length limit of Unix socket pathnames.
        let name = format!("pal-peer-credentials-{}", std::process::id());
        let address = SocketAddr::from_abstract_name(name).unwrap();
        let _server = UnixDatagram::bind_addr(&address).unwrap();
        let client = UnixDatagram::unbound().unwrap();
        client.connect_addr(&address).unwrap();

        let error = unix_socket_peer_user_id(client.as_fd()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
