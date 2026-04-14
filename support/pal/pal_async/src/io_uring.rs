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
    /// All memory referenced by the SQE must remain valid for the
    /// lifetime of the returned future.
    ///
    /// The abort-on-drop guard makes it sound to reference
    /// non-`'static` memory (such as locals in an enclosing `async
    /// fn`): either the IO completes normally, or the future is
    /// dropped and the process aborts before the referenced memory
    /// is freed.
    ///
    /// Leaking the returned future via [`std::mem::forget`] or a
    /// reference-count cycle bypasses the abort guard. This is
    /// undefined behavior if the SQE references non-`'static`
    /// memory that is not otherwise kept alive.
    unsafe fn submit(
        &self,
        sqe: squeue::Entry,
    ) -> Pin<Box<dyn Future<Output = io::Result<i32>> + Send + '_>>;
}
