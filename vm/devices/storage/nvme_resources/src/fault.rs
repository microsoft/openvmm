// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fault definitions for NVMe fault controller.

use mesh::Cell;
use nvme_spec::Command;
use std::time::Duration;

/// Supported fault behaviour for NVMe queues
#[derive(Debug, Clone, Copy)]
pub enum QueueFaultBehavior<T> {
    /// Update the queue entry with the returned data
    Update(T),
    /// Drop the queue entry
    Drop,
    /// No Fault, proceed as normal
    Default,
    /// Delay
    Delay(Duration),
}

/// A buildable fault configuration
pub struct AdminQueueFaultConfig {
    /// A map of NVME opcodes to the fault behavior for each. (This would ideally be a `HashMap`, but `mesh` doesn't support that type. Given that this is not performance sensitive, the lookup is okay)
    pub admin_submission_queue_faults: Vec<(u8, QueueFaultBehavior<Command>)>,
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
            admin_submission_queue_faults: vec![],
        }
    }

    /// Add an opcode -> FaultBehavior mapping. Cannot configure an opcode more than once
    pub fn with_submission_queue_fault(
        mut self,
        opcode: u8,
        behaviour: QueueFaultBehavior<Command>,
    ) -> Self {
        if self
            .admin_submission_queue_faults
            .iter()
            .map(|(op, _)| op)
            .any(|&op| op == opcode)
        {
            panic!("Duplicate submission queue fault for opcode {}", opcode);
        }

        self.admin_submission_queue_faults.push((opcode, behaviour));
        self
    }
}
