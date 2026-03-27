// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ring::RingBuffer;
use anyhow::Context;
use futures::future::poll_fn;
use hybrid_vsock::VsockPortOrId;
use pal_async::driver::Driver;
use pal_async::driver::PollImpl;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::AsSockRef;
use pal_async::socket::PollSocketReady;
use parking_lot::Mutex;
use std::io;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::prelude::*;
#[cfg(windows)]
use std::os::windows::prelude::*;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
#[cfg(windows)]
use tracing::event;
use unix_socket::UnixStream;
use vmcore::vm_task::VmTaskDriver;

pub struct UnixSocketRelay {
    base_path: PathBuf,
}

impl UnixSocketRelay {
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    pub fn connect(&self, driver: &VmTaskDriver, port: u32) -> anyhow::Result<RelaySocket> {
        let request = VsockPortOrId::Port(port);
        let socket_path = request.host_uds_path(&self.base_path)?;
        let stream = UnixStream::connect(socket_path)
            .context("Failed to connect to Unix socket for vsock relay")?;

        let socket = RelaySocket::new(driver, stream)
            .context("Failed to create relay socket for vsock relay")?;

        Ok(socket)
    }
}

pub struct RelaySocket {
    inner: Arc<RelaySocketInner>,
}

impl RelaySocket {
    pub fn new(driver: &VmTaskDriver, stream: UnixStream) -> io::Result<Self> {
        let sock_ref = stream.as_sock_ref();
        sock_ref.set_nonblocking(true)?;
        #[cfg(unix)]
        let fd = sock_ref.as_raw_fd();
        #[cfg(windows)]
        let fd = sock_ref.as_raw_socket();

        let poll = driver.new_dyn_socket_ready(fd)?;

        Ok(Self {
            inner: Arc::new(RelaySocketInner {
                socket: stream,
                poll: Mutex::new(poll),
                awaiting_read: AtomicBool::new(false),
                awaiting_write: AtomicBool::new(false),
                has_data: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Reads data from the socket. If the operation would block, returns Ok(None) and clears the
    /// cached ready state for the read half.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        // Read and Write require &mut self, but they are actually implemented on &UnixStream, so
        // this translates the Arc into something that will work.
        let socket = &mut &self.inner.socket;
        self.check_would_block(socket.read(buf), InterestSlot::Read)
    }

    /// Reads data from the socket into multiple buffers. If the operation would block, returns
    /// Ok(None) and clears the cached ready state for the socket.
    pub fn read_vectored(&self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<Option<usize>> {
        let socket = &mut &self.inner.socket;
        self.check_would_block(socket.read_vectored(bufs), InterestSlot::Read)
    }

    /// Writes data to the socket. If the operation would block, returns Ok(0) and clears the
    /// cached ready state for the write half.
    pub fn write_vectored(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        let socket = &mut &self.inner.socket;

        // Just return 0 instead of None for the ease of the caller, since for Write that does not
        // mean shutdown.
        self.check_would_block(socket.write_vectored(bufs), InterestSlot::Write)
            .map(|size| size.unwrap_or_default())
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let socket = &mut &self.inner.socket;
        self.check_would_block(socket.write(buf), InterestSlot::Write)
            .map(|size| size.unwrap_or_default())
    }

    /// Writes data from the given ring buffer to the socket. If the operation would block, clears
    /// the cached ready state for the write half and returns Ok(0).
    pub fn write_from_ring(&self, ring: &mut RingBuffer) -> io::Result<usize> {
        let socket = &mut &self.inner.socket;
        self.check_would_block(ring.read_to(socket), InterestSlot::Write)
            .map(|size| size.unwrap_or_default())
    }

    // Helper to handle would block errors for reading/writing.
    fn check_would_block(
        &self,
        result: io::Result<usize>,
        slot: InterestSlot,
    ) -> io::Result<Option<usize>> {
        match result {
            Ok(size) => Ok(Some(size)),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                tracing::trace!("would block");
                self.clear_ready(slot);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        self.inner.socket.shutdown(how)
    }

    pub fn await_read_ready<T: 'static + Send>(
        &self,
        result: T,
    ) -> Option<Pin<Box<dyn Future<Output = T> + Send>>> {
        self.await_read_ready_inner(
            result,
            PollEvents::IN | PollEvents::RDHUP | PollEvents::HUP | PollEvents::ERR,
        )
    }

    pub fn await_close<T: 'static + Send>(
        &self,
        result: T,
    ) -> Option<Pin<Box<dyn Future<Output = T> + Send>>> {
        self.clear_ready(InterestSlot::Read);
        self.await_read_ready_inner(result, PollEvents::HUP | PollEvents::ERR)
    }

    pub fn await_read_ready_inner<T: 'static + Send>(
        &self,
        result: T,
        events: PollEvents,
    ) -> Option<Pin<Box<dyn Future<Output = T> + Send>>> {
        self.await_read_ready_needed()
            .then(|| -> Pin<Box<dyn Future<Output = T> + Send>> {
                let inner = self.inner.clone();
                Box::pin(async move {
                    let events = inner.await_ready(InterestSlot::Read, events).await;

                    // RDHUP means the write side of the socket was shutdown by the peer, so the
                    // next read will return 0, which is handled there.
                    if events.has_in() || events.has_err() || events.has_rdhup() {
                        inner.has_data.store(true, Ordering::Release);
                    }

                    // On Windows, HUP is only sent on abortive disconnect, so RDHUP is also used
                    // to for full shutdown. This means write and read shutdown cannot be
                    // distinguished.
                    // On Linux, HUP means either both sides were shutdown or the socket was closed.
                    #[cfg(windows)]
                    let is_closed = events.has_hup() || events.has_rdhup();
                    #[cfg(not(windows))]
                    let is_closed = events.has_hup() || events.has_err();

                    if is_closed {
                        inner.closed.store(true, Ordering::Release);
                    }

                    result
                })
            })
    }

    pub fn await_write_ready<T: 'static + Send>(
        &self,
        result: T,
    ) -> Option<Pin<Box<dyn Future<Output = T> + Send>>> {
        self.await_write_ready_needed()
            .then(|| -> Pin<Box<dyn Future<Output = T> + Send>> {
                let inner = self.inner.clone();
                Box::pin(async move {
                    let events = inner
                        .await_ready(
                            InterestSlot::Write,
                            PollEvents::OUT | PollEvents::HUP | PollEvents::ERR,
                        )
                        .await;

                    if events.has_hup() {
                        inner.closed.store(true, Ordering::Release);
                    }

                    result
                })
            })
    }

    pub fn clear_ready(&self, slot: InterestSlot) {
        self.inner.poll.lock().clear_socket_ready(slot);
    }

    pub fn has_data(&self) -> bool {
        self.inner.has_data.swap(false, Ordering::AcqRel)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    fn await_read_ready_needed(&self) -> bool {
        !self.inner.awaiting_read.swap(true, Ordering::AcqRel)
    }

    fn await_write_ready_needed(&self) -> bool {
        !self.inner.awaiting_write.swap(true, Ordering::AcqRel)
    }
}

struct RelaySocketInner {
    poll: Mutex<PollImpl<dyn PollSocketReady>>,
    // The UnixStream must not be destructed before PollImp, so this must come after.
    socket: UnixStream,
    awaiting_read: AtomicBool,
    awaiting_write: AtomicBool,
    has_data: AtomicBool,
    closed: AtomicBool,
}

impl RelaySocketInner {
    async fn await_ready(&self, slot: InterestSlot, events: PollEvents) -> PollEvents {
        let events = poll_fn(|cx| {
            let mut poll = self.poll.lock();
            poll.poll_socket_ready(cx, slot, events)
        })
        .await;

        match slot {
            InterestSlot::Read => self.awaiting_read.store(false, Ordering::Release),
            InterestSlot::Write => self.awaiting_write.store(false, Ordering::Release),
        }
        events
    }
}
