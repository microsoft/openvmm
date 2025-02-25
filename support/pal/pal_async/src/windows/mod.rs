// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows-specific async infrastructure.

// UNSAFETY: Calls to various Win32 functions to interact with os-level primitives
// and handling their return values.
#![expect(unsafe_code)]
#![expect(clippy::undocumented_unsafe_blocks)]

pub mod iocp;
pub mod local;
pub mod overlapped;
pub mod pipe;
mod socket;
pub mod tp;

pub use iocp::IocpDriver as DefaultDriver;
pub use iocp::IocpPool as DefaultPool;

pub(crate) fn monotonic_nanos_now() -> u64 {
    unsafe {
        let mut time = 0;
        windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTimePrecise(
            &mut time,
        );
        time.checked_mul(100).expect("time does not fit in u64")
    }
}
