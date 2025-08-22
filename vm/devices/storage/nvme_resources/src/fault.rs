// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fault definitions for NVMe fault controller.

use nvme_spec as spec;

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
pub struct FaultConfiguration {
    /// Fault to apply to the admin queues
    pub admin_fault: Option<Box<dyn QueueFault + Send + Sync>>,
}
