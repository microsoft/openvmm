// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! io-uring submission trait.

// UNSAFETY: The `IoUringSubmit` trait has an unsafe method for submitting SQEs.
#![expect(unsafe_code)]

pub use squeue::Entry;

use io_uring::squeue;
use std::future::Future;
use std::io;

/// Component trait for drivers that optionally support io-uring submission.
///
/// All types that participate in the `Driver` blanket impl on Linux must
/// implement this trait. There is no default—implementors must explicitly
/// return `None` if they do not support io-uring, so that wrapper types
/// do not silently drop the capability.
#[cfg(target_os = "linux")]
pub trait IoUringDriver {
    /// The type used to submit io-uring operations.
    ///
    /// Use [`NoIoUring`] if the driver does not support io-uring.
    type Submitter: IoUringSubmit;

    /// Returns an io-uring submitter.
    fn io_uring_submitter(&self) -> Option<&Self::Submitter>;
}

/// A type that represents the absence of io-uring support.
pub enum NoIoUring {}

impl IoUringSubmit for NoIoUring {
    fn probe(&self, _opcode: u8) -> bool {
        match *self {}
    }

    unsafe fn submit(&self, _sqe: Entry) -> impl Future<Output = io::Result<i32>> + Send + '_ {
        (match *self {}) as std::future::Pending<_>
    }
}

/// Trait for submitting io-uring operations.
pub trait IoUringSubmit: Send + Sync {
    /// Returns whether the given opcode is supported by the ring.
    fn probe(&self, opcode: u8) -> bool;

    /// Submits an io-uring SQE for asynchronous execution.
    ///
    /// Returns a future that completes with the IO result. The future **aborts
    /// the process** if dropped while the IO is in flight, since there is no
    /// way to synchronously cancel an in-flight io-uring operation.
    ///
    /// # Safety
    ///
    /// All memory referenced by the SQE must remain valid for the lifetime of
    /// the returned future.
    ///
    /// This can be hard to do safely; in particular, if this future can be
    /// leaked (via [`std::mem::forget`] or otherwise) then the caller must
    /// ensure that any referenced memory also leaks. The easiest way to do that
    /// is to ensure that the future is `await`ed in an async function or block
    /// that owns the underlying memory. So, this is safe:
    ///
    /// ```rust,ignore
    /// async fn write(uring: &impl IoUringSubmit, file: &File, buf: Vec<u8>) -> io::Result<usize> {
    ///     let sqe = opcode::Write::new(
    ///         types::Fd(file.as_raw_fd()), buf.as_ptr(), buf.len() as u32,
    ///     ).build();
    ///     // SAFETY: `buf` is owned by this async function's state machine.
    ///     // If the outer future is leaked, `buf` leaks with it, so the
    ///     // memory remains valid for the io-uring operation.
    ///     unsafe { uring.submit(sqe).await? };
    ///     Ok(buf.len())
    /// }
    /// ```
    ///
    /// But this is not:
    ///
    /// ```rust,ignore
    /// async fn write(uring: &impl IoUringSubmit, file: &File, buf: &[u8]) -> io::Result<usize> {
    ///     let sqe = opcode::Write::new(
    ///         types::Fd(file.as_raw_fd()), buf.as_ptr(), buf.len() as u32,
    ///     ).build();
    ///     // NOT SAFE: `buf` is a borrow. If the outer future is leaked,
    ///     // the referent can be freed while the io-uring operation is
    ///     // still in flight.
    ///     unsafe { uring.submit(sqe).await? };
    ///     Ok(buf.len())
    /// }
    /// ```
    unsafe fn submit(&self, sqe: Entry) -> impl Future<Output = io::Result<i32>> + Send + '_;
}
