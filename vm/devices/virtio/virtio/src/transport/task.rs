// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device task and shared transport state machine for virtio transports.
//!
//! Both the PCI and MMIO transports spawn an async task that owns the
//! `Box<dyn DynVirtioDevice>` and processes commands via a mesh channel.
//! The transports become thin MMIO/PCI forwarders that send RPCs to
//! the task.

use crate::DynVirtioDevice;
use crate::QueueResources;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use futures::StreamExt;
use inspect::Inspect;
use mesh::rpc::FailableRpc;
use mesh::rpc::PendingRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// Commands sent from the transport to the device task.
pub enum DeviceCommand {
    /// Guest writes DRIVER_OK — start all enabled queues.
    /// Returns true on success, false on failure (errors are logged
    /// inside the device task).
    Enable(Rpc<EnableParams, bool>),
    /// Guest writes status=0 — stop all queues, reset device.
    Disable(Rpc<(), ()>),
    /// ChangeDeviceState::stop() — stop queues, return states for resume.
    Stop(Rpc<(), Vec<Option<QueueState>>>),
    /// ChangeDeviceState::start() — restart queues with saved states.
    Start(FailableRpc<StartParams, ()>),
    /// ChangeDeviceState::reset() — stop queues, reset device.
    Reset(Rpc<(), ()>),
    /// Config register read at byte offset with byte length.
    ReadConfig {
        offset: u16,
        len: u8,
        deferred: DeferredRead,
    },
    /// Config register write at byte offset with raw data.
    WriteConfig {
        offset: u16,
        len: u8,
        data: [u8; 8],
        deferred: DeferredWrite,
    },
    /// Inspect the device state.
    Inspect(inspect::Deferred),
}

/// Parameters for the Enable command.
pub struct EnableParams {
    pub queues: Vec<(u16, QueueResources)>,
    pub features: VirtioDeviceFeatures,
}

/// Parameters for the Start command.
pub struct StartParams {
    pub queues: Vec<(u16, QueueResources, Option<QueueState>)>,
    pub features: VirtioDeviceFeatures,
}

/// Transport-side state machine tracking in-flight device operations.
///
/// In the old (synchronous) design, the guest's DRIVER_OK write called
/// `enable()` inline and set DRIVER_OK before the MMIO/PCI write returned,
/// so there was no window between "enable started" and "DRIVER_OK visible".
///
/// Now that enable is an async RPC to the device task, there is a window
/// where the enable is in flight but DRIVER_OK has not been set yet. If
/// the guest writes STATUS=0 during this window, we cannot start an async
/// disable (the transport is already busy with the enable). Instead we
/// record `pending_reset` and leave STATUS unchanged — the guest sees the
/// pre-DRIVER_OK init bits (ACKNOWLEDGE|DRIVER|FEATURES_OK) and polls.
///
/// Per virtio spec v1.2 §2.1: "The driver MUST wait for a read of
/// device_status to return 0 before reinitializing the device." The spec
/// explicitly allows the device to complete reset asynchronously — STATUS
/// does not need to read back as 0 immediately after a STATUS=0 write.
///
/// When the enable completes, `poll()` sees `pending_reset`, sends Disable
/// to the device task, and transitions to Disabling. When Disable completes,
/// the transport clears STATUS to 0, and the guest's polling loop observes
/// the reset.
#[derive(Inspect)]
#[inspect(tag = "state")]
pub enum TransportState {
    Ready,
    Enabling {
        #[inspect(skip)]
        rpc: PendingRpc<bool>,
        /// Guest wrote STATUS=0 while the enable was in flight.
        /// When the enable completes, send Disable instead of setting
        /// DRIVER_OK.
        pending_reset: bool,
    },
    Disabling {
        #[inspect(skip)]
        rpc: PendingRpc<()>,
    },
}

/// Result from polling the transport state machine.
#[must_use]
pub enum TransportStateResult {
    EnableComplete(bool),
    DisableComplete,
}

impl TransportState {
    /// Try to record a guest reset. Returns true if the transport is
    /// busy (enable or disable in flight) and the reset will be handled
    /// when the in-flight operation completes. Returns false if the
    /// transport is idle and the caller should handle the reset directly.
    pub fn try_pending_reset(&mut self) -> bool {
        match self {
            TransportState::Enabling { pending_reset, .. } => {
                *pending_reset = true;
                true
            }
            TransportState::Disabling { .. } => {
                // Already tearing down — the reset will complete
                // when the disable finishes.
                true
            }
            TransportState::Ready => false,
        }
    }

    pub fn is_busy(&self) -> bool {
        !matches!(self, TransportState::Ready)
    }

    /// Send Enable to the device task and transition to `Enabling`.
    ///
    /// Panics if the transport is not `Ready`.
    pub fn start_enable(
        &mut self,
        sender: &mesh::Sender<DeviceCommand>,
        queues: Vec<(u16, QueueResources)>,
        features: VirtioDeviceFeatures,
    ) {
        assert!(!self.is_busy());
        let rpc = sender.call(DeviceCommand::Enable, EnableParams { queues, features });
        *self = TransportState::Enabling {
            rpc,
            pending_reset: false,
        };
    }

    /// Send Disable to the device task and transition to `Disabling`.
    ///
    /// Panics if the transport is not `Ready`.
    pub fn start_disable(&mut self, sender: &mesh::Sender<DeviceCommand>) {
        assert!(!self.is_busy());
        let rpc = sender.call(DeviceCommand::Disable, ());
        *self = TransportState::Disabling { rpc };
    }

    pub fn poll(
        &mut self,
        cx: &mut Context<'_>,
        sender: &mesh::Sender<DeviceCommand>,
    ) -> Poll<TransportStateResult> {
        match self {
            TransportState::Ready => Poll::Pending,
            TransportState::Enabling { rpc, pending_reset } => {
                let result = std::task::ready!(Pin::new(rpc).poll(cx));
                let pending_reset = *pending_reset;
                if pending_reset {
                    // Guest wrote STATUS=0 while enable was in flight.
                    // Send Disable to stop any running queues, then
                    // transition to Disabling so the next poll
                    // completes the reset.
                    *self = TransportState::Ready;
                    self.start_disable(sender);
                    cx.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    *self = TransportState::Ready;
                    Poll::Ready(TransportStateResult::EnableComplete(
                        result.unwrap_or(false),
                    ))
                }
            }
            TransportState::Disabling { rpc } => {
                let _ = std::task::ready!(Pin::new(rpc).poll(cx));
                *self = TransportState::Ready;
                Poll::Ready(TransportStateResult::DisableComplete)
            }
        }
    }

    /// Wait for any in-flight enable or disable to complete, returning
    /// the result so the caller can apply the same side-effects as
    /// `poll_device`.  If an enable had a pending guest reset, this
    /// chains the disable automatically (mirroring `poll`).
    pub async fn drain(
        &mut self,
        sender: &mesh::Sender<DeviceCommand>,
    ) -> Option<TransportStateResult> {
        match std::mem::replace(self, TransportState::Ready) {
            TransportState::Enabling { rpc, pending_reset } => {
                let result = rpc.await.unwrap_or(false);
                if pending_reset {
                    // Guest wrote STATUS=0 while enable was in flight.
                    // Chain the disable, just like poll() does.
                    let rpc = sender.call(DeviceCommand::Disable, ());
                    let _ = rpc.await;
                    Some(TransportStateResult::DisableComplete)
                } else {
                    Some(TransportStateResult::EnableComplete(result))
                }
            }
            TransportState::Disabling { rpc } => {
                let _ = rpc.await;
                Some(TransportStateResult::DisableComplete)
            }
            TransportState::Ready => None,
        }
    }
}

/// Owns the virtio device and processes commands from the transport.
struct DeviceTask {
    device: Box<dyn DynVirtioDevice>,
    max_queues: u16,
}

impl DeviceTask {
    async fn enable(&mut self, params: EnableParams) -> bool {
        for (idx, resources) in params.queues {
            if let Err(err) = self
                .device
                .start_queue(idx, resources, &params.features, None)
                .await
            {
                tracelimit::error_ratelimited!(
                    error = &*err as &dyn std::error::Error,
                    idx,
                    "virtio device start_queue failed"
                );
                self.stop_all_queues().await;
                self.device.reset().await;
                return false;
            }
        }
        true
    }

    async fn disable(&mut self) {
        self.stop_all_queues().await;
        self.device.reset().await;
    }

    async fn stop(&mut self) -> Vec<Option<QueueState>> {
        let mut states = vec![None; self.max_queues as usize];
        for idx in 0..self.max_queues {
            states[idx as usize] = self.device.stop_queue(idx).await;
        }
        states
    }

    async fn start(&mut self, params: StartParams) -> anyhow::Result<()> {
        for (idx, resources, initial_state) in params.queues {
            self.device
                .start_queue(idx, resources, &params.features, initial_state)
                .await
                .map_err(|err| {
                    tracelimit::error_ratelimited!(
                        error = &*err as &dyn std::error::Error,
                        idx,
                        "virtio device start_queue failed on resume"
                    );
                    err
                })?;
        }
        Ok(())
    }

    async fn reset(&mut self) {
        self.stop_all_queues().await;
        self.device.reset().await;
    }

    async fn stop_all_queues(&mut self) {
        for idx in 0..self.max_queues {
            self.device.stop_queue(idx).await;
        }
    }
}

/// Runs the device task, processing commands from the transport.
pub async fn run_device_task(
    device: Box<dyn DynVirtioDevice>,
    mut recv: mesh::Receiver<DeviceCommand>,
) {
    let mut task = DeviceTask {
        max_queues: device.traits().max_queues,
        device,
    };

    while let Some(cmd) = recv.next().await {
        match cmd {
            DeviceCommand::Enable(rpc) => {
                rpc.handle(async |params| task.enable(params).await).await;
            }
            DeviceCommand::Disable(rpc) => {
                rpc.handle(async |()| task.disable().await).await;
            }
            DeviceCommand::Stop(rpc) => {
                rpc.handle(async |()| task.stop().await).await;
            }
            DeviceCommand::Start(rpc) => {
                // Start is used by ChangeDeviceState::start(), which is
                // sync and uses Rpc::detached() — errors are logged here
                // but not propagated to the transport.
                // TODO: update ChangeDeviceState to allow async start()
                // so failures can be handled by the transport.
                rpc.handle_failable(async |params| task.start(params).await)
                    .await;
            }
            DeviceCommand::Reset(rpc) => {
                rpc.handle(async |()| task.reset().await).await;
            }
            DeviceCommand::ReadConfig {
                offset,
                len,
                deferred,
            } => {
                let start_word = offset & !3;
                let end = offset as usize + len as usize;
                let mut buf = [0u8; 12];
                for word_off in (start_word as usize..end).step_by(4) {
                    let val = task.device.read_registers_u32(word_off as u16).await;
                    let i = word_off - start_word as usize;
                    buf[i..i + 4].copy_from_slice(&val.to_ne_bytes());
                }
                let byte_off = (offset - start_word) as usize;
                deferred.complete(&buf[byte_off..byte_off + len as usize]);
            }
            DeviceCommand::WriteConfig {
                offset,
                len,
                data,
                deferred,
            } => {
                if len == 4 && offset & 3 == 0 {
                    task.device
                        .write_registers_u32(
                            offset,
                            u32::from_ne_bytes(data[..4].try_into().unwrap()),
                        )
                        .await;
                } else {
                    let start_word = offset & !3;
                    let end = offset as usize + len as usize;
                    let byte_off = (offset - start_word) as usize;
                    let mut buf = [0u8; 12];
                    for word_off in (start_word as usize..end).step_by(4) {
                        let val = task.device.read_registers_u32(word_off as u16).await;
                        let i = word_off - start_word as usize;
                        buf[i..i + 4].copy_from_slice(&val.to_ne_bytes());
                    }
                    buf[byte_off..byte_off + len as usize].copy_from_slice(&data[..len as usize]);
                    for word_off in (start_word as usize..end).step_by(4) {
                        let i = word_off - start_word as usize;
                        let val = u32::from_ne_bytes(buf[i..i + 4].try_into().unwrap());
                        task.device.write_registers_u32(word_off as u16, val).await;
                    }
                }
                deferred.complete();
            }
            DeviceCommand::Inspect(deferred) => {
                deferred.inspect(&mut *task.device);
            }
        }
    }
}

/// Send a config read to the device task, returning a deferred IO token.
pub fn defer_config_read(sender: &mesh::Sender<DeviceCommand>, offset: u16, len: u8) -> IoResult {
    let (deferred, token) = defer_read();
    sender.send(DeviceCommand::ReadConfig {
        offset,
        len,
        deferred,
    });
    IoResult::Defer(token)
}

/// Send a config write to the device task, returning a deferred IO token.
pub fn defer_config_write(
    sender: &mesh::Sender<DeviceCommand>,
    offset: u16,
    bytes: &[u8],
) -> IoResult {
    let (deferred, token) = defer_write();
    let mut data = [0u8; 8];
    data[..bytes.len()].copy_from_slice(bytes);
    sender.send(DeviceCommand::WriteConfig {
        offset,
        len: bytes.len() as u8,
        data,
        deferred,
    });
    IoResult::Defer(token)
}
