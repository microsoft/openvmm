// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for deferred IO, used when an IO can't be completed synchronously.
//!
//! Example:
//!
//! ```rust
//! # use chipset_device::io::{IoResult, deferred::{DeferredRead, defer_read}};
//! # use std::task::Context;
//! struct Device {
//!     deferred: Option<DeferredRead>,
//! }
//!
//! impl Device {
//!     fn read_handler(&mut self, data: &mut [u8]) -> IoResult {
//!         // Defer this request to later.
//!         let (deferred, token) = defer_read();
//!         IoResult::Defer(token.into())
//!     }
//!
//!     fn poll_device(&mut self, _cx: &mut Context<'_>) {
//!         // The data is now available, complete the request.
//!         if let Some(deferred) = self.deferred.take() {
//!             deferred.complete(&[123]);
//!         }
//!     }
//! }
//! ```

use std::pin::Pin;
use std::task::ready;
use std::task::Context;
use std::task::Poll;

/// Token to return in [`IoResult::Defer`](super::IoResult::Defer) for deferred
/// IOs.
///
/// Create with [`defer_read`] or [`defer_write`].
#[derive(Debug)]
pub struct DeferredToken {
    is_read: bool,
    recv: mesh::OneshotReceiver<(u64, usize)>,
}

impl DeferredToken {
    /// Polls the deferred token for the results of a read operation.
    ///
    /// Copies the results into `bytes`.
    ///
    /// Panics if the deferred token was for a write operation.
    pub fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &mut [u8],
    ) -> Poll<Result<(), mesh::RecvError>> {
        assert!(self.is_read, "defer type mismatch");
        let (v, len) = ready!(Pin::new(&mut self.recv).poll(cx))?;
        assert_eq!(len, bytes.len(), "defer size mismatch");
        bytes.copy_from_slice(&v.to_ne_bytes()[..len]);
        Poll::Ready(Ok(()))
    }

    /// Polls the deferred token for the results of a write operation.
    ///
    /// Panics if the deferred token was for a read operation.
    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), mesh::RecvError>> {
        assert!(!self.is_read, "defer type mismatch");
        ready!(Pin::new(&mut self.recv).poll(cx))?;
        Poll::Ready(Ok(()))
    }
}

/// A deferred read operation.
#[derive(Debug)]
pub struct DeferredRead {
    send: mesh::OneshotSender<(u64, usize)>,
}

impl DeferredRead {
    /// Completes the read operation with the specified data.
    pub fn complete(self, bytes: &[u8]) {
        let mut v = [0; 8];
        v[..bytes.len()].copy_from_slice(bytes);
        self.send.send((u64::from_ne_bytes(v), bytes.len()));
    }
}

/// A deferred write operation.
#[derive(Debug)]
pub struct DeferredWrite {
    send: mesh::OneshotSender<(u64, usize)>,
}

impl DeferredWrite {
    /// Completes the write operation.
    pub fn complete(self) {
        self.send.send((0, 0));
    }
}

/// Creates a deferred IO read operation.
pub fn defer_read() -> (DeferredRead, DeferredToken) {
    let (send, recv) = mesh::oneshot();
    (
        DeferredRead { send },
        DeferredToken {
            is_read: true,
            recv,
        },
    )
}

/// Creates a deferred IO write operation.
pub fn defer_write() -> (DeferredWrite, DeferredToken) {
    let (send, recv) = mesh::oneshot();
    (
        DeferredWrite { send },
        DeferredToken {
            is_read: false,
            recv,
        },
    )
}
