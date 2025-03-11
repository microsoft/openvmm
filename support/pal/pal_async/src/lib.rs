// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An asynchronous IO platform.

#[cfg(test)]
extern crate self as pal_async;

#[cfg(unix)]
pub mod fd;
pub mod interest;
pub mod local;
pub mod pipe;
pub mod socket;
pub mod timer;
pub mod wait;

mod any;
pub mod driver;
#[cfg(any(test, feature = "tests"))]
pub mod executor_tests;
pub mod io_pool;
pub mod multi_waker;
mod sparsevec;
#[cfg_attr(unix, path = "unix/mod.rs")]
#[cfg_attr(windows, path = "windows/mod.rs")]
mod sys;
pub mod task;
mod waker;

/// Windows-specific async functionality.
#[cfg(windows)]
pub mod windows {
    pub use super::sys::iocp::IocpDriver;
    pub use super::sys::iocp::IocpPool;
    pub use super::sys::overlapped;
    pub use super::sys::pipe;
    pub use super::sys::tp::TpPool;
}

/// Unix-specific async functionality.
#[cfg(unix)]
pub mod unix {
    pub use super::sys::pipe;
    pub use super::sys::wait::FdWait;

    #[cfg(target_os = "linux")]
    pub use super::sys::epoll::EpollDriver;
    #[cfg(target_os = "linux")]
    pub use super::sys::epoll::EpollPool;

    #[cfg(target_os = "macos")]
    pub use super::sys::kqueue::KqueueDriver;
    #[cfg(target_os = "macos")]
    pub use super::sys::kqueue::KqueuePool;
}

/// The default single-threaded IO driver for the platform.
pub type DefaultDriver = sys::DefaultDriver;
/// The default single-threaded task pool for the platform.
pub type DefaultPool = sys::DefaultPool;

pub use pal_async_test::async_test;
