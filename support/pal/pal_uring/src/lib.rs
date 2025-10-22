// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_os = "linux"), expect(missing_docs))]
#![cfg(target_os = "linux")]

//! [`pal_async`] support for the Linux kernel's io_uring.
//!
//! Historically, this crate included a per-CPU thread pool implementation. Some
//! vestigates remain in naming and elsewhere. TODO: clean this up.

// UNSAFETY: This module uses unsafe code to interact with the io_uring kernel
// interface.
#![expect(unsafe_code)]

mod ioring;
mod threadpool;
mod uring;

pub use threadpool::*;
pub use uring::*;
