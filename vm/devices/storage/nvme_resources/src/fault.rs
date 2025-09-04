// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fault definitions for NVMe fault controller.

use mesh::Cell;
use mesh::MeshPayload;
use nvme_spec::Command;
use std::time::Duration;

/// Supported fault behaviour for NVMe queues
#[derive(Debug, Clone, MeshPayload)]
pub enum QueueFaultBehavior<T> {
    /// Update the queue entry with the returned data
    Update(T),
    /// Drop the queue entry
    Drop,
    /// No Fault
    Default,
    /// Delay
    Delay(Duration),
    /// Panic
    Panic(String),
}

#[derive(Clone, MeshPayload)]
/// Supported fault behaviour for PCI faults
pub enum PciFaultBehavior {
    /// Introduce a delay to the PCI operation
    Delay(Duration),
    /// Do nothing
    Default,
}

#[derive(MeshPayload, Clone)]
/// A buildable fault configuration for the controller management interface (cc.en(), csts.rdy(), ... )
pub struct PciFaultConfig {
    /// Fault to apply to cc.en() bit during enablement
    pub controller_management_fault_enable: PciFaultBehavior,
}

#[derive(MeshPayload, Clone)]
/// A buildable fault configuration
pub struct AdminQueueFaultConfig {
    /// A map of NVME opcodes to the fault behavior for each. (This would ideally be a `HashMap`, but `mesh` doesn't support that type. Given that this is not performance sensitive, the lookup is okay)
    pub admin_submission_queue_faults: Vec<(CommandMatch, QueueFaultBehavior<Command>)>,
}

#[derive(Clone, MeshPayload, PartialEq)]
/// A definition of a command matching pattern.
pub struct CommandMatch {
    /// Command to match against
    pub command: Command,
    /// Bitmask that defines the bits to match against
    pub mask: [u8; 64],
}

#[derive(MeshPayload, Clone)]
/// A simple fault configuration with admin submission queue support
pub struct FaultConfiguration {
    /// Fault active state
    pub fault_active: Cell<bool>,
    /// Fault to apply to the admin queues
    pub admin_fault: AdminQueueFaultConfig,
    /// Fault to apply to management layer of the controller
    pub pci_fault: PciFaultConfig,
}

impl PciFaultConfig {
    /// Create a new no-op fault configuration
    pub fn new() -> Self {
        Self {
            controller_management_fault_enable: PciFaultBehavior::Default,
        }
    }

    /// Add a cc.en() fault
    pub fn with_cc_enable_fault(mut self, behaviour: PciFaultBehavior) -> Self {
        self.controller_management_fault_enable = behaviour;
        self
    }
}

impl AdminQueueFaultConfig {
    /// Create an empty fault configuration
    pub fn new() -> Self {
        Self {
            admin_submission_queue_faults: vec![],
        }
    }

    /// Add a CommandMatch -> FaultBehavior mapping. Cannot configure a CommandMatch more than once
    pub fn with_submission_queue_fault(
        mut self,
        pattern: CommandMatch,
        behaviour: QueueFaultBehavior<Command>,
    ) -> Self {
        if self
            .admin_submission_queue_faults
            .iter()
            .any(|(c, _)| (pattern == *c))
        {
            panic!(
                "Duplicate submission queue fault for Compare {:?} and Mask {:?}",
                pattern.command, pattern.mask
            );
        }

        self.admin_submission_queue_faults
            .push((pattern, behaviour));
        self
    }
}
