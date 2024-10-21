// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]

//! [`pal_async`] support for the Linux kernel's io_uring.
//!
//! Historically, this crate included a per-CPU thread pool implementation. Some
//! vestigates remain in naming and elsewhere. TODO: clean this up.

#![warn(missing_docs)]
// UNSAFETY: This module uses unsafe code to interact with the io_uring kernel
// interface.
#![allow(unsafe_code)]

mod ioring;
mod threadpool;
mod uring;

pub use threadpool::*;
pub use uring::*;
