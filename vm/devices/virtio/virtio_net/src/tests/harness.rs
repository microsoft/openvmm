// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test harness extensions for the egress policy, quiesce and saved-state
//! tests.
//!
//! [`ForkMockEndpoint`] wraps the mock endpoint. It accepts an egress policy
//! and wraps each mock queue in a [`ForkMockQueue`], which applies the policy
//! to the frames it transmits, can replace the next transmitted frame (to model
//! a guest that rewrites a descriptor after the device checked it), and
//! supports quiesce.

use super::MockEndpoint;
use super::MockQueue;
use super::MockQueueHandle;
use super::NET_HEADER_SIZE;
use super::QUEUE_SIZE;
use super::RX_AVAIL_ADDR;
use super::RX_DESC_ADDR;
use super::RX_USED_ADDR;
use super::TX_AVAIL_ADDR;
use super::TestHarness;
use super::new_mock_queue;
use super::post_tx_packet;
use crate::Device;
use async_trait::async_trait;
use futures::StreamExt;
use inspect::InspectMut;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::MultiQueueSupport;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxId;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::linearize;
use net_backend::next_packet;
use net_backend::quiesce::QueueQuiesceResult;
use net_backend_resources::egress::EgressPolicy;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use parking_lot::Mutex;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::test_helpers::make_available;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

/// Controls of a [`MockQueueHandle`] for queues created by
/// [`ForkMockEndpoint`].
#[derive(Default)]
pub(super) struct QueueHandleExt {
    replace_next_tx: Arc<Mutex<Option<Vec<u8>>>>,
    quiesce_notify: mesh::Receiver<()>,
}

impl MockQueueHandle {
    pub(super) fn replace_next_tx(&self, frame: Vec<u8>) {
        *self.ext.replace_next_tx.lock() = Some(frame);
    }

    pub(super) async fn wait_for_quiesce(&mut self) {
        mesh::CancelContext::new()
            .with_timeout(Duration::from_secs(5))
            .until_cancelled(self.ext.quiesce_notify.next())
            .await
            .expect("timed out waiting for queue quiesce")
            .expect("channel closed");
    }
}

/// A [`MockEndpoint`] that accepts an egress policy and creates
/// [`ForkMockQueue`]s.
struct ForkMockEndpoint {
    endpoint: MockEndpoint,
    egress_policy: Option<EgressPolicy>,
}

impl InspectMut for ForkMockEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.endpoint.inspect_mut(req);
    }
}

#[async_trait]
impl Endpoint for ForkMockEndpoint {
    fn endpoint_type(&self) -> &'static str {
        self.endpoint.endpoint_type()
    }

    async fn get_queues(
        &mut self,
        _config: Vec<QueueConfig>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn net_backend::Queue>>,
    ) -> anyhow::Result<()> {
        let (queue, mut handle) = new_mock_queue();
        let queue = ForkMockQueue {
            queue,
            egress_policy: self.egress_policy.clone(),
            replace_next_tx: handle.ext.replace_next_tx.clone(),
            quiesce_notify: handle.ext.quiesce_notify.sender(),
        };
        self.endpoint.queue_tx.send(handle);
        queues.push(Box::new(queue));
        Ok(())
    }

    fn set_egress_policy(&mut self, policy: EgressPolicy) -> anyhow::Result<()> {
        self.egress_policy = Some(policy);
        Ok(())
    }

    async fn stop(&mut self) {
        self.endpoint.stop().await
    }

    fn is_ordered(&self) -> bool {
        self.endpoint.is_ordered()
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        self.endpoint.tx_offload_support()
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        self.endpoint.multiqueue_support()
    }

    fn tx_fast_completions(&self) -> bool {
        self.endpoint.tx_fast_completions()
    }

    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        self.endpoint.wait_for_endpoint_action().await
    }
}

/// A [`MockQueue`] that applies an egress policy to transmitted frames, can
/// replace the next transmitted frame, and supports quiesce.
struct ForkMockQueue {
    queue: MockQueue,
    egress_policy: Option<EgressPolicy>,
    replace_next_tx: Arc<Mutex<Option<Vec<u8>>>>,
    quiesce_notify: mesh::Sender<()>,
}

impl InspectMut for ForkMockQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.queue.inspect_mut(req);
    }
}

#[async_trait]
impl net_backend::Queue for ForkMockQueue {
    fn poll_ready(
        &mut self,
        cx: &mut Context<'_>,
        pool: &mut dyn net_backend::BufferAccess,
    ) -> Poll<()> {
        self.queue.poll_ready(cx, pool)
    }

    fn rx_avail(&mut self, pool: &mut dyn net_backend::BufferAccess, done: &[RxId]) {
        self.queue.rx_avail(pool, done)
    }

    fn rx_poll(
        &mut self,
        pool: &mut dyn net_backend::BufferAccess,
        packets: &mut [RxId],
    ) -> anyhow::Result<usize> {
        self.queue.rx_poll(pool, packets)
    }

    async fn quiesce(
        &mut self,
        _pool: &mut dyn net_backend::BufferAccess,
    ) -> anyhow::Result<QueueQuiesceResult> {
        self.quiesce_notify.send(());
        self.queue.rx_pending.lock().clear();
        Ok(QueueQuiesceResult {
            rx_ready: self.queue.rx_ready.lock().len(),
            tx_ready: self.queue.tx_completions.lock().iter().map(Vec::len).sum(),
        })
    }

    fn tx_avail(
        &mut self,
        pool: &mut dyn net_backend::BufferAccess,
        segments: &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        if let Some(replacement) = self.replace_next_tx.lock().take() {
            let (metadata, packet_segments, _) = next_packet(segments);
            anyhow::ensure!(replacement.len() == metadata.len as usize);
            let mut offset = 0usize;
            for segment in packet_segments {
                let end = offset + segment.len as usize;
                pool.guest_memory()
                    .write_at(segment.gpa, &replacement[offset..end])?;
                offset = end;
            }
            anyhow::ensure!(offset == replacement.len());
        }
        if let Some(policy) = &self.egress_policy {
            let mut remaining = segments;
            while !remaining.is_empty() {
                let frame = linearize(pool, &mut remaining)?;
                if policy.authorize_frame(&frame, frame.len()).is_err() {
                    return Ok((true, segments.len()));
                }
            }
        }

        self.queue.tx_avail(pool, segments)
    }

    fn tx_poll(
        &mut self,
        pool: &mut dyn net_backend::BufferAccess,
        done: &mut [TxId],
    ) -> Result<usize, TxError> {
        self.queue.tx_poll(pool, done)
    }
}

impl TestHarness {
    /// Replaces the harness device with one built with `egress_policy` over a
    /// [`ForkMockEndpoint`].
    pub(super) fn new_with_egress_policy(
        driver: &DefaultDriver,
        egress_policy: Option<EgressPolicy>,
    ) -> Self {
        let mut harness = Self::new(driver);
        let (queue_tx, queue_handle_rx) = mesh::channel();
        let endpoint = ForkMockEndpoint {
            endpoint: MockEndpoint {
                queue_tx,
                is_ordered: true,
            },
            egress_policy: None,
        };
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let mac = MacAddress::new([0x00, 0x15, 0x5d, 0xaa, 0xbb, 0xcc]);
        let mut builder = Device::builder();
        if let Some(egress_policy) = egress_policy {
            builder = builder.egress_policy(egress_policy);
        }
        harness.device = builder
            .build(&driver_source, Box::new(endpoint), mac)
            .unwrap();
        harness.queue_handle_rx = queue_handle_rx;
        harness
    }

    /// Replaces the harness device with one built for save and restore over a
    /// [`ForkMockEndpoint`].
    pub(super) fn new_save_restore(driver: &DefaultDriver) -> Self {
        let mut harness = Self::new(driver);
        let (queue_tx, queue_handle_rx) = mesh::channel();
        let endpoint = ForkMockEndpoint {
            endpoint: MockEndpoint {
                queue_tx,
                is_ordered: true,
            },
            egress_policy: None,
        };
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let mac = MacAddress::new([0x52, 0x54, 0, 0, 0, 2]);
        harness.device = Device::builder()
            .save_restore(
                net_backend_resources::consomme::static_ipv4::StaticIpv4Config {
                    guest_ipv4: std::net::Ipv4Addr::new(10, 0, 0, 2),
                    prefix_length: 24,
                    gateway_ipv4: std::net::Ipv4Addr::new(10, 0, 0, 1),
                    gateway_mac: MacAddress::new([0x52, 0x54, 0, 0, 0, 1]),
                },
                (1 << 5) | (1 << 32),
            )
            .build(&driver_source, Box::new(endpoint), mac)
            .unwrap();
        harness.queue_handle_rx = queue_handle_rx;
        harness
    }

    pub(super) async fn start_rx_queue(&mut self, features: &VirtioDeviceFeatures) {
        let rx_interrupt = Interrupt::from_event(self.rx_interrupt_event.clone());

        // Queue 0: RX
        self.device
            .start_queue(
                0,
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: RX_DESC_ADDR,
                        avail_addr: RX_AVAIL_ADDR,
                        used_addr: RX_USED_ADDR,
                    },
                    notify: rx_interrupt,
                    event: self.rx_event.clone(),
                    guest_memory: self.mem.clone(),
                },
                features,
                None,
            )
            .await
            .unwrap();
    }

    pub(super) fn post_tx_frame_and_signal(&mut self, desc_index: u16, frame: &[u8]) {
        let header_gpa = self.alloc_data(NET_HEADER_SIZE);
        let data_len = u32::try_from(frame.len()).unwrap();
        let data_gpa = self.alloc_data(data_len);

        // Write a zero virtio-net header
        let header_bytes = vec![0u8; NET_HEADER_SIZE as usize];
        self.mem.write_at(header_gpa, &header_bytes).unwrap();

        self.mem.write_at(data_gpa, frame).unwrap();

        post_tx_packet(
            &self.mem,
            desc_index,
            header_gpa,
            NET_HEADER_SIZE,
            &[(data_gpa, data_len)],
        );

        make_available(
            &self.mem,
            TX_AVAIL_ADDR,
            QUEUE_SIZE,
            desc_index,
            &mut self.tx_avail_idx,
        );
        self.tx_event.signal();
    }
}
