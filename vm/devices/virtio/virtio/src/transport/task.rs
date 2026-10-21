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
use chipset_device::io::deferred::DeferredToken;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use futures::StreamExt;
use inspect::Inspect;
use mesh::rpc::FailableRpc;
use mesh::rpc::PendingRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pci_core::capabilities::msix::MsixInterrupt;
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
    /// Drop the stopped backend and exit, acknowledging after destruction.
    Teardown(Rpc<(), ()>),
    /// Config register read at byte offset with byte length.
    ReadConfig {
        offset: u16,
        len: u8,
        completion: ConfigReadCompletion,
        deferred: DeferredRead,
    },
    /// Config register write at byte offset with raw data.
    WriteConfig {
        offset: u16,
        len: u8,
        data: [u8; 8],
        deferred: DeferredWrite,
    },
    /// Finish a queue mapping and any STATUS update from the same write.
    MapInterrupt {
        interrupt: MsixInterrupt,
        status: Option<DeferredToken>,
        deferred: DeferredWrite,
    },
    /// Inspect the device state.
    Inspect(inspect::Deferred),
}

/// How a deferred config read completion should be sized and positioned.
#[derive(Copy, Clone, Debug)]
pub enum ConfigReadCompletion {
    /// Complete exactly `len` bytes at their natural position — used when the
    /// caller polls the deferred read with a `len`-sized buffer (direct MMIO).
    Exact,
    /// Complete a full 4-byte dword with the accessed `len` bytes left-aligned
    /// into the low bytes — used by the `VIRTIO_PCI_CAP_PCI_CFG` `pci_cfg_data`
    /// window, which the PCI config bus polls with a 4-byte dword buffer. Per
    /// the virtio spec the accessed bytes occupy the first `cap.length` bytes.
    LeftAlignedDword,
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
/// When the guest writes DRIVER_OK, the transport sends an Enable RPC to
/// the device task and transitions to `Enabling`.  The guest's MMIO/PCI
/// write is deferred (via [`chipset_device::io::IoResult::Defer`]) so the
/// writing VCPU blocks until the enable completes.  Concurrent transport
/// register accesses from other VCPUs are stalled and replayed once the
/// operation finishes.  Device-config register accesses are not stalled —
/// they are forwarded to the device task via the channel and serialized
/// with Enable/Disable naturally.
///
/// Similarly, when the guest writes STATUS=0 with queues active, a Disable
/// RPC is sent and the write is deferred until teardown is complete.
#[derive(Inspect)]
#[inspect(tag = "state")]
pub enum TransportState {
    Ready,
    Enabling {
        #[inspect(skip)]
        rpc: PendingRpc<bool>,
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
        *self = TransportState::Enabling { rpc };
    }

    /// Send Disable to the device task and transition to `Disabling`.
    ///
    /// Panics if the transport is not `Ready`.
    pub fn start_disable(&mut self, sender: &mesh::Sender<DeviceCommand>) {
        assert!(!self.is_busy());
        let rpc = sender.call(DeviceCommand::Disable, ());
        *self = TransportState::Disabling { rpc };
    }

    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<TransportStateResult> {
        match self {
            TransportState::Ready => Poll::Pending,
            TransportState::Enabling { rpc } => {
                let result = std::task::ready!(Pin::new(rpc).poll(cx));
                *self = TransportState::Ready;
                Poll::Ready(TransportStateResult::EnableComplete(
                    result.unwrap_or(false),
                ))
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
    /// `poll_device`.
    pub async fn drain(&mut self) -> Option<TransportStateResult> {
        match std::mem::replace(self, TransportState::Ready) {
            TransportState::Enabling { rpc } => {
                let result = rpc.await.unwrap_or(false);
                Some(TransportStateResult::EnableComplete(result))
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
    recv: mesh::Receiver<DeviceCommand>,
    interrupts: Vec<MsixInterrupt>,
) {
    // Keep software interrupts progressing while commands await device IO.
    let events = std::future::poll_fn(|cx| {
        for interrupt in &interrupts {
            if interrupt.poll_event(cx).is_ready() {
                cx.waker().wake_by_ref();
            }
        }
        Poll::<()>::Pending
    });
    let commands = std::pin::pin!(run_device_commands(device, recv, &interrupts));
    let events = std::pin::pin!(events);
    futures::future::select(commands, events).await;
}

async fn run_device_commands(
    device: Box<dyn DynVirtioDevice>,
    mut recv: mesh::Receiver<DeviceCommand>,
    interrupts: &[MsixInterrupt],
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
                rpc.handle(async |()| {
                    let states = task.stop().await;
                    for interrupt in interrupts {
                        std::future::poll_fn(|cx| interrupt.poll_drain(cx)).await;
                    }
                    states
                })
                .await;
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
            DeviceCommand::Teardown(rpc) => {
                drop(task);
                rpc.complete(());
                return;
            }
            DeviceCommand::MapInterrupt {
                interrupt,
                status,
                deferred,
            } => {
                std::future::poll_fn(|cx| interrupt.poll_mapping(cx)).await;
                if let Some(status) = status
                    && let Err(error) = status.write_future().await
                {
                    deferred.complete_error(error);
                    continue;
                }
                deferred.complete();
            }
            DeviceCommand::ReadConfig {
                offset,
                len,
                completion,
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
                let data = &buf[byte_off..byte_off + len as usize];
                match completion {
                    ConfigReadCompletion::Exact => deferred.complete(data),
                    ConfigReadCompletion::LeftAlignedDword => {
                        let mut dword = [0u8; 4];
                        dword[..len as usize].copy_from_slice(data);
                        deferred.complete(&dword);
                    }
                }
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
pub fn defer_config_read(
    sender: &mesh::Sender<DeviceCommand>,
    offset: u16,
    len: u8,
    completion: ConfigReadCompletion,
) -> IoResult {
    let (deferred, token) = defer_read();
    sender.send(DeviceCommand::ReadConfig {
        offset,
        len,
        completion,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceTraits;
    use crate::VirtioDevice;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pci_core::capabilities::msix::MsixEmulator;
    use pci_core::msi::MsiConnection;
    use std::future::Future;
    use std::future::poll_fn;

    #[derive(InspectMut)]
    struct IdleDevice;

    impl VirtioDevice for IdleDevice {
        fn traits(&self) -> DeviceTraits {
            DeviceTraits::default()
        }

        async fn read_registers_u32(&mut self, _: u16) -> u32 {
            0
        }

        async fn write_registers_u32(&mut self, _: u16, _: u32) {}

        async fn start_queue(
            &mut self,
            _: u16,
            _: QueueResources,
            _: &VirtioDeviceFeatures,
            _: Option<QueueState>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn stop_queue(&mut self, _: u16) -> Option<QueueState> {
            None
        }
    }

    #[async_test]
    async fn stop_drains_software_interrupt_before_acknowledging(driver: DefaultDriver) {
        use vmcore::save_restore::ProtobufSaveRestore;

        for vector in [Some(0), None] {
            let conn = MsiConnection::new();
            let (msix, mut cap) = MsixEmulator::new(2, 1, &conn.target());
            let source = msix.new_interrupt(&driver).unwrap();
            assert!(source.set_vector(vector));
            let interrupt = source.interrupt();
            let (event, proxy) = interrupt.event_or_proxy(&driver).unwrap();
            assert!(proxy.is_none());

            let (sender, receiver) = mesh::channel();
            let mut stopped = sender.call(DeviceCommand::Stop, ());
            let interrupts = [source];
            let mut commands = std::pin::pin!(run_device_commands(
                Box::new(IdleDevice),
                receiver,
                &interrupts
            ));
            event.signal();
            poll_fn(|cx| {
                assert!(commands.as_mut().poll(cx).is_pending());
                Pin::new(&mut stopped).poll(cx)
            })
            .await
            .unwrap();

            let saved = cap.save().unwrap();
            let (restored, mut restored_cap) = MsixEmulator::new(2, 1, &conn.target());
            restored_cap.restore(saved).unwrap();
            assert_eq!(restored.read_u32(16), u32::from(vector.is_some()));
            assert!(!event.try_wait());
        }
    }

    #[async_test]
    async fn software_interrupt_while_mapping_waits_for_status(driver: DefaultDriver) {
        let conn = MsiConnection::new();
        let (msix, _cap) = MsixEmulator::new(2, 1, &conn.target());
        let source = msix.new_interrupt(&driver).unwrap();
        assert!(source.set_vector(Some(0)));
        let interrupt = source.interrupt();
        let (event, proxy) = interrupt.event_or_proxy(&driver).unwrap();
        assert!(proxy.is_none());

        let (sender, receiver) = mesh::channel();
        let (status, status_token) = defer_write();
        let (deferred, mut token) = defer_write();
        sender.send(DeviceCommand::MapInterrupt {
            interrupt: source.clone(),
            status: Some(status_token),
            deferred,
        });
        let mut task = std::pin::pin!(run_device_task(
            Box::new(IdleDevice),
            receiver,
            vec![source]
        ));
        let mut timer = pal_async::timer::PolledTimer::new(&driver);
        let mut timeout = std::pin::pin!(timer.sleep(std::time::Duration::from_secs(5)));
        event.signal();
        poll_fn(|cx| {
            assert!(
                timeout.as_mut().poll(cx).is_pending(),
                "software interrupt stalled"
            );
            assert!(task.as_mut().poll(cx).is_pending());
            assert!(token.poll_write(cx).is_pending());
            if msix.read_u32(16) == 1 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        status.complete();
        poll_fn(|cx| {
            assert!(task.as_mut().poll(cx).is_pending());
            token.poll_write(cx)
        })
        .await
        .unwrap();
    }
}
