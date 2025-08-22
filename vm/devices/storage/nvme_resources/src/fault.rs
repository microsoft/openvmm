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
    /// Delay queue processing. Time in ms
    Delay(u64),
    /// No Fault
    Default,
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
    /// Fault active state
    pub fault_active: Cell<bool>,
    /// Fault to apply to the admin queues
    pub admin_fault: AdminQueueFaultConfig,
}

/// A mesh sendable fault configuration for the nvme admin queue
#[derive(MeshPayload)]
pub struct AdminQueueFaultConfig {
    /// A mapping from the admin opcode to its fault behavior. This should ideally be using a HashMap but Encode/Decode is not yet available for that.
    /// It should also be a mapping from OpCode -> QueueFaultBehavior<Command> but Encode/Decode is not yet available for those types.
    pub admin_submission_queue_intercept: Vec<(u8, QueueFaultBehavior<[u8; 64]>)>,
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
        behaviour: QueueFaultBehavior<[u8; 64]>,
    ) -> Self {
        self.admin_submission_queue_intercept
            .push((opcode, behaviour));
        self
    }

    /// Given the opcode, return the fault behaviour for the Admin Command
    pub fn fault_submission_queue(&self, command: spec::Command) -> QueueFaultBehavior<[u8; 64]> {
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
