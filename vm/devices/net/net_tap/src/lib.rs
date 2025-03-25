// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A TAP interface based endpoint.

#![cfg(unix)]
#![expect(missing_docs)]

pub mod resolver;
mod tap;

use async_trait::async_trait;
use futures::io::AsyncRead;
use inspect::InspectMut;
use net_backend::BufferAccess;
use net_backend::Endpoint;
use net_backend::Queue;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::linearize;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("TAP interface error")]
    TapInterface(#[source] tap::Error),
}

/// An endpoint based on a TAP interface.
pub struct TapEndpoint {
    tap: Arc<Mutex<Option<tap::Tap>>>,
}

impl TapEndpoint {
    pub fn new(name: &str) -> Result<Self, Error> {
        let tap = tap::Tap::new(name).map_err(Error::TapInterface)?;
        Ok(Self {
            tap: Arc::new(Mutex::new(Some(tap))),
        })
    }
}

impl InspectMut for TapEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

#[async_trait]
impl Endpoint for TapEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "tap"
    }

    async fn get_queues(
        &mut self,
        mut config: Vec<QueueConfig<'_>>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        assert_eq!(config.len(), 1);
        let config = config.drain(..).next().unwrap();

        queues.push(Box::new(TapQueue::new(
            config.driver.as_ref(),
            self.tap.clone(),
            config.pool,
            config.initial_rx,
        )?));
        Ok(())
    }

    async fn stop(&mut self) {
        assert!(self.tap.lock().is_some(), "queue has not been dropped");
    }

    fn is_ordered(&self) -> bool {
        true
    }
}

struct TapQueue {
    slot: Arc<Mutex<Option<tap::Tap>>>,
    tap: Option<tap::PolledTap>,
    inner: Inner,
    buffer: Box<[u8]>,
}

struct Inner {
    pool: Box<dyn BufferAccess>,
    rx_free: VecDeque<RxId>,
    rx_ready: VecDeque<RxId>,
}

impl InspectMut for TapQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl Drop for TapQueue {
    fn drop(&mut self) {
        if let Some(tap) = self.tap.take() {
            *self.slot.lock() = Some(tap.into_inner());
        }
    }
}

impl TapQueue {
    fn new(
        driver: &dyn Driver,
        slot: Arc<Mutex<Option<tap::Tap>>>,
        pool: Box<dyn BufferAccess>,
        initial_rx: &[RxId],
    ) -> anyhow::Result<Self> {
        let tap = slot.lock().take().expect("queue is already in use");
        let tap = tap.polled(driver)?;
        Ok(Self {
            slot,
            tap: Some(tap),
            inner: Inner {
                pool,
                rx_free: initial_rx.iter().copied().collect(),
                rx_ready: VecDeque::new(),
            },
            buffer: Box::new([0; 65535]),
        })
    }
}

impl Queue for TapQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.inner.rx_ready.is_empty() {
            return Poll::Ready(());
        }

        let tap = if let Some(tap) = self.tap.as_mut() {
            tap
        } else {
            return Poll::Pending;
        };

        while let Some(&rx) = self.inner.rx_free.front() {
            match Pin::new(&mut *tap).poll_read(cx, &mut self.buffer) {
                Poll::Ready(Ok(read_len)) => {
                    self.inner.pool.write_packet(
                        rx,
                        &RxMetadata {
                            offset: 0,
                            len: read_len,
                            ..Default::default()
                        },
                        &self.buffer[..read_len],
                    );

                    self.inner.rx_ready.push_back(rx);
                    self.inner.rx_free.pop_front();
                }
                Poll::Ready(Err(err)) => {
                    tracing::warn!(error = &err as &dyn std::error::Error, "tap rx error");
                    break;
                }
                Poll::Pending => break,
            }
        }

        if !self.inner.rx_ready.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.inner.rx_free.extend(done);
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        // Send to the guest any packets that might have been read during poll_ready().
        let n = std::cmp::min(self.inner.rx_ready.len(), packets.len());
        for (done, id) in packets[..n].iter_mut().zip(self.inner.rx_ready.drain(..n)) {
            *done = id;
        }
        Ok(n)
    }

    fn tx_avail(&mut self, mut segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        let n = segments.len();
        // Synchronously send packets received from the guest to host's network.
        if let Some(tap) = self.tap.as_mut() {
            while !segments.is_empty() {
                let packet = linearize(self.inner.pool.as_ref(), &mut segments)?;
                match tap.write(&packet) {
                    Ok(bytes_written) => {
                        assert_eq!(
                            bytes_written,
                            packet.len(),
                            "TAP should never partial write"
                        );
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        // dropped packet: buffer is full

                        // TODO: return partial transmit here. This relies on
                        // remembering this condition and polling for POLLOUT in
                        // poll_ready().
                    }
                    Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                        // dropped packet: interface is not up
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = &err as &dyn std::error::Error,
                            "write to TAP interface failed"
                        );
                    }
                }
            }
        }
        let completed_synchronously = true;
        Ok((completed_synchronously, n))
    }

    fn tx_poll(&mut self, _done: &mut [TxId]) -> anyhow::Result<usize> {
        // Packets are sent synchronously so there is no no need to check here if
        // sending has been completed.
        Ok(0)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
        Some(self.inner.pool.as_mut())
    }
}
