// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use futures::future::poll_fn;
use hybrid_vsock::ConnectionRequest;
use pal_async::driver::Driver;
use pal_async::driver::PollImpl;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::AsSockRef;
use pal_async::socket::PollSocketReady;
use parking_lot::Mutex;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use vmcore::vm_task::VmTaskDriver;

pub struct UnixSocketRelay {
    base_path: PathBuf,
}

impl UnixSocketRelay {
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    pub fn connect(&self, driver: &VmTaskDriver, port: u32) -> anyhow::Result<RelaySocket> {
        let request = ConnectionRequest::Port(port);
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
        let poll = driver.new_dyn_socket_ready(sock_ref.as_raw_fd())?;
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

    pub fn get(&self) -> &UnixStream {
        &self.inner.socket
    }

    pub fn await_read_ready<T: 'static + Send>(
        &self,
        result: T,
    ) -> Option<Pin<Box<dyn Future<Output = T> + Send>>> {
        self.await_read_ready_needed()
            .then(|| -> Pin<Box<dyn Future<Output = T> + Send>> {
                let inner = self.inner.clone();
                Box::pin(async move {
                    let events = inner
                        .await_ready(
                            InterestSlot::Read,
                            PollEvents::IN | PollEvents::RDHUP | PollEvents::HUP | PollEvents::ERR,
                        )
                        .await;

                    if events.has_in() | events.has_err() | events.has_rdhup() {
                        inner.has_data.store(true, Ordering::Release);
                    }

                    if events.has_hup() {
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
