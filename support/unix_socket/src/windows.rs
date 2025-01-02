// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements Unix socket wrappers for Windows, since the ones in
//! `std::os::unix::net` are only available on Unix.

#![cfg(windows)]
// UNSAFETY: needed to call platform APIs.
#![expect(unsafe_code)]

use socket2::Socket;
use std::io;
use std::net::Shutdown;
use std::os::windows::prelude::*;
use std::path::Path;
use windows_sys::Win32::Networking::WinSock;

/// Connected AF_UNIX stream socket.
#[derive(Debug)]
pub struct UnixStream(Socket);

#[cfg(feature = "mesh")]
mesh_protobuf::os_resource!(UnixStream, OwnedSocket);

impl UnixStream {
    /// Returns a new socket connected to the specified path.
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let s = Self::socket()?;
        s.connect(&socket2::SockAddr::unix(path.as_ref())?)?;
        Ok(Self(s))
    }

    fn socket() -> io::Result<Socket> {
        Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
    }

    /// Creates a connected pair of Unix sockets.
    pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
        // Generate a random path to bind to.
        let mut path = std::env::temp_dir();
        let mut n = [0; 16];
        getrandom::getrandom(&mut n).unwrap();
        path.push(format!("{:x}", u128::from_ne_bytes(n)));
        let listener = UnixListener::bind(&path)?;

        // Perform a non-blocking accept and connects. The accept should
        // complete immediately.
        listener.set_nonblocking(true)?;
        let client = Self::socket()?;
        client.set_nonblocking(true)?;
        match client.connect(&socket2::SockAddr::unix(&path)?) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
            Err(err) => return Err(err),
        }
        std::fs::remove_file(&path)?;
        let (server, _) = listener.accept()?;

        if !poll_out_ready(&client)? {
            return Err(io::Error::from_raw_os_error(WinSock::WSAEWOULDBLOCK));
        }
        if let Some(err) = client.take_error()? {
            return Err(err);
        }
        // Clear the nonblocking states.
        server.set_nonblocking(false)?;
        client.set_nonblocking(false)?;
        Ok((server, Self(client)))
    }

    /// Sets the nonblocking state for the socket.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.0.set_nonblocking(nonblocking)
    }

    /// Shuts down the read, write, or both ends of the socket.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.shutdown(how)
    }
}

fn poll_out_ready(socket: &Socket) -> io::Result<bool> {
    let mut pollfd = WinSock::WSAPOLLFD {
        fd: socket.as_raw_socket() as usize,
        events: WinSock::POLLOUT,
        revents: 0,
    };
    // SAFETY: calling winsock APIs according to docs.
    unsafe {
        let r = WinSock::WSAPoll(&mut pollfd, 1, 0);
        if r < 0 {
            return Err(io::Error::from_raw_os_error(WinSock::WSAGetLastError()));
        }
        Ok(r > 0)
    }
}

impl AsSocket for UnixStream {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.0.as_socket()
    }
}

impl From<OwnedSocket> for UnixStream {
    fn from(socket: OwnedSocket) -> Self {
        Self(socket.into())
    }
}

impl From<UnixStream> for OwnedSocket {
    fn from(s: UnixStream) -> Self {
        s.0.into()
    }
}

impl From<Socket> for UnixStream {
    fn from(socket: Socket) -> Self {
        Self(socket)
    }
}

impl From<UnixStream> for Socket {
    fn from(s: UnixStream) -> Self {
        s.0
    }
}

impl io::Read for &'_ UnixStream {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(data)
    }

    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        (&self.0).read_vectored(bufs)
    }
}

impl io::Write for &'_ UnixStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        (&self.0).write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        (&self.0).write_vectored(bufs)
    }
}

impl io::Read for UnixStream {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        (&self.0).read(data)
    }

    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        (&self.0).read_vectored(bufs)
    }
}

impl io::Write for UnixStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        (&self.0).write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&self.0).flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        (&self.0).write_vectored(bufs)
    }
}

/// Listener for Unix stream sockets.
#[derive(Debug)]
pub struct UnixListener(Socket);

#[cfg(feature = "mesh")]
mesh_protobuf::os_resource!(UnixListener, OwnedSocket);

impl UnixListener {
    /// Returns a new listener bound to the specified path.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let s = Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        s.bind(&socket2::SockAddr::unix(path.as_ref())?)?;
        s.listen(128)?;
        Ok(Self(s))
    }

    /// Sets the listener's nonblocking state (which affects the behavior
    /// of accept).
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.0.set_nonblocking(nonblocking)
    }

    /// Accepts a connection.
    ///
    /// Returns a tuple to match the behavior of std::net::UnixListener (TODO:
    /// actually return the peer address instead of `()`).
    pub fn accept(&self) -> io::Result<(UnixStream, ())> {
        let (s, _) = self.0.accept()?;
        Ok((UnixStream(s), ()))
    }
}

impl AsSocket for UnixListener {
    fn as_socket(&self) -> BorrowedSocket<'_> {
        self.0.as_socket()
    }
}

impl From<OwnedSocket> for UnixListener {
    fn from(socket: OwnedSocket) -> Self {
        Self(socket.into())
    }
}

impl From<UnixListener> for OwnedSocket {
    fn from(s: UnixListener) -> Self {
        s.0.into()
    }
}

impl From<Socket> for UnixListener {
    fn from(socket: Socket) -> Self {
        Self(socket)
    }
}

impl From<UnixListener> for Socket {
    fn from(s: UnixListener) -> Self {
        s.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use io::Read;
    use io::Write;

    #[test]
    fn test_pair() {
        let (_s, _c) = UnixStream::pair().unwrap();
    }

    #[test]
    fn test_io() {
        let (mut s, mut c) = UnixStream::pair().unwrap();
        s.write_all(b"abc").unwrap();
        let mut v = [0; 3];
        c.read_exact(&mut v).unwrap();
        assert_eq!(&v, b"abc");
    }
}
