// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Console worker modes and the direct forwarding mode.
//!
//! The worker runs in a [`ConsoleWorkerMode`]: direct forwarding between the
//! virtio queues and a [`SerialIo`] backend. In direct mode a
//! [`VirtioConsoleDisconnectPolicy`] selects whether guest output is discarded
//! or retained while the backend is disconnected, and host input is staged in
//! the worker state before it is copied to a guest buffer, so input already
//! accepted from the backend survives worker restarts and snapshots.

use crate::BUF_SIZE;
use crate::ConsoleWorker;
use crate::ConsoleWorkerState;
use crate::VirtioConsoleDevice;
use crate::WorkerError;
use crate::spec::VirtioConsoleConfig;
use futures::AsyncRead;
use guestmem::GuestMemory;
use inspect::InspectMut;
use serial_core::SerialIo;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Poll;
use task_control::TaskControl;
use virtio::VirtioQueue;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use vmcore::vm_task::VmTaskDriverSource;

pub(crate) enum ConsoleWorkerMode {
    Direct {
        io: Box<dyn SerialIo>,
        disconnect_policy: VirtioConsoleDisconnectPolicy,
    },
}

impl InspectMut for ConsoleWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        let ConsoleWorkerMode::Direct { io, .. } = &mut self.mode;
        req.respond().field("mode", "direct").field_mut("io", io);
    }
}

impl VirtioConsoleDevice {
    /// Create a console with explicit behavior while its backend is disconnected.
    pub fn new_with_policy(
        driver_source: &VmTaskDriverSource,
        io: Box<dyn SerialIo>,
        disconnect_policy: VirtioConsoleDisconnectPolicy,
    ) -> Self {
        Self::new_with_name_and_policy(driver_source, io, "virtio-console", disconnect_policy)
    }

    /// Create a named console with explicit disconnected-backend behavior.
    pub fn new_with_name_and_policy(
        driver_source: &VmTaskDriverSource,
        io: Box<dyn SerialIo>,
        worker_name: &'static str,
        disconnect_policy: VirtioConsoleDisconnectPolicy,
    ) -> Self {
        let driver = driver_source.simple();
        let mut worker = TaskControl::new(ConsoleWorker {
            mode: ConsoleWorkerMode::Direct {
                io,
                disconnect_policy,
            },
        });
        worker.insert(
            &driver,
            worker_name,
            ConsoleWorkerState {
                receiveq: None,
                transmitq: None,
                mem: GuestMemory::empty(),
                partial_transmit: 0,
                staged_rx: VecDeque::new(),
                input_gated: false,
            },
        );
        Self {
            driver,
            config: VirtioConsoleConfig::default(),
            worker,
        }
    }
}

/// Receive half of the direct forwarding loop.
///
/// Reads host input into `staged_rx` before copying it to guest receive
/// buffers, so that accepted input survives a cancel and restart of the worker.
/// Returns `Ok(false)` when the backend disconnects. While `input_gated` is
/// set, no host input is read.
pub(crate) async fn receive(
    receiveq: &mut Option<VirtioQueue>,
    io: &parking_lot::Mutex<&mut Box<dyn SerialIo>>,
    mem: &GuestMemory,
    staged_rx: &mut VecDeque<u8>,
    input_gated: bool,
) -> Result<bool, WorkerError> {
    if input_gated {
        return std::future::pending().await;
    }
    'rx: loop {
        if staged_rx.is_empty() {
            let mut buf = [0u8; BUF_SIZE];
            let read = poll_fn(|cx| {
                if let Some(receiveq) = receiveq.as_mut() {
                    loop {
                        match receiveq.try_peek() {
                            Ok(Some(work)) => {
                                let writeable_len = work
                                    .payload()
                                    .iter()
                                    .filter(|payload| payload.writeable)
                                    .map(|payload| payload.length as usize)
                                    .sum::<usize>();
                                if writeable_len != 0 {
                                    break;
                                }
                                let work = work.consume();
                                receiveq.complete(work, 0);
                            }
                            Ok(None) => {
                                let _ = receiveq.poll_kick(cx);
                                break;
                            }
                            Err(error) => {
                                return Poll::Ready(Err(WorkerError::Virtio(error)));
                            }
                        }
                    }
                }

                Pin::new(&mut **io.lock())
                    .poll_read(cx, &mut buf)
                    .map(|result| result.map_err(WorkerError::Serial))
            })
            .await;
            let read = match read {
                Ok(read) => read,
                Err(WorkerError::Serial(_)) => break 'rx Ok(false),
                Err(error) => return Err(error),
            };
            if read == 0 {
                break 'rx Ok(false);
            }
            staged_rx.extend(&buf[..read]);
        }

        let Some(receiveq) = receiveq.as_mut() else {
            std::future::pending().await
        };
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
        let n = staged_rx.len().min(writeable_len);
        let work = work.consume();
        if let Err(err) = work.write(mem, &staged_rx.make_contiguous()[..n]) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "failed to write to guest receive buffer"
            );
            receiveq.complete(work, 0);
        } else {
            staged_rx.drain(..n);
            receiveq.complete(work, n as u32);
        }
    }
}
