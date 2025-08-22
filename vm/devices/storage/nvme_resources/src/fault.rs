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
    /// A map of NVME opcodes to the fault behavior for each. (This would ideally be a `HashMap`, but `mesh` doesn't support that type. Given that this is not performance sensitive, the lookup is okay)
    admin_submission_queue_intercept: Vec<(u8, QueueFaultBehavior<Command>)>,
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

    /// Add a simple submission queue fault based on opcodes. Multiple calls to add faults for the same opcode will panic.
    pub fn with_submission_queue_fault(
        mut self,
        opcode: u8,
        behaviour: QueueFaultBehavior<Command>,
    ) -> Self {
        if self
            .admin_submission_queue_intercept
            .iter()
            .find_map(|(op, b)| if *op == opcode { Some(b) } else { None })
            .is_some()
        {
            panic!("Duplicate submission queue fault for opcode {}", opcode);
        }

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
            *behavior
        } else {
            QueueFaultBehavior::Default
        }
    }
}
