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
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;
    use std::os::unix::net::UnixDatagram;
    use test_with_tracing::test;

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
