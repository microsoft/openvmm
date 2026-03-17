// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use futures::AsyncWrite;
use futures::future::poll_fn;
use pal_async::driver::Driver;
use pal_async::driver::PollImpl;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::socket::PollSocketReady;
use pal_async::socket::PolledSocket;
use parking_lot::Mutex;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use vmcore::vm_task::VmTaskDriver;

pub struct UnixSocketRelay {
    driver: VmTaskDriver,
    base_path: PathBuf,
}

impl UnixSocketRelay {
    pub fn new(driver: VmTaskDriver, base_path: PathBuf) -> Self {
        Self { driver, base_path }
    }

    pub fn connect(&self, port: u32) -> anyhow::Result<RelaySocket> {
        let socket_path = format!("{}_{}", self.base_path.to_str().unwrap(), port);
        tracing::info!(
            "Connecting to Unix socket for vsock relay: {:?}",
            socket_path
        );
        let stream = UnixStream::connect(socket_path)
            .with_context(|| "Failed to connect to Unix socket for vsock relay")?;
        let socket = RelaySocket::new(&self.driver, stream)
            .with_context(|| "Failed to create relay socket for vsock relay")?;

        Ok(socket)
    }
}

pub struct RelaySocket {
    inner: Arc<RelaySocketInner>,
}

impl RelaySocket {
    pub fn new(driver: &VmTaskDriver, stream: UnixStream) -> io::Result<Self> {
        let poll = driver.new_dyn_socket_ready(stream.as_raw_fd())?;
        Ok(Self {
            inner: Arc::new(RelaySocketInner {
                socket: stream,
                poll: Mutex::new(poll),
                awaiting_read: AtomicBool::new(false),
                awaiting_write: AtomicBool::new(false),
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
                    inner
                        .await_ready(
                            InterestSlot::Read,
                            PollEvents::IN | PollEvents::RDHUP | PollEvents::HUP | PollEvents::ERR,
                        )
                        .await;

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
                    inner
                        .await_ready(
                            InterestSlot::Write,
                            PollEvents::OUT | PollEvents::RDHUP | PollEvents::HUP | PollEvents::ERR,
                        )
                        .await;

                    result
                })
            })
    }

    fn await_read_ready_needed(&self) -> bool {
        !self.inner.awaiting_read.swap(true, Ordering::AcqRel)
    }

    fn await_write_ready_needed(&self) -> bool {
        !self.inner.awaiting_write.swap(true, Ordering::AcqRel)
    }
}

struct RelaySocketInner {
    socket: UnixStream,
    poll: Mutex<PollImpl<dyn PollSocketReady>>,
    awaiting_read: AtomicBool,
    awaiting_write: AtomicBool,
}

impl RelaySocketInner {
    async fn await_ready(self: Arc<Self>, slot: InterestSlot, events: PollEvents) -> PollEvents {
        let events = poll_fn(|cx| self.poll.lock().poll_socket_ready(cx, slot, events)).await;
        match slot {
            InterestSlot::Read => self.awaiting_read.store(false, Ordering::Release),
            InterestSlot::Write => self.awaiting_write.store(false, Ordering::Release),
        }
        events
    }
}
