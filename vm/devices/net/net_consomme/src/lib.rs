// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod resolver;

use async_trait::async_trait;
use consomme::ChecksumState;
use consomme::Consomme;
use consomme::ConsommeControl;
use consomme::ConsommeState;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use net_backend::BufferAccess;
use net_backend::L4Protocol;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxChecksumState;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxId;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use pal_async::driver::Driver;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

pub struct ConsommeEndpoint {
    consomme: Arc<Mutex<Option<Consomme>>>,
}

impl ConsommeEndpoint {
    pub fn new() -> Result<Self, consomme::Error> {
        Ok(Self {
            consomme: Arc::new(Mutex::new(Some(Consomme::new()?))),
        })
    }

    pub fn new_with_state(state: ConsommeState) -> Self {
        Self {
            consomme: Arc::new(Mutex::new(Some(Consomme::new_with_state(state)))),
        }
    }

    pub fn new_dynamic(state: ConsommeState) -> (Self, ConsommeControl) {
        let (consomme, control) = Consomme::new_dynamic(state);
        (
            Self {
                consomme: Arc::new(Mutex::new(Some(consomme))),
            },
            control,
        )
    }
}

impl InspectMut for ConsommeEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        if let Some(consomme) = &mut *self.consomme.lock() {
            consomme.inspect_mut(req);
        }
    }
}

#[async_trait]
impl net_backend::Endpoint for ConsommeEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "consomme"
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn net_backend::Queue>>,
    ) -> anyhow::Result<()> {
        assert_eq!(config.len(), 1);
        let config = config.into_iter().next().unwrap();
        let mut queue = Box::new(ConsommeQueue {
            slot: self.consomme.clone(),
            consomme: self.consomme.lock().take(),
            state: QueueState {
                pool: config.pool,
                rx_avail: config.initial_rx.iter().copied().collect(),
                rx_ready: VecDeque::new(),
                tx_avail: VecDeque::new(),
                tx_ready: VecDeque::new(),
            },
            stats: Default::default(),
            driver: config.driver,
        });
        queue.with_consomme(|c| c.refresh_driver());
        queues.push(queue);
        Ok(())
    }

    async fn stop(&mut self) {
        assert!(self.consomme.lock().is_some());
    }

    fn is_ordered(&self) -> bool {
        true
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport {
            ipv4_header: true,
            tcp: true,
            udp: true,
            tso: true,
        }
    }
}

pub struct ConsommeQueue {
    slot: Arc<Mutex<Option<Consomme>>>,
    consomme: Option<Consomme>,
    state: QueueState,
    stats: Stats,
    driver: Box<dyn Driver>,
}

impl InspectMut for ConsommeQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .merge(self.consomme.as_mut().unwrap())
            .field("rx_avail", self.state.rx_avail.len())
            .field("rx_ready", self.state.rx_ready.len())
            .field("tx_avail", self.state.tx_avail.len())
            .field("tx_ready", self.state.tx_ready.len())
            .field("stats", &self.stats);
    }
}

impl Drop for ConsommeQueue {
    fn drop(&mut self) {
        *self.slot.lock() = self.consomme.take();
    }
}

impl ConsommeQueue {
    fn with_consomme<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut consomme::Access<'_, Client<'_>>) -> R,
    {
        f(&mut self.consomme.as_mut().unwrap().access(&mut Client {
            state: &mut self.state,
            stats: &mut self.stats,
            driver: &self.driver,
        }))
    }
}

impl net_backend::Queue for ConsommeQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while let Some(head) = self.state.tx_avail.front() {
            let TxSegmentType::Head(meta) = &head.ty else {
                unreachable!()
            };
            let tx_id = meta.id;
            let checksum = ChecksumState {
                ipv4: meta.offload_ip_header_checksum,
                tcp: meta.offload_tcp_checksum,
                udp: meta.offload_udp_checksum,
                tso: meta
                    .offload_tcp_segmentation
                    .then_some(meta.max_tcp_segment_size),
            };

            let mut buf = vec![0; meta.len];
            let gm = self.state.pool.guest_memory();
            let mut offset = 0;
            for segment in self.state.tx_avail.drain(..meta.segment_count) {
                let dest = &mut buf[offset..offset + segment.len as usize];
                if let Err(err) = gm.read_at(segment.gpa, dest) {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "memory write failure"
                    );
                }
                offset += segment.len as usize;
            }

            if let Err(err) = self.with_consomme(|c| c.send(&buf, &checksum)) {
                tracing::debug!(error = &err as &dyn std::error::Error, "tx packet ignored");
                match err {
                    consomme::DropReason::SendBufferFull => self.stats.tx_dropped.increment(),
                    consomme::DropReason::UnsupportedEthertype(_)
                    | consomme::DropReason::UnsupportedIpProtocol(_)
                    | consomme::DropReason::UnsupportedDhcp(_)
                    | consomme::DropReason::UnsupportedArp => self.stats.tx_unknown.increment(),
                    consomme::DropReason::Packet(_)
                    | consomme::DropReason::Ipv4Checksum
                    | consomme::DropReason::Io(_)
                    | consomme::DropReason::BadTcpState(_) => self.stats.tx_errors.increment(),
                    consomme::DropReason::PortNotBound => unreachable!(),
                }
            }

            self.state.tx_ready.push_back(tx_id);
        }

        self.with_consomme(|c| c.poll(cx));

        if !self.state.tx_ready.is_empty() || !self.state.rx_ready.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.state.rx_avail.extend(done);
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        let n = packets.len().min(self.state.rx_ready.len());
        for (x, y) in packets.iter_mut().zip(self.state.rx_ready.drain(..n)) {
            *x = y;
        }
        Ok(n)
    }

    fn tx_avail(&mut self, segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        self.state.tx_avail.extend(segments.iter().cloned());
        Ok((false, segments.len()))
    }

    fn tx_poll(&mut self, done: &mut [TxId]) -> anyhow::Result<usize> {
        let n = done.len().min(self.state.tx_ready.len());
        for (x, y) in done.iter_mut().zip(self.state.tx_ready.drain(..n)) {
            *x = y;
        }
        Ok(n)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
        Some(self.state.pool.as_mut())
    }
}

struct QueueState {
    pool: Box<dyn BufferAccess>,
    rx_avail: VecDeque<RxId>,
    rx_ready: VecDeque<RxId>,
    tx_avail: VecDeque<TxSegment>,
    tx_ready: VecDeque<TxId>,
}

#[derive(Inspect, Default)]
struct Stats {
    rx_dropped: Counter,
    tx_dropped: Counter,
    tx_errors: Counter,
    tx_unknown: Counter,
}

struct Client<'a> {
    state: &'a mut QueueState,
    stats: &'a mut Stats,
    driver: &'a dyn Driver,
}

impl consomme::Client for Client<'_> {
    fn driver(&self) -> &dyn Driver {
        self.driver
    }

    fn recv(&mut self, data: &[u8], checksum: &ChecksumState) {
        let Some(rx_id) = self.state.rx_avail.pop_front() else {
            // This should be rare, only affecting unbuffered protocols. TCP and
            // UDP are buffered and they won't indicate packets unless rx_mtu()
            // returns a non-zero value.
            self.stats.rx_dropped.increment();
            return;
        };
        let max = self.state.pool.capacity(rx_id) as usize;
        if data.len() <= max {
            self.state.pool.write_packet(
                rx_id,
                &RxMetadata {
                    offset: 0,
                    len: data.len(),
                    ip_checksum: if checksum.ipv4 {
                        RxChecksumState::Good
                    } else {
                        RxChecksumState::Unknown
                    },
                    l4_checksum: if checksum.tcp || checksum.udp {
                        RxChecksumState::Good
                    } else {
                        RxChecksumState::Unknown
                    },
                    l4_protocol: if checksum.tcp {
                        L4Protocol::Tcp
                    } else if checksum.udp {
                        L4Protocol::Udp
                    } else {
                        L4Protocol::Unknown
                    },
                },
                data,
            );
            self.state.rx_ready.push_back(rx_id);
        } else {
            tracing::warn!(len = data.len(), max, "dropping rx packet: too large");
            self.state.rx_avail.push_front(rx_id);
        }
    }

    fn rx_mtu(&mut self) -> usize {
        if let Some(&rx_id) = self.state.rx_avail.front() {
            self.state.pool.capacity(rx_id) as usize
        } else {
            0
        }
    }
}
