// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Saved state of a virtio-net device built for save and restore.
//!
//! [`NicBuilder::save_restore`] binds the device to an exact static IPv4
//! identity and an effective feature contract. When the transport stops an
//! active queue pair, the device quiesces the endpoint and captures the
//! drained queue progress, which the transport then collects queue by queue.
//! The device-private [`SavedState`] records the identity, the feature
//! contract and the queue-pair lifecycle, and is validated against the
//! destination device before it is restored.

use crate::Adapter;
use crate::Device;
use crate::NetStatus;
use crate::NicBuilder;
use crate::QueuePairState;
use crate::Worker;
use mesh::payload::Protobuf;
use net_backend::TxOffloadSupport;
use net_backend_resources::consomme::static_ipv4::StaticIpv4Config;
use std::ops::ControlFlow;
use virtio::device::saved_state::DeviceQueueState;
use virtio::device::saved_state::DeviceStateValidator;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use virtio_resources::net::VirtioNetHandle;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::save_restore::SavedStateRoot;

const SAVED_STATE_VERSION: u32 = 1;

/// Save and restore configuration of a device built with
/// [`NicBuilder::save_restore`].
pub(crate) struct SaveRestoreConfig {
    static_ipv4: StaticIpv4Config,
    effective_features: u64,
}

/// Queue-pair stop, quiesce and restore bookkeeping of a [`Device`].
#[derive(Default)]
pub(crate) struct Lifecycle {
    stop_error: Option<anyhow::Error>,
    stopped_feature_banks: Option<[u32; 2]>,
    stopped_queue_count: u32,
    pub(crate) endpoint_generation: u64,
    pub(crate) input_quiesced: bool,
}

impl Lifecycle {
    /// Clears the queue-pair state on device reset. The endpoint generation is
    /// kept.
    pub(crate) fn reset(&mut self) {
        self.stop_error = None;
        self.stopped_feature_banks = None;
        self.stopped_queue_count = 0;
        self.input_quiesced = false;
    }
}

impl NicBuilder {
    /// Enables save and restore for a device with the given exact static IPv4
    /// identity and effective feature contract.
    pub fn save_restore(mut self, static_ipv4: StaticIpv4Config, effective_features: u64) -> Self {
        self.save_restore = Some(SaveRestoreConfig {
            static_ipv4,
            effective_features,
        });
        self
    }

    /// Applies the save and restore configuration of a virtio-net resource.
    pub(crate) fn save_restore_resource(self, resource: &VirtioNetHandle) -> anyhow::Result<Self> {
        if !resource.save_restore {
            return Ok(self);
        }
        Ok(self.save_restore(
            resource
                .static_ipv4
                .clone()
                .ok_or_else(|| anyhow::anyhow!("saved virtio-net requires static IPv4 identity"))?,
            resource.effective_features.ok_or_else(|| {
                anyhow::anyhow!("saved virtio-net requires an effective feature contract")
            })?,
        ))
    }
}

impl Device {
    /// Checks and resets the saved queue-pair state before `start_queue`
    /// starts queue `pair_idx`/`is_rx`.
    pub(crate) fn prepare_queue_start(
        &mut self,
        pair_idx: usize,
        is_rx: bool,
        features: &VirtioDeviceFeatures,
    ) -> anyhow::Result<()> {
        let feature_banks = [features.bank(0), features.bank(1)];
        match self.pairs.get(pair_idx) {
            Some(QueuePairState::Empty) => {
                self.lifecycle.stopped_queue_count = 0;
            }
            Some(QueuePairState::Stopped { rx, tx }) => {
                anyhow::ensure!(
                    rx.is_none() && tx.is_none(),
                    "cannot restart virtio-net before all stopped queue states are collected"
                );
                if let Some(previous) = self.lifecycle.stopped_feature_banks {
                    anyhow::ensure!(
                        previous == feature_banks,
                        "virtio-net negotiated features changed while restarting queues"
                    );
                }
                self.lifecycle.stopped_queue_count = 0;
            }
            Some(QueuePairState::HalfOpen {
                is_rx: pending_is_rx,
                feature_banks: pending_feature_banks,
                ..
            }) if *pending_is_rx != is_rx => {
                anyhow::ensure!(
                    *pending_feature_banks == feature_banks,
                    "virtio-net queue pair negotiated different features"
                );
            }
            _ => {}
        }
        Ok(())
    }

    /// Stops queue `idx` when its state is kept: a started half-open queue, an
    /// active pair of a device built for save and restore, or a stopped pair.
    ///
    /// Returns [`ControlFlow::Continue`] when `stop_queue` must stop the queue
    /// itself.
    pub(crate) async fn stop_saved_queue(&mut self, idx: u16) -> ControlFlow<Option<QueueState>> {
        let pair_idx = (idx / 2) as usize;
        if pair_idx >= self.pairs.len() {
            return ControlFlow::Continue(());
        }
        match self.pairs[pair_idx] {
            QueuePairState::HalfOpen { is_rx, .. } if is_rx == idx.is_multiple_of(2) => {
                let previous = std::mem::replace(&mut self.pairs[pair_idx], QueuePairState::Empty);
                let QueuePairState::HalfOpen {
                    queue,
                    feature_banks,
                    ..
                } = previous
                else {
                    unreachable!()
                };
                self.lifecycle.stopped_feature_banks = Some(feature_banks);
                self.lifecycle.stopped_queue_count = 1;
                ControlFlow::Break(Some(queue.queue_state()))
            }
            QueuePairState::Active if self.adapter.save_restore.is_some() => {
                if let Err(error) = self.capture_stopped_pairs().await {
                    self.lifecycle.stop_error = Some(error);
                    return ControlFlow::Break(None);
                }
                ControlFlow::Break(self.take_stopped_queue(idx))
            }
            QueuePairState::Stopped { .. } => ControlFlow::Break(self.take_stopped_queue(idx)),
            _ => ControlFlow::Continue(()),
        }
    }

    fn take_stopped_queue(&mut self, idx: u16) -> Option<QueueState> {
        let pair_idx = (idx / 2) as usize;
        match self.pairs.get_mut(pair_idx) {
            Some(QueuePairState::Stopped { rx, tx }) => {
                if idx.is_multiple_of(2) {
                    rx.take()
                } else {
                    tx.take()
                }
            }
            _ => None,
        }
    }

    async fn capture_stopped_pairs(&mut self) -> anyhow::Result<()> {
        self.quiesce_active_workers(false).await?;
        let mut coordinator = self.coordinator.remove();
        self.lifecycle.stopped_queue_count = 0;
        for (pair_index, worker) in coordinator.workers.iter_mut().enumerate() {
            worker.task_mut().state.take();
            let worker = worker.remove();
            let (rx, tx, feature_banks) = worker.into_stopped_state()?;
            if let Some(previous) = self.lifecycle.stopped_feature_banks {
                anyhow::ensure!(
                    previous == feature_banks,
                    "virtio-net queue pairs negotiated different features"
                );
            } else {
                self.lifecycle.stopped_feature_banks = Some(feature_banks);
            }
            self.pairs[pair_index] = QueuePairState::Stopped {
                rx: Some(rx),
                tx: Some(tx),
            };
            self.lifecycle.stopped_queue_count += 2;
        }
        Ok(())
    }

    /// Implements `save_device`.
    pub(crate) fn save_private_state(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        if let Some(error) = self.lifecycle.stop_error.take() {
            return Err(SaveError::Other(error));
        }
        let config = self.adapter.save_restore.as_ref().ok_or_else(|| {
            SaveError::Other(anyhow::anyhow!(
                "virtio-net static IPv4 identity is unavailable"
            ))
        })?;
        let static_ipv4 = &config.static_ipv4;
        let effective_features = config.effective_features;
        if !self.pairs.iter().all(|pair| {
            matches!(
                pair,
                QueuePairState::Empty | QueuePairState::Stopped { rx: None, tx: None }
            )
        }) {
            return Err(SaveError::Other(anyhow::anyhow!(
                "virtio-net queues are still running"
            )));
        }
        let negotiated = match self.lifecycle.stopped_queue_count {
            0 => [0, 0],
            1 | 2 => self.lifecycle.stopped_feature_banks.ok_or_else(|| {
                SaveError::Other(anyhow::anyhow!(
                    "virtio-net stopped queue features are unavailable"
                ))
            })?,
            count => {
                return Err(SaveError::Other(anyhow::anyhow!(
                    "virtio-net stopped queue count {count} is invalid"
                )));
            }
        };
        Ok(Some(SavedStateBlob::new(SavedState {
            schema_version: SAVED_STATE_VERSION,
            guest_ipv4: u32::from(static_ipv4.guest_ipv4),
            prefix_length: u32::from(static_ipv4.prefix_length),
            gateway_ipv4: u32::from(static_ipv4.gateway_ipv4),
            guest_mac: self.adapter.mac_address.to_bytes().to_vec(),
            gateway_mac: static_ipv4.gateway_mac.to_bytes().to_vec(),
            link_up: NetStatus::from(self.registers.status).link_up(),
            effective_feature_bank0: effective_features as u32,
            effective_feature_bank1: (effective_features >> 32) as u32,
            negotiated_feature_bank0: negotiated[0],
            negotiated_feature_bank1: negotiated[1],
            endpoint_offload_bits: endpoint_offload_bits(self.adapter.tx_offload_support),
            queue_pair_lifecycle: self.lifecycle.stopped_queue_count,
            rx_in_order_outstanding: 0,
            tx_in_order_outstanding: 0,
            pending_rx_packet_count: 0,
            pending_rx_payload_bytes: 0,
            pending_tx_packet_count: 0,
            pending_tx_payload_bytes: 0,
            endpoint_generation: self.lifecycle.endpoint_generation,
        })))
    }

    /// Implements `restore_device`.
    pub(crate) fn restore_private_state(
        &mut self,
        state: Option<SavedStateBlob>,
    ) -> Result<(), RestoreError> {
        let saved = validate_saved_state(state.as_ref(), &self.adapter, None, None)?;
        self.registers.status = NetStatus::new().with_link_up(saved.link_up).into();
        self.lifecycle.endpoint_generation = saved
            .endpoint_generation
            .checked_add(1)
            .ok_or_else(|| invalid_saved_state("virtio-net endpoint generation overflowed"))?;
        self.lifecycle.stopped_feature_banks = (saved.queue_pair_lifecycle != 0).then_some([
            saved.negotiated_feature_bank0,
            saved.negotiated_feature_bank1,
        ]);
        self.lifecycle.stopped_queue_count = saved.queue_pair_lifecycle;
        Ok(())
    }

    /// Implements `device_state_validator`.
    pub(crate) fn private_state_validator(&self) -> DeviceStateValidator {
        let adapter = self.adapter.clone();
        Box::new(move |state, features, queues, _guest_memory| {
            validate_saved_state(state, &adapter, Some(features), Some(queues))?;
            Ok(())
        })
    }
}

impl Worker {
    fn into_stopped_state(mut self) -> anyhow::Result<(QueueState, QueueState, [u32; 2])> {
        anyhow::ensure!(
            self.active_state.data.tx_segments.is_empty()
                && self
                    .active_state
                    .pending_tx_packets
                    .iter()
                    .all(Option::is_none)
                && self.virtio_state.tx_in_order.outstanding() == 0,
            "virtio-net TX ownership remained after endpoint quiesce"
        );

        // The number of guest RX descriptors currently owned by the device.
        let pending_rx = self
            .active_state
            .pending_rx_packets
            .fill_ready(&mut self.active_state.data.rx_ready);
        anyhow::ensure!(
            pending_rx == self.virtio_state.rx_in_order.outstanding(),
            "virtio-net RX ownership does not match its completion cursor"
        );
        let current_rx = self.virtio_state.rx_queue.queue_state();
        anyhow::ensure!(
            usize::from(current_rx.avail_index.wrapping_sub(current_rx.used_index)) == pending_rx,
            "virtio-net RX queue cursors do not match pending descriptor ownership"
        );
        let rx = QueueState {
            avail_index: current_rx.used_index,
            used_index: current_rx.used_index,
        };

        let tx = self.virtio_state.tx_queue.queue_state();
        anyhow::ensure!(
            tx.avail_index == tx.used_index,
            "virtio-net TX queue retained an incomplete descriptor"
        );
        Ok((
            rx,
            tx,
            [
                self.negotiated_features.into_bits(),
                self.negotiated_features_bank1.into_bits(),
            ],
        ))
    }
}

fn endpoint_offload_bits(offloads: TxOffloadSupport) -> u32 {
    u32::from(offloads.ipv4_header)
        | (u32::from(offloads.tcp) << 1)
        | (u32::from(offloads.udp) << 2)
        | (u32::from(offloads.tso) << 3)
        | (u32::from(offloads.uso) << 4)
}

fn derived_mac(address: std::net::Ipv4Addr) -> [u8; 6] {
    let [_, second, third, fourth] = address.octets();
    [0x52, 0x54, 0, second, third, fourth]
}

fn invalid_saved_state(message: impl Into<String>) -> RestoreError {
    RestoreError::InvalidSavedState(anyhow::anyhow!(message.into()))
}

fn validate_saved_state(
    state: Option<&SavedStateBlob>,
    adapter: &Adapter,
    features: Option<&VirtioDeviceFeatures>,
    queues: Option<&[DeviceQueueState]>,
) -> Result<SavedState, RestoreError> {
    let state = state.ok_or_else(|| invalid_saved_state("missing virtio-net private state"))?;
    let saved: SavedState = state.parse()?;
    if saved.schema_version != SAVED_STATE_VERSION {
        return Err(invalid_saved_state(format!(
            "unsupported virtio-net schema version {}",
            saved.schema_version
        )));
    }

    let config = adapter
        .save_restore
        .as_ref()
        .ok_or_else(|| invalid_saved_state("virtio-net static IPv4 identity is unavailable"))?;
    let static_ipv4 = &config.static_ipv4;
    let effective_features = config.effective_features;
    if saved.guest_ipv4 != u32::from(static_ipv4.guest_ipv4)
        || saved.prefix_length != u32::from(static_ipv4.prefix_length)
        || saved.gateway_ipv4 != u32::from(static_ipv4.gateway_ipv4)
        || saved.guest_mac != adapter.mac_address.to_bytes()
        || saved.gateway_mac != static_ipv4.gateway_mac.to_bytes()
    {
        return Err(invalid_saved_state(
            "virtio-net static identity does not match the destination",
        ));
    }
    if saved.guest_mac != derived_mac(static_ipv4.guest_ipv4)
        || saved.gateway_mac != derived_mac(static_ipv4.gateway_ipv4)
    {
        return Err(invalid_saved_state(
            "virtio-net static identity is not canonical",
        ));
    }
    if !(1..=30).contains(&static_ipv4.prefix_length) {
        return Err(invalid_saved_state("virtio-net prefix is out of range"));
    }
    let mask = u32::MAX << (32 - static_ipv4.prefix_length);
    let guest = u32::from(static_ipv4.guest_ipv4);
    let network = guest & mask;
    if u32::from(static_ipv4.gateway_ipv4) != network + 1
        || guest == network
        || guest == (network | !mask)
        || static_ipv4.guest_ipv4 == static_ipv4.gateway_ipv4
    {
        return Err(invalid_saved_state(
            "virtio-net IPv4 identity is internally inconsistent",
        ));
    }

    if saved.effective_feature_bank0 != effective_features as u32
        || saved.effective_feature_bank1 != (effective_features >> 32) as u32
        || saved.endpoint_offload_bits != endpoint_offload_bits(adapter.tx_offload_support)
    {
        return Err(invalid_saved_state(
            "virtio-net feature or endpoint capability contract changed",
        ));
    }
    if saved.queue_pair_lifecycle > 2
        || saved.rx_in_order_outstanding != 0
        || saved.tx_in_order_outstanding != 0
        || saved.pending_rx_packet_count != 0
        || saved.pending_rx_payload_bytes != 0
        || saved.pending_tx_packet_count != 0
        || saved.pending_tx_payload_bytes != 0
    {
        return Err(invalid_saved_state(
            "virtio-net saved packet ownership is not fully drained",
        ));
    }

    if let Some(features) = features {
        if features.ring_packed() || (features.into_bits() & !effective_features) != 0 {
            return Err(invalid_saved_state(
                "virtio-net negotiated features violate the device contract",
            ));
        }
        if saved.queue_pair_lifecycle == 0 {
            if saved.negotiated_feature_bank0 != 0 || saved.negotiated_feature_bank1 != 0 {
                return Err(invalid_saved_state(
                    "virtio-net inactive queues retain negotiated feature state",
                ));
            }
        } else if saved.negotiated_feature_bank0 != features.bank(0)
            || saved.negotiated_feature_bank1 != features.bank(1)
        {
            return Err(invalid_saved_state(
                "virtio-net negotiated features do not match saved state",
            ));
        }
    }
    if let Some(queues) = queues {
        if queues.len() != 2 {
            return Err(invalid_saved_state(
                "virtio-net saved state requires exactly one queue pair",
            ));
        }
        let started_count = queues
            .iter()
            .filter(|queue| queue.params.enable && queue.queue_state.is_some())
            .count();
        if started_count != usize::try_from(saved.queue_pair_lifecycle).unwrap_or(usize::MAX) {
            return Err(invalid_saved_state(
                "virtio-net queue lifecycle does not match transport state",
            ));
        }
        for queue in queues {
            if !queue.params.enable && queue.queue_state.is_some() {
                return Err(invalid_saved_state(
                    "virtio-net disabled queue retains progress state",
                ));
            }
            if !queue.params.enable {
                continue;
            }
            if queue.params.size == 0 || queue.params.size > 256 {
                return Err(invalid_saved_state(
                    "virtio-net restored queue size is out of range",
                ));
            }
            if queue
                .queue_state
                .is_some_and(|state| state.avail_index != state.used_index)
            {
                return Err(invalid_saved_state(
                    "virtio-net restored queue retains unrepresented ownership",
                ));
            }
        }
    }
    Ok(saved)
}

#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "virtio.net")]
pub struct SavedState {
    #[mesh(1)]
    pub schema_version: u32,
    #[mesh(2)]
    pub guest_ipv4: u32,
    #[mesh(3)]
    pub prefix_length: u32,
    #[mesh(4)]
    pub gateway_ipv4: u32,
    #[mesh(5)]
    pub guest_mac: Vec<u8>,
    #[mesh(6)]
    pub gateway_mac: Vec<u8>,
    #[mesh(7)]
    pub link_up: bool,
    #[mesh(8)]
    pub effective_feature_bank0: u32,
    #[mesh(9)]
    pub effective_feature_bank1: u32,
    #[mesh(10)]
    pub negotiated_feature_bank0: u32,
    #[mesh(11)]
    pub negotiated_feature_bank1: u32,
    #[mesh(12)]
    pub endpoint_offload_bits: u32,
    #[mesh(13)]
    pub queue_pair_lifecycle: u32,
    #[mesh(14)]
    pub rx_in_order_outstanding: u32,
    #[mesh(15)]
    pub tx_in_order_outstanding: u32,
    #[mesh(16)]
    pub pending_rx_packet_count: u32,
    #[mesh(17)]
    pub pending_rx_payload_bytes: u64,
    #[mesh(18)]
    pub pending_tx_packet_count: u32,
    #[mesh(19)]
    pub pending_tx_payload_bytes: u64,
    #[mesh(20)]
    pub endpoint_generation: u64,
}
