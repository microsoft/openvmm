// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Activation tests: a queue pair whose endpoint cannot create its queues
//! fails its start, instead of leaving the device without endpoint queues,
//! and a restored device whose start fails keeps the state that the restore
//! gave it.

use super::MockEndpoint;
use super::QUEUE_SIZE;
use super::RX_AVAIL_ADDR;
use super::RX_DESC_ADDR;
use super::RX_USED_ADDR;
use super::TOTAL_MEM_SIZE;
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
use virtio::device::saved_state::DeviceQueueState;
use virtio::queue::QueueParams;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

/// An endpoint that fails to create queues the first `failures` times.
struct FailingEndpoint {
    endpoint: Box<dyn Endpoint>,
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
            endpoint: Box::new(MockEndpoint {
                queue_tx,
                queue_config_tx,
                endpoint_action_rx,
                is_ordered: true,
            }),
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

    /// Replaces the harness device with one built for save and restore whose
    /// endpoint fails to create queues the first `failures` times.
    fn new_save_restore_with_endpoint_failures(driver: &DefaultDriver, failures: usize) -> Self {
        Self::new_save_restore_over(driver, |endpoint| {
            Box::new(FailingEndpoint { endpoint, failures })
        })
    }

    fn rx_queue_resources(&self) -> QueueResources {
        QueueResources {
            params: rx_queue_params(),
            notify: Interrupt::from_event(self.rx_interrupt_event.clone()),
            event: self.rx_event.clone(),
            guest_memory: self.mem.clone(),
        }
    }

    fn tx_queue_resources(&self) -> QueueResources {
        QueueResources {
            params: tx_queue_params(),
            notify: Interrupt::from_event(self.tx_interrupt_event.clone()),
            event: self.tx_event.clone(),
            guest_memory: self.mem.clone(),
        }
    }
}

fn rx_queue_params() -> QueueParams {
    QueueParams {
        size: QUEUE_SIZE,
        enable: true,
        desc_addr: RX_DESC_ADDR,
        avail_addr: RX_AVAIL_ADDR,
        used_addr: RX_USED_ADDR,
    }
}

fn tx_queue_params() -> QueueParams {
    QueueParams {
        size: QUEUE_SIZE,
        enable: true,
        desc_addr: TX_DESC_ADDR,
        avail_addr: TX_AVAIL_ADDR,
        used_addr: TX_USED_ADDR,
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

/// A restored NIC whose pair fails to start when the VM starts, because its
/// endpoint cannot create its queues or its TX queue cannot be created, keeps
/// the queue lifecycle that the restore gave it: a save of the stopped VM holds
/// the lifecycle of the queues whose progress the transport keeps, so that the
/// snapshot restores, and a later start resumes the queues where the restore
/// left them.
#[async_test]
async fn failed_start_of_a_restored_nic_keeps_its_queue_lifecycle(driver: DefaultDriver) {
    let features = VirtioDeviceFeatures::new();
    let mut source = TestHarness::new_save_restore(&driver);
    let _queue = source.enable_and_get_handle().await;
    source.post_tx_and_signal(0, 64);
    assert_eq!(source.wait_for_used().await.0, 0);
    source.device.quiesce_input().await.unwrap();
    let rx = source.device.stop_queue(0).await.unwrap();
    let tx = source.device.stop_queue(1).await.unwrap();
    let saved = source.device.save_device().unwrap().unwrap();

    let mut restored = TestHarness::new_save_restore_with_endpoint_failures(&driver, 1);
    let mut memory = vec![0; TOTAL_MEM_SIZE];
    source.mem.read_at(0, &mut memory).unwrap();
    restored.mem.write_at(0, &memory).unwrap();
    restored.tx_avail_idx = source.tx_avail_idx;
    restored.tx_used_idx = source.tx_used_idx;
    restored.next_data_offset = source.next_data_offset;
    restored.device.restore_device(Some(saved)).unwrap();

    let restored_queues = [
        DeviceQueueState {
            params: rx_queue_params(),
            queue_state: Some(rx),
        },
        DeviceQueueState {
            params: tx_queue_params(),
            queue_state: Some(tx),
        },
    ];
    // TX progress that the queue refuses: more in flight than the ring holds.
    let invalid_tx = QueueState {
        avail_index: tx.avail_index,
        used_index: tx.avail_index.wrapping_add(1),
    };
    for (tx_state, failure) in [
        (tx, "injected endpoint queue failure"),
        (invalid_tx, "failed creating virtio net queue"),
    ] {
        // The TX queue completes the pair, which fails to start.
        restored
            .device
            .start_queue(0, restored.rx_queue_resources(), &features, Some(rx))
            .await
            .unwrap();
        let error = restored
            .device
            .start_queue(1, restored.tx_queue_resources(), &features, Some(tx_state))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(failure), "{error:#}");
        // The transport stops the queue that started, which reports no
        // progress, so the transport keeps the restored progress of both.
        assert!(restored.device.stop_queue(0).await.is_none());
        assert!(restored.device.stop_queue(1).await.is_none());

        let resaved = restored.device.save_device().unwrap().unwrap();
        restored.device.device_state_validator()(
            Some(&resaved),
            &features,
            &restored_queues,
            &restored.mem,
        )
        .unwrap();
    }

    // The next start resumes the queues: the device transmits the guest's
    // next packet, and not the one that it transmitted before the snapshot.
    restored
        .device
        .start_queue(0, restored.rx_queue_resources(), &features, Some(rx))
        .await
        .unwrap();
    restored
        .device
        .start_queue(1, restored.tx_queue_resources(), &features, Some(tx))
        .await
        .unwrap();
    let queue = restored.wait_for_recreated_queue().await;
    restored.post_tx_and_signal(2, 64);
    assert_eq!(restored.wait_for_used().await.0, 2);
    assert_eq!(queue.take_tx_avail_log().len(), 1);
}
