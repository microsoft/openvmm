// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Windows FFI
#![expect(unsafe_code)]

pub use server::run_server;

mod handlers;
pub mod igvm_agent;
mod server;
