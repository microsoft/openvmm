// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! io-uring submission trait.

// UNSAFETY: The `IoUringSubmit` trait has an unsafe method for submitting SQEs.
#![expect(unsafe_code)]

use io_uring::squeue;
use std::future::Future;
use std::io;
use std::pin::Pin;

/// Trait for submitting io-uring operations.
pub trait IoUringSubmit: Send + Sync {
    /// Returns whether the given opcode is supported by the ring.
    fn probe(&self, opcode: u8) -> bool;

    /// Submits an io-uring SQE for asynchronous execution.
    ///
    /// Returns a future that completes with the IO result. The future
    /// **aborts the process** if dropped while the IO is in flight,
    /// since there is no way to synchronously cancel an in-flight
    /// io-uring operation.
    ///
    /// # Safety
    ///
    /// The SQE must only reference memory with `'static` lifetime.
    ///
    /// In particular, the SQE must **not** reference borrowed or
    /// stack-allocated memory, even though the abort-on-drop guard would
    /// normally prevent use-after-free on future drop. The guard does not
    /// protect against [`std::mem::forget`]: if the caller forgets the
    /// returned future, the IO will complete after the borrowed memory is
    /// freed, causing undefined behavior.
    ///
    /// Callers that need to use non-`'static` buffers must ensure the
    /// buffer outlives the IO through some other mechanism (e.g., owning
    /// the buffer in a pinned enclosing future that itself aborts on
    /// drop).
    unsafe fn submit(
        &self,
        sqe: squeue::Entry,
    ) -> Pin<Box<dyn Future<Output = io::Result<i32>> + Send + '_>>;
}
