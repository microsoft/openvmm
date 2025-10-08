// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fault definitions for NVMe fault controller.

use mesh::Cell;
use mesh::MeshPayload;
use mesh::rpc::Rpc;
use nvme_spec::Command;
use nvme_spec::Completion;
use std::time::Duration;

/// Supported fault behaviour for NVMe queues
#[derive(Debug, Clone, MeshPayload)]
pub enum QueueFaultBehavior<T> {
    /// Update the queue entry with the returned data
    Update(T),
    /// Drop the queue entry
    Drop,
    /// Delay
    Delay(Duration),
    /// Panic
    Panic(String),
    /// Update a completion payload
    CustomPayload(Vec<u8>),
}

#[derive(Clone, MeshPayload)]
/// Supported fault behaviour for PCI faults
pub enum PciFaultBehavior {
    /// Introduce a delay to the PCI operation
    Delay(Duration),
    /// Do nothing
    Default,
}

#[derive(MeshPayload)]
/// A notification to the test confirming namespace change processing.
pub enum NamespaceChange {
    /// Input: Namespace ID to notify
    ChangeNotification(Rpc<u32, ()>),
}

#[derive(MeshPayload, Clone)]
/// A buildable fault configuration for the controller management interface (cc.en(), csts.rdy(), ... )
pub struct PciFaultConfig {
    /// Fault to apply to cc.en() bit during enablement
    pub controller_management_fault_enable: PciFaultBehavior,
}

#[derive(MeshPayload)]
/// A fault config to allow sending namespace change notifications to the controller.
pub struct NamespaceFaultConfig {
    /// Receiver for changed namespace notifications
    pub recv_changed_namespace: mesh::Receiver<NamespaceChange>,
}

#[derive(MeshPayload, Clone)]
/// A buildable fault configuration
pub struct AdminQueueFaultConfig {
    /// A map of NVME opcodes to the submission fault behavior for each. (This
    /// would ideally be a `HashMap`, but `mesh` doesn't support that type.
    /// Given that this is not performance sensitive, the lookup is okay)
    pub admin_submission_queue_faults: Vec<(CommandMatch, QueueFaultBehavior<Command>)>,
    /// A map of NVME opcodes to the completion fault behavior for each.
    pub admin_completion_queue_faults: Vec<(CommandMatch, QueueFaultBehavior<Completion>)>,
}

#[derive(Clone, MeshPayload, PartialEq)]
/// A definition of a command matching pattern.
pub struct CommandMatch {
    /// Command to match against
    pub command: Command,
    /// Bitmask that defines the bits to match against
    pub mask: [u8; 64],
}

#[derive(MeshPayload)]
/// A simple fault configuration with admin submission queue support
pub struct FaultConfiguration {
    /// Fault active state
    pub fault_active: Cell<bool>,
    /// Fault to apply to the admin queues
    pub admin_fault: AdminQueueFaultConfig,
    /// Fault to apply to management layer of the controller
    pub pci_fault: PciFaultConfig,
    /// Fault for test triggered namespace change notifications
    pub namespace_fault: NamespaceFaultConfig,
}

impl FaultConfiguration {
    /// Create a new empty fault configuration
    pub fn new(fault_active: Cell<bool>) -> Self {
        // Ideally the faults should begin life as Option::None.
        // For now, use a dummy mesh channel for namespace fault to avoid
        // test setup complexity & special cases in the AdminHandler run loop.
        Self {
            fault_active,
            admin_fault: AdminQueueFaultConfig::new(),
            pci_fault: PciFaultConfig::new(),
            namespace_fault: NamespaceFaultConfig::new(mesh::channel().1),
        }
    }

    /// Add a PCI fault configuration to the fault configuration
    pub fn with_pci_fault(mut self, pci_fault: PciFaultConfig) -> Self {
        self.pci_fault = pci_fault;
        self
    }

    /// Add an admin queue fault configuration to the fault configuration
    pub fn with_admin_queue_fault(mut self, admin_fault: AdminQueueFaultConfig) -> Self {
        self.admin_fault = admin_fault;
        self
    }

    /// Add a namespace fault configuration to the fault configuration
    pub fn with_namespace_fault(mut self, namespace_fault: NamespaceFaultConfig) -> Self {
        self.namespace_fault = namespace_fault;
        self
    }
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
            admin_completion_queue_faults: vec![],
        }
    }

    /// Add a [`CommandMatch`] -> [`QueueFaultBehavior`] mapping for the submission queue.
    ///
    /// # Panics
    /// Panics if an identical [`CommandMatch`] has already been configured.
    pub fn with_submission_queue_fault(
        mut self,
        pattern: CommandMatch,
        behaviour: QueueFaultBehavior<Command>,
    ) -> Self {
        if self
            .admin_submission_queue_faults
            .iter()
            .any(|(c, _)| pattern == *c)
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

    /// Add a [`CommandMatch`] -> [`QueueFaultBehavior`] mapping for the completion queue.
    ///
    /// # Panics
    /// Panics if an identical [`CommandMatch`] has already been configured.
    pub fn with_completion_queue_fault(
        mut self,
        pattern: CommandMatch,
        behaviour: QueueFaultBehavior<Completion>,
    ) -> Self {
        if self
            .admin_completion_queue_faults
            .iter()
            .any(|(c, _)| pattern == *c)
        {
            panic!(
                "Duplicate completion queue fault for Compare {:?} and Mask {:?}",
                pattern.command, pattern.mask
            );
        }

        self.admin_completion_queue_faults
            .push((pattern, behaviour));
        self
    }
}

impl NamespaceFaultConfig {
    /// Creates a new NamespaceFaultConfig with a fresh channel.
    pub fn new(recv_changed_namespace: mesh::Receiver<NamespaceChange>) -> Self {
        Self {
            recv_changed_namespace,
        }
    }
}
