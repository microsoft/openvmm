// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for using the tracing crate more effectively.
//!
//! In particular, this includes extension traits to make it easier to pass
//! errors to tracing events. The events can take a `dyn Error`, but getting a
//! `dyn Error` from an arbitrary error type requires casting.
//!
//! Hopefully this crate will be short lived as `tracing` ergonomics continue
//! to improve.

#![forbid(unsafe_code)]

pub mod formatter;

/// Extension trait to make it easy to trace anyhow errors.
pub trait AnyhowValueExt {
    /// Returns the error as a type that can be traced.
    fn as_error(&self) -> &(dyn 'static + std::error::Error);
}

impl AnyhowValueExt for anyhow::Error {
    fn as_error(&self) -> &(dyn 'static + std::error::Error) {
        &**self
    }
}

/// Extension trait to make it easy to trace errors.
pub trait ErrorValueExt {
    /// Returns the error as a type that can be traced.
    fn as_error(&self) -> &(dyn 'static + std::error::Error);
}

impl<T> ErrorValueExt for T
where
    T: 'static + std::error::Error,
{
    fn as_error(&self) -> &(dyn 'static + std::error::Error) {
        self
    }
}
