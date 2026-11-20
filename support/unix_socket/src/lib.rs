// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides [`UnixStream`] and [`UnixListener`] implementations for
//! Windows, and re-exposes the `std` types for UNIX.
//!
//! This can go away once the `std` types are available on Windows.
//!
//! <https://github.com/rust-lang/rust/issues/56533>
#![cfg_attr(
    unix,
    doc = "",
    doc = "On UNIX, it also provides low-level `SCM_RIGHTS` fd-passing helpers",
    doc = "(see [`send_with_fds`] and [`ScmReceiver`])."
)]

cfg_select! {
    windows => {
        mod windows;

        pub use windows::*;
    }
    unix => {
        mod unix;

        pub use std::os::unix::net::UnixListener;
        pub use std::os::unix::net::UnixStream;
        pub use unix::ScmDrainIter;
        pub use unix::ScmReceiver;
        pub use unix::send_with_fds;
    }
}
