// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows-specific async infrastructure.

// UNSAFETY: Calls to various Win32 functions to interact with os-level primitives
// and handling their return values.
#![expect(unsafe_code)]

pub mod iocp;
pub mod local;
pub mod overlapped;
pub mod pipe;
mod socket;
pub mod tp;

pub use iocp::IocpDriver as DefaultDriver;
pub use iocp::IocpPool as DefaultPool;

pub(crate) fn monotonic_nanos_now() -> u64 {
    let mut time = 0;
    // SAFETY: passing a valid buffer.
    unsafe {
        windows_sys::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTimePrecise(
            &mut time,
        );
    }
    time.checked_mul(100).expect("time does not fit in u64")
}
