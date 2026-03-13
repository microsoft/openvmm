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
use spec::VIRTIO_DEVICE_ID_CONSOLE;
use spec::VirtioConsoleConfig;
use std::future::poll_fn;
use std::pin::Pin;
use std::pin::pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

/// A virtio console device backed by a [`SerialIo`] backend.
#[derive(InspectMut)]
pub struct VirtioConsoleDevice {
    driver: VmTaskDriver,
    config: VirtioConsoleConfig,
    #[inspect(skip)]
    io: Option<Box<dyn SerialIo>>,
    #[inspect(skip)]
    worker: Option<TaskControl<ConsoleWorker, ConsoleWorkerState>>,
    memory: GuestMemory,
}

impl VirtioConsoleDevice {
    /// Create a new virtio console device backed by the given serial I/O.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        memory: GuestMemory,
        io: Box<dyn SerialIo>,
    ) -> Self {
        Self {
            driver: driver_source.simple(),
            config: VirtioConsoleConfig::default(),
            io: Some(io),
            worker: None,
            memory,
        }
    }
}

impl VirtioDevice for VirtioConsoleDevice {
    fn traits(&self) -> DeviceTraits {
        let mut features = VirtioDeviceFeatures::new();
        features.set_bank(0, 1 << VIRTIO_CONSOLE_F_SIZE);
        DeviceTraits {
            device_id: VIRTIO_DEVICE_ID_CONSOLE,
            device_features: features,
            max_queues: 2, // receiveq (0) + transmitq (1)
            device_register_length: size_of::<VirtioConsoleConfig>() as u32,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    fn read_registers_u32(&self, offset: u16) -> u32 {
        self.config.read_u32(offset)
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {
        // Console config is read-only from the guest perspective.
    }

    fn enable(&mut self, resources: Resources) -> anyhow::Result<()> {
        assert!(self.worker.is_none());

        // Both queues must be enabled.
        if !resources.queues[0].params.enable || !resources.queues[1].params.enable {
            return Ok(());
        }

        let io = self.io.take().expect("io should be present when enabling");

        let receiveq = VirtioQueue::new(
            resources.features.clone(),
            resources.queues[0].params,
            self.memory.clone(),
            resources.queues[0].notify.clone(),
            pal_async::wait::PolledWait::new(&self.driver, resources.queues[0].event.clone())?,
        )?;

        let transmitq = VirtioQueue::new(
            resources.features,
            resources.queues[1].params,
            self.memory.clone(),
            resources.queues[1].notify.clone(),
            pal_async::wait::PolledWait::new(&self.driver, resources.queues[1].event.clone())?,
        )?;

        let mut task = TaskControl::new(ConsoleWorker);
        task.insert(
            &self.driver,
            "virtio-console",
            ConsoleWorkerState {
                io,
                receiveq,
                transmitq,
                mem: self.memory.clone(),
                partial_transmit: 0,
            },
        );
        task.start();
        self.worker = Some(task);
        Ok(())
    }

    fn poll_disable(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(worker) = &mut self.worker {
            ready!(worker.poll_stop(cx));
        }
        if let Some(mut worker) = self.worker.take() {
            let state = worker.remove();
            self.io = Some(state.io);
        }
        Poll::Ready(())
    }
}

#[derive(InspectMut)]
struct ConsoleWorker;

#[derive(InspectMut)]
struct ConsoleWorkerState {
    #[inspect(mut)]
    io: Box<dyn SerialIo>,
    receiveq: VirtioQueue,
    transmitq: VirtioQueue,
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
        stop.until_stopped(console_worker_loop(state))
            .await
            .map(|r| {
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
    Queue(#[source] std::io::Error),
    #[error("guest memory error")]
    GuestMemory(#[source] guestmem::GuestMemoryError),
}

/// Core worker loop driven.
///
/// Note that this must be cancel safe--it could be stopped at any await point.
/// So, be careful not to leave any state in a weird intermediate state across
/// an await point.
async fn console_worker_loop(state: &mut ConsoleWorkerState) -> Result<(), WorkerError> {
    let mut connected: bool = state.io.is_connected();
    let receiveq = &mut state.receiveq;
    let transmitq = &mut state.transmitq;
    let mut io = parking_lot::Mutex::new(&mut state.io);
    let mem = &state.mem;
    let partial_transmit = &mut state.partial_transmit;
    loop {
        if !connected {
            // Wait for the backend to connect, discarding any guest tx data
            // in the meantime.
            let wait_connect = async {
                poll_fn(|cx| io.get_mut().poll_connect(cx))
                    .await
                    .map_err(WorkerError::Queue)?;
                Ok::<_, WorkerError>(true)
            };
            let drain_tx = async {
                loop {
                    let work = transmitq.peek().await.map_err(WorkerError::Queue)?;
                    work.consume().complete(0);
                }
            };
            // Give wait_connect priority so that drain_tx cannot
            // consume a descriptor on the same poll cycle where
            // the backend becomes connected.
            connected = match futures::future::select(pin!(wait_connect), pin!(drain_tx)).await {
                futures::future::Either::Left((result, _))
                | futures::future::Either::Right((result, _)) => result?,
            };
        } else {
            let rx = async {
                'rx: loop {
                    let work = receiveq.peek().await.map_err(WorkerError::Queue)?;
                    let writeable_len = work
                        .payload()
                        .iter()
                        .filter(|p| p.writeable)
                        .map(|p| p.length as usize)
                        .sum::<usize>();
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
                            let mut work = work.consume();
                            if let Err(err) = work.write(mem, &buf[..n]) {
                                tracelimit::error_ratelimited!(
                                    error = &err as &dyn std::error::Error,
                                    "failed to write to guest receive buffer"
                                );
                                work.complete(0);
                            } else {
                                work.complete(n as u32);
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
                'tx: loop {
                    let work = transmitq.peek().await.map_err(WorkerError::Queue)?;
                    let readable_len = work.readable_length() as usize;
                    let n = BUF_SIZE.min(readable_len);
                    let mut buf = [0u8; BUF_SIZE];
                    let n = work
                        .read(mem, &mut buf[..n])
                        .map_err(WorkerError::GuestMemory)?;
                    match poll_fn(|cx| {
                        Pin::new(&mut **io.lock()).poll_write(cx, &buf[*partial_transmit..n])
                    })
                    .await
                    {
                        Ok(written) => {
                            *partial_transmit += written;
                            if *partial_transmit >= n {
                                work.consume().complete(n as u32);
                                *partial_transmit = 0;
                            }
                        }
                        Err(_) => {
                            // Disconnect on write error.
                            break 'tx Ok(false);
                        }
                    }
                }
            };

            // Run rx and tx concurrently; if either signals disconnect, loop
            // back to the disconnected state.
            connected = (rx, tx).race().await?;
        }
    }
}
