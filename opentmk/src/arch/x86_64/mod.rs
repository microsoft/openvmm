// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod hypercall;
#[cfg(nightly)]
pub mod interrupt;
#[cfg(nightly)]
mod interrupt_handler_register;
mod io;
pub mod rtc;
pub mod serial;
pub mod tpm;
