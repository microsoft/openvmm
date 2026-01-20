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

use crate::io::IoError;
use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;

/// Token to return in [`IoResult::Defer`](super::IoResult::Defer) for deferred
/// IOs.
///
/// Create with [`defer_read`] or [`defer_write`].
#[derive(Debug)]
pub struct DeferredToken {
    is_read: bool,
    recv: mesh::OneshotReceiver<Result<(u64, usize), IoError>>,
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
    ) -> Poll<Result<(), IoError>> {
        assert!(self.is_read, "defer type mismatch");
        let r = ready!(Pin::new(&mut self.recv).poll(cx));
        match r {
            Ok(Ok((v, len))) => {
                assert_eq!(len, bytes.len(), "defer size mismatch");
                bytes.copy_from_slice(&v.to_ne_bytes()[..len]);
                Poll::Ready(Ok(()))
            }
            Ok(Err(e)) => Poll::Ready(Err(e)),
            Err(_) => Poll::Ready(Err(IoError::NoResponse)),
        }
    }

    /// Polls the deferred token for the results of a write operation.
    ///
    /// Panics if the deferred token was for a read operation.
    pub fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        assert!(!self.is_read, "defer type mismatch");
        let r = ready!(Pin::new(&mut self.recv).poll(cx));
        match r {
            Ok(Ok(_)) => Poll::Ready(Ok(())),
            Ok(Err(e)) => Poll::Ready(Err(e)),
            Err(_) => Poll::Ready(Err(IoError::NoResponse)),
        }
    }
}

/// A deferred read operation.
#[derive(Debug)]
pub struct DeferredRead {
    send: mesh::OneshotSender<Result<(u64, usize), IoError>>,
}

impl DeferredRead {
    /// Completes the read operation with the specified data.
    pub fn complete(self, bytes: &[u8]) {
        let mut v = [0; 8];
        v[..bytes.len()].copy_from_slice(bytes);
        self.send.send(Ok((u64::from_ne_bytes(v), bytes.len())));
    }

    /// Completes the read operation with an error.
    pub fn complete_error(self, error: IoError) {
        self.send.send(Err(error));
    }
}

/// A deferred write operation.
#[derive(Debug)]
pub struct DeferredWrite {
    send: mesh::OneshotSender<Result<(u64, usize), IoError>>,
}

impl DeferredWrite {
    /// Completes the write operation.
    pub fn complete(self) {
        self.send.send(Ok((0, 0)));
    }

    /// Completes the write operation with an error.
    pub fn complete_error(self, error: IoError) {
        self.send.send(Err(error));
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
