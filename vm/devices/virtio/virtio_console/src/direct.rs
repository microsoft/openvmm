// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Console worker modes and the direct forwarding mode.
//!
//! The worker runs in a [`ConsoleWorkerMode`]: direct forwarding between the
//! virtio queues and a [`SerialIo`] backend. In direct mode a
//! [`VirtioConsoleDisconnectPolicy`] selects whether guest output is discarded
//! or retained while the backend is disconnected.

use crate::BUF_SIZE;
use crate::ConsoleWorker;
use crate::VirtioConsoleDevice;
use crate::WorkerError;
use crate::spec::VirtioConsoleConfig;
use futures::AsyncRead;
use guestmem::GuestMemory;
use inspect::InspectMut;
use serial_core::SerialIo;
use std::future::poll_fn;
use std::pin::Pin;
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
        Self {
            driver: driver_source.simple(),
            config: VirtioConsoleConfig::default(),
            worker: TaskControl::new(ConsoleWorker {
                mode: ConsoleWorkerMode::Direct {
                    io,
                    disconnect_policy,
                },
            }),
        }
    }
}

/// Receive half of the direct forwarding loop.
///
/// Copies host input from the backend to guest receive buffers. Returns
/// `Ok(false)` when the backend disconnects.
pub(crate) async fn receive(
    receiveq: &mut Option<VirtioQueue>,
    io: &parking_lot::Mutex<&mut Box<dyn SerialIo>>,
    mem: &GuestMemory,
) -> Result<bool, WorkerError> {
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
        match poll_fn(|cx| Pin::new(&mut **io.lock()).poll_read(cx, &mut buf[..n])).await {
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
}
