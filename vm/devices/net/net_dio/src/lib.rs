// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An endpoint built on the vmswitch DirectIO interface.

#![cfg(windows)]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::Context as _;
use async_trait::async_trait;
use inspect::InspectMut;
use net_backend::BufferAccess;
use net_backend::Endpoint;
use net_backend::Queue;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::next_packet;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use std::io::ErrorKind;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use vmswitch::dio;

/// An endpoint that uses vmswitch's DirectIO interface to plug in to a
/// switch.
pub struct DioEndpoint {
    nic: Arc<Mutex<Option<dio::DioNic>>>,
}

impl DioEndpoint {
    pub fn new(nic: dio::DioNic) -> Self {
        Self {
            nic: Arc::new(Mutex::new(Some(nic))),
        }
    }
}

impl InspectMut for DioEndpoint {
    fn inspect_mut(&mut self, _req: inspect::Request<'_>) {
        // TODO
    }
}

#[async_trait]
impl Endpoint for DioEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "dio"
    }

    async fn get_queues(
        &mut self,
        mut config: Vec<QueueConfig<'_>>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        assert_eq!(config.len(), 1);
        let config = config.drain(..).next().unwrap();
        queues.push(Box::new(DioQueue::new(
            &config.driver,
            self.nic.clone(),
            config.pool,
            config.initial_rx,
        )));
        Ok(())
    }

    async fn stop(&mut self) {
        assert!(self.nic.lock().is_some(), "the queue has not been dropped");
    }
}

/// A DirectIO queue.
pub struct DioQueue {
    slot: Arc<Mutex<Option<dio::DioNic>>>,
    nic: Option<dio::DioQueue>,
    free: Vec<RxId>,
    rx_pool: Box<dyn BufferAccess>,
}

impl InspectMut for DioQueue {
    fn inspect_mut(&mut self, _req: inspect::Request<'_>) {
        // TODO
    }
}

impl Drop for DioQueue {
    fn drop(&mut self) {
        // Return the NIC to the endpoint.
        *self.slot.lock() = self.nic.take().map(|x| x.into_inner())
    }
}

impl DioQueue {
    fn new(
        driver: &(impl ?Sized + Driver),
        slot: Arc<Mutex<Option<dio::DioNic>>>,
        rx_pool: Box<dyn BufferAccess>,
        initial_rx: &[RxId],
    ) -> Self {
        let nic = slot.lock().take();
        Self {
            slot,
            nic: nic.map(|nic| dio::DioQueue::new(driver, nic)),
            free: initial_rx.to_vec(),
            rx_pool,
        }
    }
}

impl Queue for DioQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(nic) = &mut self.nic {
            nic.poll_read_ready(cx)
        } else {
            Poll::Pending
        }
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.free.extend(done);
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        let mut n_packets = 0;
        if let Some(nic) = &mut self.nic {
            // Transmit incoming packets to the guest until there are no more available.
            for done_id in packets {
                let id = if let Some(&id) = self.free.last() {
                    id
                } else {
                    break;
                };
                let result = nic.read_with(|buf| {
                    self.rx_pool.write_packet(
                        id,
                        &RxMetadata {
                            offset: 0,
                            len: buf.len(),
                            ..Default::default()
                        },
                        buf,
                    );
                });
                match result {
                    Ok(()) => self.free.pop(),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        // The DIO endpoint is in a bad state.
                        //
                        // Disconnect the NIC, but do not fail the operation
                        // since that would indicate a guest error.
                        tracing::error!(error = &e as &dyn std::error::Error, "dio error");
                        self.nic = None;
                        break;
                    }
                };
                *done_id = id;
                n_packets += 1;
            }
        }
        Ok(n_packets)
    }

    fn tx_avail(&mut self, mut segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        let n = segments.len();
        if let Some(nic) = &mut self.nic {
            let mem = self.rx_pool.guest_memory();
            while !segments.is_empty() {
                let (metadata, this, rest) = next_packet(segments);
                segments = rest;
                nic.write_with(metadata.len as usize, |mut buf| -> anyhow::Result<_> {
                    for segment in this {
                        let (this, rest) = buf.split_at_mut(segment.len as usize);
                        mem.read_at(segment.gpa, this)
                            .context("failed to write guest memory")?;
                        buf = rest;
                    }
                    Ok(())
                })
                .unwrap_or(Ok(()))?;
            }
        }
        Ok((true, n))
    }

    fn tx_poll(&mut self, _done: &mut [TxId]) -> Result<usize, TxError> {
        Ok(0)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
        Some(self.rx_pool.as_mut())
    }
}
