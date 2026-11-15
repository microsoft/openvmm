// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Imports and re-exports architecture-specific implementations.

mod aarch64;
mod x86_64;

cfg_select! {
    target_arch = "x86_64" => {
        use x86_64 as arch;
        pub use x86_64::msr;
        pub use x86_64::serial::COM3;
        pub use x86_64::serial::com3;
    }
    target_arch = "aarch64" => {
        use aarch64 as arch;
        pub use aarch64::serial::Serial;
    }
    _ => {
        compile_error!("target_arch is not supported");
    }
}

pub(crate) use arch::enlightened_panic::write_crash_reg;
pub use arch::hypercall;
pub use arch::intrinsics::dead_loop;
pub use arch::intrinsics::fault;
pub(crate) use arch::reftime::reference_time;
