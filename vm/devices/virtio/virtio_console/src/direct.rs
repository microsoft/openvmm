// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The direct forwarding mode of the console worker.
//!
//! In direct mode the worker forwards data between the virtio queues and a
//! [`SerialIo`] backend.

use crate::BUF_SIZE;
use crate::WorkerError;
use futures::AsyncRead;
use guestmem::GuestMemory;
use serial_core::SerialIo;
use std::future::poll_fn;
use std::pin::Pin;
use virtio::VirtioQueue;

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
