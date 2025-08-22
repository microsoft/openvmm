// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fault definitions for NVMe fault controller.

use mesh::Cell;
use nvme_spec::Command;

/// Supported fault behaviour for NVMe queues
#[derive(Debug, Clone, Copy)]
pub enum QueueFaultBehavior<T> {
    /// Update the queue entry with the returned data
    Update(T),
    /// Drop the queue entry
    Drop,
    /// No Fault, proceed as normal
    Default,
}

/// A buildable fault configuration
pub struct AdminQueueFaultConfig {
    /// A mapping from the admin opcode to its fault behavior. This should ideally be using a HashMap but Encode/Decode is not yet available for that.
    /// It should also be a mapping from OpCode -> QueueFaultBehavior<Command> but Encode/Decode is not yet available for those types.
    pub admin_submission_queue_intercept: Vec<(u8, QueueFaultBehavior<Command>)>,
}

/// A simple fault configuration with admin submission queue support
pub struct FaultConfiguration {
    /// Fault active state
    pub fault_active: Cell<bool>,
    /// Fault to apply to the admin queues
    pub admin_fault: AdminQueueFaultConfig,
}

impl AdminQueueFaultConfig {
    /// Create an empty fault configuration
    pub fn new() -> Self {
        Self {
            admin_submission_queue_intercept: vec![],
        }
    }

    /// Add a simple submission queue fault based on opcodes
    pub fn with_submission_queue_fault(
        mut self,
        opcode: u8,
        behaviour: QueueFaultBehavior<Command>,
    ) -> Self {
        self.admin_submission_queue_intercept
            .push((opcode, behaviour));
        self
    }

    /// Given the opcode, return the fault behaviour for the Admin Command
    pub fn fault_submission_queue(&self, command: Command) -> QueueFaultBehavior<Command> {
        let opcode: u8 = nvme_spec::AdminOpcode(command.cdw0.opcode()).0;
        if let Some(behavior) = self
            .admin_submission_queue_intercept
            .iter()
            .find_map(|(op, b)| if *op == opcode { Some(b) } else { None })
        {
            behavior.clone()
        } else {
            QueueFaultBehavior::Default
        }
    }
}
