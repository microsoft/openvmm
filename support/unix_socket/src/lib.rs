// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides [`UnixStream`] and [`UnixListener`] implementations for
//! Windows, and re-exposes the `std` types for UNIX.
//!
//! This can go away once the `std` types are available on Windows.
//!
//! <https://github.com/rust-lang/rust/issues/56533>

#![cfg_attr(not(windows), forbid(unsafe_code))]
#![warn(missing_docs)]

mod windows;

#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
pub use std::os::unix::net::UnixListener;
#[cfg(unix)]
pub use std::os::unix::net::UnixStream;
