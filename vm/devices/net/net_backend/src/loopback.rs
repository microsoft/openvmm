// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Loopback endpoint implementation, which reflects all transmits back as
//! receives.
//!
//! This is useful for testing.

use crate::BufferAccess;
use crate::Endpoint;
use crate::MultiQueueSupport;
use crate::Queue;
use crate::QueueConfig;
use crate::RssConfig;
use crate::RxId;
use crate::RxMetadata;
use crate::TxError;
use crate::TxId;
use crate::TxSegment;
use crate::linearize;
use crate::next_packet;
use crate::packet_count;
use async_trait::async_trait;
use inspect::InspectMut;
use std::collections::VecDeque;
use std::task::Context;
use std::task::Poll;

/// A networking backend that reflects all transmitted packets back as received
/// packets.
#[derive(InspectMut)]
#[inspect(skip)]
pub struct LoopbackEndpoint(());

impl LoopbackEndpoint {
    /// Returns a new endpoint.
    pub fn new() -> Self {
        Self(())
    }
}

#[async_trait]
impl Endpoint for LoopbackEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "loopback"
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        queues.extend(config.into_iter().map(|_config| {
            Box::new(LoopbackQueue {
                rx_avail: VecDeque::new(),
                rx_done: VecDeque::new(),
            }) as _
        }));
        Ok(())
    }

    async fn stop(&mut self) {}

    fn is_ordered(&self) -> bool {
        true
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        MultiQueueSupport {
            max_queues: u16::MAX,
            indirection_table_size: 64,
        }
    }
}

#[derive(InspectMut)]
#[inspect(skip)]
pub struct LoopbackQueue {
    pub(crate) rx_avail: VecDeque<RxId>,
    pub(crate) rx_done: VecDeque<RxId>,
}

impl Queue for LoopbackQueue {
    fn poll_ready(&mut self, _cx: &mut Context<'_>, _pool: &mut dyn BufferAccess) -> Poll<()> {
        if self.rx_done.is_empty() {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }

    fn rx_avail(&mut self, _pool: &mut dyn BufferAccess, done: &[RxId]) {
        tracing::debug!(count = done.len(), "rx_avail");
        self.rx_avail.extend(done);
    }

    fn rx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        packets: &mut [RxId],
    ) -> anyhow::Result<usize> {
        let n = packets.len().min(self.rx_done.len());
        for (d, s) in packets.iter_mut().zip(self.rx_done.drain(..n)) {
            *d = s;
        }
        Ok(n)
    }

    fn tx_avail(
        &mut self,
        pool: &mut dyn BufferAccess,
        mut segments: &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        tracing::debug!(count = packet_count(segments), "tx_avail");
        let mut sent = 0;
        while !segments.is_empty() && !self.rx_avail.is_empty() {
            let (meta, _, _) = next_packet(segments);
            let vlan = meta.vlan;
            let before = segments.len();
            let packet = linearize(pool, &mut segments)?;
            sent += before - segments.len();
            let rx_id = self.rx_avail.pop_front().unwrap();
            pool.write_packet(
                rx_id,
                &RxMetadata {
                    offset: 0,
                    len: packet.len(),
                    vlan,
                    ..Default::default()
                },
                &packet,
            );
            self.rx_done.push_back(rx_id);
        }
        Ok((true, sent))
    }

    fn tx_poll(
        &mut self,
        _pool: &mut dyn BufferAccess,
        _done: &mut [TxId],
    ) -> Result<usize, TxError> {
        Ok(0)
    }
}
