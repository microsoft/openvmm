// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod admin;
pub mod pci;

use crate::spec::Command;
use std::future::Future;
use vmcore::vm_task::VmTaskDriver;

/// A type alias for a fault injection function that is used to alter NVMe commands.
/// ### Example `FaultFn`
///
/// ```rust
/// // Add a delay and change the command data
/// let fault_injector = Box::new(|driver, command| {
///     Box::pin(async move {
///         PolledTimer::new(&driver).sleep(Duration::new(5, 0)).await;
///         // Modify command to introduce errors
///         command.cdw10 = 0xDEADBEEF;
///         Some(command)
///     })
/// });
pub type FaultFn = Box<
    dyn Fn(VmTaskDriver, Command) -> std::pin::Pin<Box<dyn Future<Output = Option<Command>> + Send>>
        + Send
        + Sync,
>;
