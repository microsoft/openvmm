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

#![forbid(unsafe_code)]

pub mod resolver;
mod spec;
#[cfg(test)]
mod tests;

use futures::AsyncRead;
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
        Self {
            driver: driver_source.simple(),
            config: VirtioConsoleConfig::default(),
            worker: TaskControl::new(ConsoleWorker { io }),
        }
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
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory,
            resources.notify,
            pal_async::wait::PolledWait::new(&self.driver, resources.event)?,
            initial_state,
        )?;

        assert!(idx < 2);

        if self.worker.has_state() {
            // Worker is already running with the other queue — inject this one.
            // update_with cancels the current run iteration, applies the
            // closure, then the worker restarts.
            self.worker.update_with(move |_worker, state| {
                if let Some(state) = state {
                    if idx == 0 {
                        state.receiveq = Some(queue);
                    } else {
                        state.transmitq = Some(queue);
                    }
                }
            });
        } else {
            // First queue to start — create the worker state.
            let (receiveq, transmitq) = if idx == 0 {
                (Some(queue), None)
            } else {
                (None, Some(queue))
            };
            self.worker.insert(
                &self.driver,
                "virtio-console",
                ConsoleWorkerState {
                    receiveq,
                    transmitq,
                    mem: guest_memory,
                    partial_transmit: 0,
                },
            );
            self.worker.start();
        }
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
            _ => unreachable!(),
        };

        // If both queues have been taken, remove the worker state entirely.
        // Otherwise, restart the worker so the remaining queue stays active.
        if state.receiveq.is_none() && state.transmitq.is_none() {
            self.worker.remove();
        } else {
            self.worker.start();
        }

        queue.map(|q| q.queue_state())
    }

    async fn reset(&mut self) {}
}

#[derive(InspectMut)]
struct ConsoleWorker {
    #[inspect(mut)]
    io: Box<dyn SerialIo>,
}

#[derive(InspectMut)]
struct ConsoleWorkerState {
    receiveq: Option<VirtioQueue>,
    transmitq: Option<VirtioQueue>,
    mem: GuestMemory,
    /// Bytes already written for the current transmitq descriptor.
    /// Must survive cancel/restart to avoid re-sending data.
    partial_transmit: usize,
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
                tracing::error!(
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
        let mut connected: bool = self.io.is_connected();
        let receiveq = &mut state.receiveq;
        let transmitq = &mut state.transmitq;
        let mut io = parking_lot::Mutex::new(&mut self.io);
        let mem = &state.mem;
        let partial_transmit = &mut state.partial_transmit;

        // If neither queue is present, there's nothing to do.
        if receiveq.is_none() && transmitq.is_none() {
            std::future::pending::<()>().await;
        }
        loop {
            if !connected {
                // Wait for the backend to connect, discarding any guest tx data
                // in the meantime.
                let wait_connect = async {
                    poll_fn(|cx| io.get_mut().poll_connect(cx))
                        .await
                        .map_err(WorkerError::Serial)?;
                    Ok::<_, WorkerError>(true)
                };
                let drain_tx = async {
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
                let rx = async {
                    let Some(receiveq) = receiveq.as_mut() else {
                        std::future::pending().await
                    };
                    'rx: loop {
                        let work = receiveq.peek().await.map_err(WorkerError::Virtio)?;
                        let writeable_len = work
                            .payload()
                            .iter()
                            .filter(|p| p.writeable)
                            .map(|p| p.length as usize)
                            .sum::<usize>();
                        if writeable_len == 0 {
                            // Guest posted a zero-length buffer; complete it
                            // immediately without calling poll_read (which
                            // would return Ok(0) and look like a disconnect).
                            let work = work.consume();
                            receiveq.complete(work, 0);
                            continue 'rx;
                        }
                        let n = BUF_SIZE.min(writeable_len);
                        let mut buf = [0u8; BUF_SIZE];
                        match poll_fn(|cx| Pin::new(&mut **io.lock()).poll_read(cx, &mut buf[..n]))
                            .await
                        {
                            Ok(0) => {
                                // Backend disconnected.
                                break 'rx Ok(false);
                            }
                            Ok(n) => {
                                let work = work.consume();
                                if let Err(err) = work.write(mem, &buf[..n]) {
                                    tracelimit::error_ratelimited!(
                                        error = &err as &dyn std::error::Error,
                                        "failed to write to guest receive buffer"
                                    );
                                    receiveq.complete(work, 0);
                                } else {
                                    receiveq.complete(work, n as u32);
                                }
                            }
                            Err(_) => {
                                // Disconnect on error, like other serial impls.
                                break 'rx Ok(false);
                            }
                        }
                    }
                };
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
                                    Ok(written) => {
                                        assert!(written > 0);
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
