// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio console device — a single-port console backed by [`SerialIo`].
//!
//! This crate implements virtio device ID 3 (console) as defined in the
//! [virtio spec §5.3](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).
//! It exposes `/dev/hvc0` inside the guest and bridges it to any
//! [`SerialIo`] backend (Unix socket, named pipe, in-memory buffer, etc.).
//!
//! # Queues
//!
//! The device uses two virtio queues:
//!
//! | Queue | Direction | Purpose |
//! |-------|-----------|---------|
//! | 0 — receiveq | host → guest | Data written by the backend appears here |
//! | 1 — transmitq | guest → host | Data written by the guest is forwarded to the backend |
//!
//! # Features
//!
//! * **`F_SIZE`** — advertised so the guest can query the console dimensions
//!   (columns × rows) from config space.
//! * **`F_MULTIPORT`** — *not* supported. This is a single-port implementation.
//!
//! # Disconnect / reconnect
//!
//! When the [`SerialIo`] backend disconnects (i.e. `poll_read` returns
//! `Ok(0)`), the worker drains any pending guest TX descriptors without
//! forwarding them. Once `poll_connect` resolves, normal bidirectional
//! forwarding resumes.
//! [`VirtioConsoleDevice::new_with_policy`] can instead retain pending guest
//! TX descriptors until the backend reconnects.

#![forbid(unsafe_code)]

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the broker worker drives the state machine")
)]
pub(crate) mod control_session_broker;
pub(crate) mod control_session_protocol;
mod direct;
pub mod resolver;
mod saved_state;
mod spec;
#[cfg(test)]
mod tests;

use futures::AsyncWrite;
use futures_concurrency::future::Race as _;
use guestmem::GuestMemory;
use inspect::InspectMut;
use serial_core::SerialIo;
use spec::VIRTIO_CONSOLE_F_SIZE;
use spec::VirtioConsoleConfig;
use std::future::poll_fn;
use std::pin::Pin;
use std::pin::pin;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

/// A virtio console device backed by a [`SerialIo`] backend.
#[derive(InspectMut)]
pub struct VirtioConsoleDevice {
    driver: VmTaskDriver,
    config: VirtioConsoleConfig,
    #[inspect(mut)]
    worker: TaskControl<ConsoleWorker, ConsoleWorkerState>,
}

impl VirtioConsoleDevice {
    /// Create a new virtio console device backed by the given serial I/O.
    pub fn new(driver_source: &VmTaskDriverSource, io: Box<dyn SerialIo>) -> Self {
        Self::new_with_policy(driver_source, io, VirtioConsoleDisconnectPolicy::Discard)
    }
}

impl VirtioDevice for VirtioConsoleDevice {
    fn traits(&self) -> DeviceTraits {
        let features = VirtioDeviceFeatures::new()
            .with_device_specific_low(1 << VIRTIO_CONSOLE_F_SIZE)
            .with_ring_event_idx(true)
            .with_ring_indirect_desc(true)
            .with_ring_packed(true);
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::CONSOLE,
            device_features: features,
            max_queues: 2, // receiveq (0) + transmitq (1)
            device_register_length: size_of::<VirtioConsoleConfig>() as u32,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        self.config.read_u32(offset)
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {
        // Console config is read-only from the guest perspective.
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        let guest_memory = resources.guest_memory.clone();
        let mut queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory,
            resources.notify,
            pal_async::wait::PolledWait::new(&self.driver, resources.event)?,
            initial_state,
        )?;

        anyhow::ensure!(idx < 2, "invalid virtio-console queue index {idx}");

        self.worker.stop().await;
        let state = self.worker.state_mut().unwrap();
        saved_state::check_restored_tx_offset(idx, state.partial_transmit, &mut queue)?;
        state.mem = guest_memory;
        if idx == 0 {
            state.receiveq = Some(queue);
        } else {
            state.transmitq = Some(queue);
        }
        self.worker.start();
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        if !self.worker.has_state() {
            return None;
        }

        // Stop the worker (shared by both queues). Once stopped, we can
        // reach into the state to take the requested queue.
        self.worker.stop().await;

        let state = self.worker.state_mut().unwrap();
        let queue = match idx {
            0 => state.receiveq.take(),
            1 => state.transmitq.take(),
            _ => return None,
        };

        // Keep the stopped worker state when both queues are gone so private
        // TX/RX progress remains available to snapshot capture.
        if state.receiveq.is_some() || state.transmitq.is_some() {
            self.worker.start();
        }

        queue.map(|q| q.queue_state())
    }

    async fn reset(&mut self) {
        self.reset_private_state();
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.set_input_gated(true).await
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.set_input_gated(false).await
    }

    fn supports_save_restore(&self) -> bool {
        true
    }

    fn save_device(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        self.save_private_state()
    }

    fn restore_device(&mut self, state: Option<SavedStateBlob>) -> Result<(), RestoreError> {
        self.restore_private_state(state)
    }

    fn device_state_validator(&self) -> virtio::device::saved_state::DeviceStateValidator {
        self.private_state_validator()
    }
}

struct ConsoleWorker {
    mode: direct::ConsoleWorkerMode,
}

#[derive(InspectMut)]
struct ConsoleWorkerState {
    receiveq: Option<VirtioQueue>,
    transmitq: Option<VirtioQueue>,
    mem: GuestMemory,
    /// Bytes already written for the current transmitq descriptor.
    /// Must survive cancel/restart to avoid re-sending data.
    partial_transmit: usize,
    #[inspect(with = "std::collections::VecDeque::len")]
    staged_rx: std::collections::VecDeque<u8>,
    input_gated: bool,
}

impl InspectTaskMut<ConsoleWorkerState> for ConsoleWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut ConsoleWorkerState>) {
        req.respond().merge(self).merge(state);
    }
}

impl AsyncRun<ConsoleWorkerState> for ConsoleWorker {
    async fn run(
        &mut self,
        stop: &mut task_control::StopTask<'_>,
        state: &mut ConsoleWorkerState,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(self.run_loop(state)).await.map(|r| {
            if let Err(err) = r {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "virtio-console worker loop failed"
                );
            }
        })
    }
}

/// Maximum buffer size for a single read/write operation.
const BUF_SIZE: usize = 4096;

#[derive(Debug, thiserror::Error)]
enum WorkerError {
    #[error("virtio queue error")]
    Virtio(#[source] std::io::Error),
    #[error("serial I/O error")]
    Serial(#[source] std::io::Error),
    #[error("guest memory error")]
    GuestMemory(#[source] guestmem::GuestMemoryError),
}

impl ConsoleWorker {
    /// Core worker loop.
    ///
    /// Note that this must be cancel safe--it could be stopped at any await point.
    /// So, be careful not to leave any state in a weird intermediate state across
    /// an await point.
    async fn run_loop(&mut self, state: &mut ConsoleWorkerState) -> Result<(), WorkerError> {
        let direct::ConsoleWorkerMode::Direct {
            io: serial_io,
            disconnect_policy,
        } = &mut self.mode;
        let disconnect_policy = *disconnect_policy;
        let mut connected: bool = serial_io.is_connected();
        let receiveq = &mut state.receiveq;
        let transmitq = &mut state.transmitq;
        let mut io = parking_lot::Mutex::new(serial_io);
        let mem = &state.mem;
        let partial_transmit = &mut state.partial_transmit;
        let staged_rx = &mut state.staged_rx;
        let input_gated = state.input_gated;

        // If neither queue is present, there's nothing to do.
        if receiveq.is_none() && transmitq.is_none() {
            std::future::pending::<()>().await;
        }
        loop {
            if !connected {
                poll_fn(|cx| io.get_mut().poll_disconnect(cx))
                    .await
                    .map_err(WorkerError::Serial)?;
                // Wait for the backend to connect, discarding any guest tx data
                // in the meantime.
                let wait_connect = async {
                    poll_fn(|cx| io.get_mut().poll_connect(cx))
                        .await
                        .map_err(WorkerError::Serial)?;
                    Ok::<_, WorkerError>(true)
                };
                let drain_tx = async {
                    if disconnect_policy == VirtioConsoleDisconnectPolicy::Retain {
                        return std::future::pending().await;
                    }
                    let Some(transmitq) = transmitq.as_mut() else {
                        std::future::pending().await
                    };
                    loop {
                        let work = transmitq.peek().await.map_err(WorkerError::Virtio)?;
                        let work = work.consume();
                        transmitq.complete(work, 0);
                        *partial_transmit = 0;
                    }
                };
                // Give wait_connect priority so that drain_tx cannot
                // consume a descriptor on the same poll cycle where
                // the backend becomes connected.
                connected = match futures::future::select(pin!(wait_connect), pin!(drain_tx)).await
                {
                    futures::future::Either::Left((result, _))
                    | futures::future::Either::Right((result, _)) => result?,
                };
            } else {
                let rx = direct::receive(receiveq, &io, mem, staged_rx, input_gated);
                let tx = async {
                    let Some(transmitq) = transmitq.as_mut() else {
                        std::future::pending().await
                    };
                    'tx: loop {
                        let work = transmitq.peek().await.map_err(WorkerError::Virtio)?;
                        let readable_len = work.readable_length() as usize;
                        let mut buf = [0u8; BUF_SIZE];
                        while *partial_transmit < readable_len {
                            let n = work
                                .read_at_offset(*partial_transmit as u64, mem, &mut buf)
                                .map_err(WorkerError::GuestMemory)?;
                            let mut written_this_chunk = 0;
                            while written_this_chunk < n {
                                match poll_fn(|cx| {
                                    Pin::new(&mut **io.lock())
                                        .poll_write(cx, &buf[written_this_chunk..n])
                                })
                                .await
                                {
                                    Ok(0) => {
                                        break 'tx Ok(false);
                                    }
                                    Ok(written) => {
                                        written_this_chunk += written;
                                        *partial_transmit += written;
                                    }
                                    Err(_) => {
                                        // Backend disconnected. Leave
                                        // partial_transmit as-is so we can
                                        // resume if the backend reconnects
                                        // before the descriptor is drained.
                                        break 'tx Ok(false);
                                    }
                                }
                            }
                        }
                        *partial_transmit = 0;
                        let work = work.consume();
                        transmitq.complete(work, 0);
                    }
                };

                // Run rx and tx concurrently; if either signals disconnect, loop
                // back to the disconnected state.
                connected = (rx, tx).race().await?;
            }
        }
    }
}
