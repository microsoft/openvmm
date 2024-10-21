// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix-specific async infrastructure.

// UNSAFETY: Calls to various libc functions to interact with os-level primitives
// and handling their return values.
#![allow(unsafe_code)]

use cfg_if::cfg_if;

pub mod local;
pub mod pipe;
pub mod wait;

cfg_if! {
    if #[cfg(target_os = "linux")] {
        pub mod epoll;

        pub use epoll::EpollDriver as DefaultDriver;
        pub use epoll::EpollPool as DefaultPool;
    } else if #[cfg(target_os = "macos")] {
        pub mod kqueue;

        pub use kqueue::KqueueDriver as DefaultDriver;
        pub use kqueue::KqueuePool as DefaultPool;
    }
}

pub(crate) fn monotonic_nanos_now() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    // SAFETY: calling C APIs as documented, with no special requirements, and validating its return value.
    unsafe {
        assert_eq!(libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts), 0);
    }

    let sec: u64 = ts.tv_sec as u64;
    sec.checked_mul(1000 * 1000 * 1000)
        .and_then(|n| n.checked_add(ts.tv_nsec as u64))
        .expect("time does not fit in u64")
}
