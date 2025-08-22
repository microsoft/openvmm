// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fault definitions for NVMe fault controller.

use mesh::Cell;
use mesh::MeshPayload;
use nvme_spec as spec;

/// Supported fault behaviour for NVMe queues
#[derive(Debug, Clone, Copy, MeshPayload)]
pub enum QueueFaultBehavior<T> {
    /// Update the queue entry with the returned data
    Update(T),
    /// Drop the queue entry
    Drop,
    /// No Fault, proceed as normal
    Default,
    /// Change the completion ID of a message
    ChangeCompletionId(u16),
}

/// Provides fault logic for a pair of submission and completion queue.
#[async_trait::async_trait]
pub trait QueueFault {
    /// Provided a command in the submission queue, return the appropriate fault behavior.
    async fn fault_submission_queue(
        &self,
        command: spec::Command,
    ) -> QueueFaultBehavior<spec::Command>;

    /// Provided a command in the completion queue, return the appropriate fault behavior.
    async fn fault_completion_queue(
        &self,
        completion: spec::Completion,
    ) -> QueueFaultBehavior<spec::Completion>;
}

/// Configuration for NVMe controller faults.
#[derive(MeshPayload)]
pub struct FaultConfiguration {
    /// Signal to start the fault
    pub signal: Cell<bool>,
    /// Fault to apply to the admin queues
    pub admin_fault: AdminQueueFaultConfig,
}

/// Something
#[derive(MeshPayload)]
pub struct AdminQueueFaultConfig {
    /// A mapping from the admin opcode to its fault behavior
    /// TODO: This should technically be a map but Encode/Decode has not yet been implemented for the HashMap type
    /// TODO: This should technically also be using an OpCode -> Command mapping. Saving that work for right now
    pub admin_submission_queue_intercept: Vec<(u8, QueueFaultBehavior<u32>)>,
}

impl AdminQueueFaultConfig {
    /// Some documentation
    pub fn new() -> Self {
        Self {
            admin_submission_queue_intercept: Vec::new(),
        }
    }

    /// Some documentation
    pub fn with_submission_queue_fault(
        mut self,
        opcode: u8,
        behaviour: QueueFaultBehavior<u32>,
    ) -> Self {
        self.admin_submission_queue_intercept
            .push((opcode, behaviour));
        self
    }

    /// Given a certain op_code this returns the Fault behaviour for that OpCode
    pub fn fault_submission_queue(&self, command: spec::Command) -> QueueFaultBehavior<u32> {
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

    // Not implemented for now at least
    pub fn fault_completion_queue(
        self,
        _: spec::Completion,
    ) -> QueueFaultBehavior<spec::Completion> {
        // For now, we do not have any specific completion queue faults.
        QueueFaultBehavior::Default
    }
}
