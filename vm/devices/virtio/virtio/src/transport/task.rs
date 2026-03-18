// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device task and shared transport state machine for virtio transports.
//!
//! Both the PCI and MMIO transports spawn an async task that owns the
//! `Box<dyn VirtioDevice>` and processes commands via a mesh channel.
//! The transports become thin MMIO/PCI forwarders that send RPCs to
//! the task.

use crate::QueueResources;
use crate::VirtioDevice;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use futures::StreamExt;
use inspect::Inspect;
use mesh::error::RemoteError;
use mesh::rpc::FailableRpc;
use mesh::rpc::PendingFailableRpc;
use mesh::rpc::PendingRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

/// Commands sent from the transport to the device task.
pub enum DeviceCommand {
    /// Guest writes DRIVER_OK — start all enabled queues.
    Enable(FailableRpc<EnableParams, ()>),
    /// Guest writes status=0 — stop all queues, reset device.
    Disable(Rpc<(), ()>),
    /// ChangeDeviceState::stop() — stop queues, return states for resume.
    Stop(Rpc<(), Vec<Option<QueueState>>>),
    /// ChangeDeviceState::start() — restart queues with saved states.
    Start(FailableRpc<StartParams, ()>),
    /// ChangeDeviceState::reset() — stop queues, reset device.
    Reset(Rpc<(), ()>),
    /// Config register read (u32 at offset). Completed via DeferredRead.
    ReadConfig { offset: u16, deferred: DeferredRead },
    /// Config register write (u32 at offset). Completed via DeferredWrite.
    WriteConfig {
        offset: u16,
        val: u32,
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
#[derive(Inspect)]
#[inspect(tag = "state")]
pub enum TransportState {
    Ready,
    Enabling {
        #[inspect(skip)]
        recv: PendingFailableRpc<(), RemoteError>,
    },
    Disabling {
        #[inspect(skip)]
        recv: PendingRpc<()>,
    },
}

/// Result from polling the transport state machine.
pub enum TransportStateResult {
    EnableComplete(Result<(), ()>),
    DisableComplete,
}

impl TransportState {
    pub fn is_busy(&self) -> bool {
        !matches!(self, TransportState::Ready)
    }

    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<TransportStateResult> {
        match self {
            TransportState::Ready => Poll::Pending,
            TransportState::Enabling { recv } => {
                let result = std::task::ready!(Pin::new(recv).poll(cx));
                *self = TransportState::Ready;
                Poll::Ready(TransportStateResult::EnableComplete(result.map_err(|_| ())))
            }
            TransportState::Disabling { recv } => {
                let _ = std::task::ready!(Pin::new(recv).poll(cx));
                *self = TransportState::Ready;
                Poll::Ready(TransportStateResult::DisableComplete)
            }
        }
    }

    /// Wait for any in-flight enable or disable to complete, returning
    /// whether a disable was in progress.
    pub async fn drain(&mut self) -> bool {
        match std::mem::replace(self, TransportState::Ready) {
            TransportState::Enabling { recv } => {
                let _ = recv.await;
                false
            }
            TransportState::Disabling { recv } => {
                let _ = recv.await;
                true
            }
            TransportState::Ready => false,
        }
    }
}

/// Result of a register read, which may be synchronous or deferred to the
/// device task (for device-config registers).
pub enum ReadResult {
    Value(u32),
    Defer(IoResult),
}

/// Result of a register write, which may be synchronous or deferred to the
/// device task (for device-config registers).
pub enum WriteResult {
    Ok,
    Defer(IoResult),
}

/// Owns the virtio device and processes commands from the transport.
struct DeviceTask {
    device: Box<dyn VirtioDevice>,
    max_queues: u16,
}

impl DeviceTask {
    async fn enable(&mut self, params: EnableParams) -> anyhow::Result<()> {
        for (idx, resources) in params.queues {
            if let Err(err) = self
                .device
                .start_queue(idx, resources, &params.features, None)
            {
                tracelimit::error_ratelimited!(
                    error = &*err as &dyn std::error::Error,
                    idx,
                    "virtio device start_queue failed"
                );
                self.stop_all_queues().await;
                self.device.reset();
                return Err(err);
            }
        }
        Ok(())
    }

    async fn disable(&mut self) {
        self.stop_all_queues().await;
        self.device.reset();
    }

    async fn stop(&mut self) -> Vec<Option<QueueState>> {
        let mut states = vec![None; self.max_queues as usize];
        for idx in 0..self.max_queues {
            states[idx as usize] = poll_fn(|cx| self.device.poll_stop_queue(cx, idx)).await;
        }
        states
    }

    fn start(&mut self, params: StartParams) -> anyhow::Result<()> {
        for (idx, resources, initial_state) in params.queues {
            self.device
                .start_queue(idx, resources, &params.features, initial_state)
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
        self.device.reset();
    }

    async fn stop_all_queues(&mut self) {
        for idx in 0..self.max_queues {
            poll_fn(|cx| self.device.poll_stop_queue(cx, idx)).await;
        }
    }
}

/// Runs the device task, processing commands from the transport.
pub async fn run_device_task(
    device: Box<dyn VirtioDevice>,
    mut recv: mesh::Receiver<DeviceCommand>,
) {
    let mut task = DeviceTask {
        max_queues: device.traits().max_queues,
        device,
    };

    while let Some(cmd) = recv.next().await {
        match cmd {
            DeviceCommand::Enable(rpc) => {
                rpc.handle_failable(async |params| task.enable(params).await)
                    .await;
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
                rpc.handle_failable_sync(|params| task.start(params));
            }
            DeviceCommand::Reset(rpc) => {
                rpc.handle(async |()| task.reset().await).await;
            }
            DeviceCommand::ReadConfig { offset, deferred } => {
                let val = task.device.read_registers_u32(offset);
                deferred.complete(&val.to_ne_bytes());
            }
            DeviceCommand::WriteConfig {
                offset,
                val,
                deferred,
            } => {
                task.device.write_registers_u32(offset, val);
                deferred.complete();
            }
            DeviceCommand::Inspect(deferred) => {
                deferred.inspect(&mut *task.device);
            }
        }
    }
}

/// Send Enable command, return pending result to poll.
pub fn send_enable(
    sender: &mesh::Sender<DeviceCommand>,
    queues: Vec<(u16, QueueResources)>,
    features: VirtioDeviceFeatures,
) -> PendingFailableRpc<(), RemoteError> {
    sender.call_failable(
        DeviceCommand::Enable,
        EnableParams {
            queues,
            features,
        },
    )
}

/// Send Disable command, return pending result to poll.
pub fn send_disable(sender: &mesh::Sender<DeviceCommand>) -> PendingRpc<()> {
    sender.call(DeviceCommand::Disable, ())
}

/// Send a config read to the device task, returning a deferred IO token.
pub fn defer_config_read(sender: &mesh::Sender<DeviceCommand>, offset: u16) -> IoResult {
    let (deferred, token) = defer_read();
    sender.send(DeviceCommand::ReadConfig { offset, deferred });
    IoResult::Defer(token)
}

/// Send a config write to the device task, returning a deferred IO token.
pub fn defer_config_write(sender: &mesh::Sender<DeviceCommand>, offset: u16, val: u32) -> IoResult {
    let (deferred, token) = defer_write();
    sender.send(DeviceCommand::WriteConfig {
        offset,
        val,
        deferred,
    });
    IoResult::Defer(token)
}
