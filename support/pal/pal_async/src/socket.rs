// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Socket-related functionality.

#[cfg(unix)]
use super::fd;
use super::interest::InterestSlot;
use super::interest::PollEvents;
use crate::driver::Driver;
use crate::driver::PollImpl;
use futures::AsyncRead;
use futures::AsyncWrite;
use parking_lot::Mutex;
use std::fmt::Debug;
use std::future::poll_fn;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::prelude::*;
#[cfg(windows)]
use std::os::windows::prelude::*;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use unix_socket::UnixStream;

/// A trait for driving socket ready polling.
pub trait SocketReadyDriver: Unpin {
    /// The socket ready type.
    type SocketReady: 'static + PollSocketReady;

    /// Creates a new object for polling socket readiness.
    #[cfg(windows)]
    fn new_socket_ready(&self, socket: RawSocket) -> io::Result<Self::SocketReady>;
    /// Creates a new object for polling socket readiness.
    #[cfg(unix)]
    fn new_socket_ready(&self, socket: RawFd) -> io::Result<Self::SocketReady>;
}

#[cfg(unix)]
impl<T: fd::FdReadyDriver> SocketReadyDriver for T {
    type SocketReady = <Self as fd::FdReadyDriver>::FdReady;

    fn new_socket_ready(&self, socket: RawFd) -> io::Result<Self::SocketReady> {
        self.new_fd_ready(socket)
    }
}

/// A trait for polling socket readiness.
pub trait PollSocketReady: Unpin + Send + Sync {
    /// Polls a socket for readiness.
    fn poll_socket_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents>;

    /// Clears cached socket readiness so that the next call to
    /// `poll_socket_ready` will poll the OS again.
    fn clear_socket_ready(&mut self, slot: InterestSlot);
}

#[cfg(unix)]
impl<T: fd::PollFdReady> PollSocketReady for T {
    fn poll_socket_ready(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
    ) -> Poll<PollEvents> {
        self.poll_fd_ready(cx, slot, events)
    }

    fn clear_socket_ready(&mut self, slot: InterestSlot) {
        self.clear_fd_ready(slot)
    }
}

/// A polled socket.
pub struct PolledSocket<T> {
    poll: PollImpl<dyn PollSocketReady>, // must be first--some executors require that it's dropped before socket.
    socket: T,
}

/// Trait implemented by socket types.
pub trait AsSockRef: Unpin {
    /// Returns a socket reference.
    fn as_sock_ref(&self) -> socket2::SockRef<'_>;
}

impl<T: Unpin> AsSockRef for T
where
    for<'a> &'a T: Into<socket2::SockRef<'a>>,
{
    fn as_sock_ref(&self) -> socket2::SockRef<'_> {
        self.into()
    }
}

impl<T: AsSockRef> PolledSocket<T> {
    /// Creates a new polled socket.
    pub fn new(driver: &(impl ?Sized + Driver), socket: T) -> io::Result<Self> {
        let sock_ref = socket.as_sock_ref();
        sock_ref.set_nonblocking(true)?;
        #[cfg(windows)]
        let fd = sock_ref.as_raw_socket();
        #[cfg(unix)]
        let fd = sock_ref.as_raw_fd();
        Ok(Self {
            poll: driver.new_dyn_socket_ready(fd)?,
            socket,
        })
    }

    /// Extracts the inner socket.
    pub fn into_inner(self) -> T {
        let sock_ref = self.socket.as_sock_ref();
        sock_ref.set_nonblocking(false).unwrap();
        self.socket
    }
}

impl<T> PolledSocket<T> {
    /// Gets a reference to the inner socket.
    pub fn get(&self) -> &T {
        &self.socket
    }

    /// Gets a mutable reference to the inner socket.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.socket
    }

    /// Converts the inner socket type.
    pub fn convert<T2: From<T>>(self) -> PolledSocket<T2> {
        PolledSocket {
            socket: T2::from(self.socket),
            poll: self.poll,
        }
    }
}

/// Trait for objects that can be polled for readiness.
pub trait PollReady {
    /// Polls an object for readiness.
    fn poll_ready(&mut self, cx: &mut Context<'_>, events: PollEvents) -> Poll<PollEvents>;
}

/// Extension methods for implementations of [`PollReady`].
pub trait PollReadyExt {
    /// Waits for a socket or file to hang up.
    fn wait_ready(&mut self, events: PollEvents) -> Ready<'_, Self>
    where
        Self: Unpin + Sized;
}

impl<T: PollReady + Unpin> PollReadyExt for T {
    fn wait_ready(&mut self, events: PollEvents) -> Ready<'_, Self>
    where
        Self: Unpin + Sized,
    {
        Ready(self, events)
    }
}

/// Future for [`PollReadyExt::wait_ready`].
pub struct Ready<'a, T>(&'a mut T, PollEvents);

impl<T: Unpin + PollReady> Future for Ready<'_, T> {
    type Output = PollEvents;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.0.poll_ready(cx, this.1)
    }
}

impl<T> PolledSocket<T> {
    /// Calls nonblocking operation `f` when the socket has least one event in
    /// `events` ready.
    ///
    /// Uses interest slot `slot` to allow multiple concurrent operations.
    ///
    /// If `f` returns `Err(err)` with `err.kind() ==
    /// io::ErrorKind::WouldBlock`, then this re-polls the socket for readiness
    /// and returns `Poll::Pending`.
    pub fn poll_io<F, R>(
        &mut self,
        cx: &mut Context<'_>,
        slot: InterestSlot,
        events: PollEvents,
        mut f: F,
    ) -> Poll<io::Result<R>>
    where
        F: FnMut(&mut Self) -> io::Result<R>,
    {
        loop {
            std::task::ready!(self.poll.poll_socket_ready(cx, slot, events));
            match f(self) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.poll.clear_socket_ready(slot);
                }
                r => break Poll::Ready(r),
            }
        }
    }
}

impl<T: AsSockRef> PollReady for PolledSocket<T> {
    fn poll_ready(&mut self, cx: &mut Context<'_>, events: PollEvents) -> Poll<PollEvents> {
        self.poll.poll_socket_ready(cx, InterestSlot::Read, events)
    }
}

impl<T> PolledSocket<T>
where
    T: AsSockRef + Read + Write,
{
    /// Splits the socket into a read and write half that can be used
    /// concurrently.
    ///
    /// This is more flexible and efficient than
    /// [`futures::io::AsyncReadExt::split`], since it avoids holding a lock
    /// while calling into the kernel, and it provides access to the underlying
    /// socket for more advanced operations.
    pub fn split(self) -> (ReadHalf<T>, WriteHalf<T>) {
        let inner = Arc::new(SplitInner {
            poll: Mutex::new(self.poll),
            socket: self.socket,
        });
        (
            ReadHalf {
                inner: inner.clone(),
            },
            WriteHalf { inner },
        )
    }
}

fn is_connect_incomplete_error(err: &io::Error) -> bool {
    // This handles the Windows and AF_UNIX case.
    if err.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    // This handles the remaining cases on Linux.
    #[cfg(unix)]
    if err.raw_os_error() == Some(libc::EINPROGRESS) {
        return true;
    }
    false
}

impl PolledSocket<socket2::Socket> {
    /// Connects the socket to address `addr`.
    pub async fn connect(&mut self, addr: &socket2::SockAddr) -> io::Result<()> {
        match self.socket.connect(addr) {
            Ok(()) => Ok(()),
            Err(err) if is_connect_incomplete_error(&err) => {
                self.poll.clear_socket_ready(InterestSlot::Write);
                poll_fn(|cx| {
                    self.poll
                        .poll_socket_ready(cx, InterestSlot::Write, PollEvents::OUT)
                })
                .await;
                if let Some(err) = self.socket.take_error()? {
                    return Err(err);
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
}

impl PolledSocket<UnixStream> {
    /// Creates a new connected Unix stream socket.
    pub async fn connect_unix(
        driver: &(impl ?Sized + Driver),
        addr: impl AsRef<Path>,
    ) -> io::Result<Self> {
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        let mut socket = PolledSocket::new(driver, socket)?;
        socket
            .connect(&socket2::SockAddr::unix(addr.as_ref())?)
            .await?;
        Ok(socket.convert())
    }
}

impl<T: AsSockRef + Read> AsyncRead for PolledSocket<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Read, PollEvents::IN, |this| {
            this.socket.read(buf)
        })
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Read, PollEvents::IN, |this| {
            this.socket.read_vectored(bufs)
        })
    }
}

impl<T: AsSockRef + Write> AsyncWrite for PolledSocket<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |this| {
            this.socket.write(buf)
        })
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |this| {
            this.socket.flush()
        })
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.socket.as_sock_ref().shutdown(Shutdown::Write))
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, InterestSlot::Write, PollEvents::OUT, |this| {
            this.socket.write_vectored(bufs)
        })
    }
}

/// Trait for listening sockets.
pub trait Listener: AsSockRef {
    /// The socket type.
    type Socket: AsSockRef + Read + Write + Into<socket2::Socket>;
    /// The socket address type.
    type Address: Debug;

    /// Accepts an incoming socket.
    fn accept(&self) -> io::Result<(Self::Socket, Self::Address)>;
    /// Returns the local address of the listener.
    fn local_addr(&self) -> io::Result<Self::Address>;
}

impl<'a, T> Listener for &'a T
where
    T: Listener,
    &'a T: AsSockRef,
{
    type Socket = T::Socket;
    type Address = T::Address;

    fn accept(&self) -> io::Result<(Self::Socket, Self::Address)> {
        (**self).accept()
    }

    fn local_addr(&self) -> io::Result<Self::Address> {
        (**self).local_addr()
    }
}

macro_rules! listener {
    ($ty:ty, $socket:ty, $addr:ty) => {
        impl Listener for $ty {
            type Socket = $socket;
            type Address = $addr;
            fn accept(&self) -> io::Result<(Self::Socket, Self::Address)> {
                <$ty>::accept(self)
            }
            fn local_addr(&self) -> io::Result<Self::Address> {
                <$ty>::local_addr(self)
            }
        }
    };
}

listener!(
    std::net::TcpListener,
    std::net::TcpStream,
    std::net::SocketAddr
);

#[cfg(unix)]
listener!(
    unix_socket::UnixListener,
    UnixStream,
    std::os::unix::net::SocketAddr
);

#[cfg(windows)]
impl Listener for unix_socket::UnixListener {
    type Socket = UnixStream;
    type Address = ();

    fn accept(&self) -> io::Result<(Self::Socket, Self::Address)> {
        self.accept()
    }

    fn local_addr(&self) -> io::Result<Self::Address> {
        Ok(())
    }
}

listener!(socket2::Socket, socket2::Socket, socket2::SockAddr);

impl PolledSocket<socket2::Socket> {
    /// Listens for incoming connections.
    pub fn listen(&self, backlog: i32) -> io::Result<()> {
        self.socket.listen(backlog)
    }
}

impl<T: Listener> PolledSocket<T> {
    /// Polls for a new connection.
    pub fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<(T::Socket, T::Address)>> {
        self.poll_io(cx, InterestSlot::Read, PollEvents::IN, |this| {
            this.socket.accept()
        })
    }

    /// Accepts a new connection.
    pub async fn accept(&mut self) -> io::Result<(T::Socket, T::Address)> {
        poll_fn(|cx| self.poll_accept(cx)).await
    }
}

struct SplitInner<T> {
    poll: Mutex<PollImpl<dyn PollSocketReady>>, // must be first--some executors require that it's dropped before socket.
    socket: T,
}

/// The read half of a socket, via [`PolledSocket::split`].
pub struct ReadHalf<T> {
    inner: Arc<SplitInner<T>>,
}

impl<T> ReadHalf<T> {
    /// Gets a reference to the inner socket.
    pub fn get(&self) -> &T {
        &self.inner.socket
    }

    /// Calls nonblocking operation `f` when the socket is ready for read.
    ///
    /// If `f` returns `Err(err)` with `err.kind() ==
    /// io::ErrorKind::WouldBlock`, then this re-polls the socket for readiness
    /// and returns `Poll::Pending`.
    pub fn poll_io<F, R>(&mut self, cx: &mut Context<'_>, mut f: F) -> Poll<io::Result<R>>
    where
        F: FnMut(&mut Self) -> io::Result<R>,
    {
        loop {
            std::task::ready!(self.inner.poll.lock().poll_socket_ready(
                cx,
                InterestSlot::Read,
                PollEvents::IN
            ));
            match f(self) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.inner
                        .poll
                        .lock()
                        .clear_socket_ready(InterestSlot::Read);
                }
                r => break Poll::Ready(r),
            }
        }
    }
}

/// The write half of a socket, via [`PolledSocket::split`].
pub struct WriteHalf<T> {
    inner: Arc<SplitInner<T>>,
}

impl<T> WriteHalf<T> {
    /// Gets a reference to the inner socket.
    pub fn get(&self) -> &T {
        &self.inner.socket
    }

    /// Calls nonblocking operation `f` when the socket is ready for write.
    ///
    /// If `f` returns `Err(err)` with `err.kind() ==
    /// io::ErrorKind::WouldBlock`, then this re-polls the socket for readiness
    /// and returns `Poll::Pending`.
    pub fn poll_io<F, R>(&mut self, cx: &mut Context<'_>, mut f: F) -> Poll<io::Result<R>>
    where
        F: FnMut(&mut Self) -> io::Result<R>,
    {
        loop {
            std::task::ready!(self.inner.poll.lock().poll_socket_ready(
                cx,
                InterestSlot::Write,
                PollEvents::OUT
            ));
            match f(self) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.inner
                        .poll
                        .lock()
                        .clear_socket_ready(InterestSlot::Write);
                }
                r => break Poll::Ready(r),
            }
        }
    }
}

impl<T: AsSockRef> PollReady for ReadHalf<T> {
    fn poll_ready(&mut self, cx: &mut Context<'_>, events: PollEvents) -> Poll<PollEvents> {
        self.inner
            .poll
            .lock()
            .poll_socket_ready(cx, InterestSlot::Read, events)
    }
}

impl<T: AsSockRef> AsyncRead for ReadHalf<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| (&*this.inner.socket.as_sock_ref()).read(buf))
    }

    fn poll_read_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| {
            (&*this.inner.socket.as_sock_ref()).read_vectored(bufs)
        })
    }
}

impl<T: AsSockRef> PollReady for WriteHalf<T> {
    fn poll_ready(&mut self, cx: &mut Context<'_>, events: PollEvents) -> Poll<PollEvents> {
        self.inner
            .poll
            .lock()
            .poll_socket_ready(cx, InterestSlot::Write, events)
    }
}

impl<T: AsSockRef> AsyncWrite for WriteHalf<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| (&*this.inner.socket.as_sock_ref()).write(buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_io(cx, |this| (&*this.inner.socket.as_sock_ref()).flush())
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.inner.socket.as_sock_ref().shutdown(Shutdown::Write))
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_io(cx, |this| {
            (&*this.inner.socket.as_sock_ref()).write_vectored(bufs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PolledSocket;
    use crate::DefaultDriver;
    use futures::AsyncReadExt;
    use futures::AsyncWriteExt;
    use pal_async_test::async_test;
    use unix_socket::UnixStream;

    #[async_test]
    async fn split(driver: DefaultDriver) {
        let (a, b) = UnixStream::pair().unwrap();
        let a = PolledSocket::new(&driver, a).unwrap();
        let b = PolledSocket::new(&driver, b).unwrap();
        let (mut ar, mut aw) = a.split();
        let (br, mut bw) = b.split();
        let copy = async {
            futures::io::copy(br, &mut bw).await.unwrap();
            bw.close().await.unwrap();
        };
        let rest = async {
            aw.write_all(b"abc").await.unwrap();
            let mut v = vec![0; 3];
            ar.read_exact(&mut v).await.unwrap();
            aw.write_all(b"def").await.unwrap();
            aw.close().await.unwrap();
            ar.read_to_end(&mut v).await.unwrap();
            assert_eq!(&v, b"abcdef");
        };
        futures::future::join(copy, rest).await;
    }
}
