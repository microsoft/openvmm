// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Activation tests: a queue pair whose endpoint cannot create its queues
//! fails its start, instead of leaving the device without endpoint queues.

use super::MockEndpoint;
use super::QUEUE_SIZE;
use super::TX_AVAIL_ADDR;
use super::TX_DESC_ADDR;
use super::TX_USED_ADDR;
use super::TestHarness;
use crate::Device;
use async_trait::async_trait;
use inspect::InspectMut;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::MultiQueueSupport;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::TxOffloadSupport;
use net_backend_resources::mac_address::MacAddress;
use pal_async::DefaultDriver;
use pal_async::async_test;
use test_with_tracing::test;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

/// A [`MockEndpoint`] that fails to create queues the first `failures` times.
struct FailingEndpoint {
    endpoint: MockEndpoint,
    failures: usize,
}

impl InspectMut for FailingEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.endpoint.inspect_mut(req);
    }
}

#[async_trait]
impl Endpoint for FailingEndpoint {
    fn endpoint_type(&self) -> &'static str {
        self.endpoint.endpoint_type()
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn net_backend::Queue>>,
    ) -> anyhow::Result<()> {
        if self.failures > 0 {
            self.failures -= 1;
            anyhow::bail!("injected endpoint queue failure");
        }
        self.endpoint.get_queues(config, rss, queues).await
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

impl TestHarness {
    /// Replaces the harness device with one whose endpoint fails to create
    /// queues the first `failures` times.
    fn new_with_endpoint_failures(driver: &DefaultDriver, failures: usize) -> Self {
        let mut harness = Self::new(driver);
        let (queue_tx, queue_handle_rx) = mesh::channel();
        let (queue_config_tx, queue_config_rx) = mesh::channel();
        let (endpoint_action_tx, endpoint_action_rx) = mesh::channel();
        let endpoint = FailingEndpoint {
            endpoint: MockEndpoint {
                queue_tx,
                queue_config_tx,
                endpoint_action_rx,
                is_ordered: true,
            },
            failures,
        };
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let mac = MacAddress::new([0x00, 0x15, 0x5d, 0xaa, 0xbb, 0xcc]);
        harness.device = Device::builder()
            .build(&driver_source, Box::new(endpoint), mac)
            .unwrap();
        harness.queue_handle_rx = queue_handle_rx;
        harness.queue_config_rx = queue_config_rx;
        harness.endpoint_action_tx = endpoint_action_tx;
        harness
    }

    fn tx_queue_resources(&self) -> QueueResources {
        QueueResources {
            params: QueueParams {
                size: QUEUE_SIZE,
                enable: true,
                desc_addr: TX_DESC_ADDR,
                avail_addr: TX_AVAIL_ADDR,
                used_addr: TX_USED_ADDR,
            },
            notify: Interrupt::from_event(self.tx_interrupt_event.clone()),
            event: self.tx_event.clone(),
            guest_memory: self.mem.clone(),
        }
    }
}

#[async_test]
async fn endpoint_queue_failure_fails_the_queue_start(driver: DefaultDriver) {
    let mut harness = TestHarness::new_with_endpoint_failures(&driver, 1);
    let features = VirtioDeviceFeatures::new();

    // The TX queue completes the pair, whose endpoint queues fail.
    harness.start_rx_queue(&features).await;
    let error = harness
        .device
        .start_queue(1, harness.tx_queue_resources(), &features, None)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("injected endpoint queue failure"),
        "{error:#}"
    );

    // No queue of the failed pair remains started.
    assert!(harness.device.stop_queue(0).await.is_none());
    assert!(harness.device.stop_queue(1).await.is_none());

    // After the reset that follows a failed start, the pair starts again and
    // transmits.
    harness.device.reset().await;
    let _handle = harness.enable_and_get_handle().await;
    harness.post_tx_and_signal(0, 64);
    let (used_id, _) = harness.wait_for_used().await;
    assert_eq!(used_id, 0);
}
