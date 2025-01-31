// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hyper-V hypercall parsing.
//!
//! This crate helps you implement handling for Hyper-V hypercalls issued by
//! guest VMs. These are the hypercalls defined in the [Hypervisor Top Level
//! Functional Specification][].
//!
//! Besides providing parsing of the core hypercall ABI, it also provides Rust
//! traits for each supported hypercall.
//!
//! To use this crate, you provide access to the processor's registers, and you
//! implement the trait corresponding to each hypercall you want to support.
//! Then you use the [`dispatcher`] macro to instantiate a dispatcher, and you
//! call [`Dispatcher::dispatch`] dispatch the hypercall.
//!
//! [Hypervisor Top Level Functional Specification]:
//!     <https://learn.microsoft.com/en-us/virtualization/hyper-v-on-windows/tlfs/tlfs>

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod aarch64;
mod imp;
mod support;
#[cfg(test)]
mod tests;
mod x86;

pub use self::aarch64::Arm64RegisterIo;
pub use self::aarch64::Arm64RegisterState;
pub use self::imp::*;
pub use self::support::AsHandler;
pub use self::support::Dispatcher;
pub use self::support::HvRepResult;
pub use self::support::HypercallDefinition;
pub use self::support::HypercallHandler;
pub use self::support::HypercallIo;
pub use self::x86::X64HypercallRegister;
pub use self::x86::X64RegisterIo;
pub use self::x86::X64RegisterState;
