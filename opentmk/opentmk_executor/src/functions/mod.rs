// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod hvcall_meta;
pub mod hyperv;
#[cfg(target_arch = "x86_64")]
pub mod io_port;
pub mod netvsp;
mod registry;
pub mod variable;
pub mod vmbus;

pub use registry::FunctionRegistry;
pub use variable::FuzzFunction;
pub use variable::FuzzFunctionVariable;
pub use variable::VerifyFuzzVariables;
