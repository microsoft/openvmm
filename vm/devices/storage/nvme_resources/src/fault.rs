// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides an interface to programmatically and deterministically inject faults in the NVMe fault controller.

use mesh::Cell;
use mesh::MeshPayload;
use mesh::OneshotSender;
use mesh::rpc::Rpc;
use nvme_spec::Command;
use nvme_spec::Completion;
use std::sync::Arc;
use std::time::Duration;

/// Supported fault behaviour for NVMe admin queues
#[derive(Debug, MeshPayload)]
pub enum AdminQueueFaultBehavior<T> {
    /// Update the queue entry with the returned data
    Update(T),
    /// Drop the queue entry
    Drop,
    /// Delay. Note: This delay is not asynchronously applied. i.e. Subsequent
    /// commands will be processed until the delay is over.
    Delay(Duration),
    /// Panic
    Panic(String),
    /// Writes the given payload to the PRP range. The test should ensure
    /// that the payload is of valid size. If the size is too large, the fault
    /// controller will panic. This behavior is not yet supported by the submission
    /// queue fault.
    CustomPayload(Vec<u8>),
    /// Verify that a command was seen.
    Verify(Option<OneshotSender<()>>),
}

/// Supported fault behaviour for NVMe IO queues
#[derive(Debug, MeshPayload, Clone)]
pub enum IoQueueFaultBehavior {
    /// Writes the given payload to the PRP range. The test should ensure
    /// that the payload is of valid size. If the size is too large, the fault
    /// controller will panic. This behavior is not yet supported by the submission
    /// queue fault.
    CustomPayload(Vec<u8>),
    /// Panic
    Panic(String),
}

/// Supported fault behaviour for PCI faults
#[derive(MeshPayload)]
pub enum PciFaultBehavior {
    /// Introduce a delay to the PCI operation. This WILL block the processing
    /// thread for the delay duration.
    Delay(Duration),
    /// Do nothing
    Default,
    /// Verify that the fault was triggered.
    Verify(Option<OneshotSender<()>>),
}

/// A notification to the test confirming namespace change processing.
#[derive(MeshPayload)]
pub enum NamespaceChange {
    /// Input: Namespace ID to notify, Output: Empty confirmation.
    ChangeNotification(Rpc<u32, ()>),
}

/// A fault configuration to apply [`PciFaultBehavior`] to the controller management layer.
///
/// Currently the only supported fault is to delay enabling the controller via
/// cc.en().
///
/// # Example
/// Delay enabling the controller by 500ms.
///
/// ```no_run
/// use mesh::CellUpdater;
/// use nvme_resources::fault::FaultConfiguration;
/// use nvme_resources::fault::PciFaultBehavior;
/// use nvme_resources::fault::PciFaultConfig;
/// use std::time::Duration;
///
/// pub fn pci_enable_delay_fault() -> FaultConfiguration{
///     let mut fault_start_updater = CellUpdater::new(false);
///     FaultConfiguration::new(fault_start_updater.cell())
///         .with_pci_fault(
///             PciFaultConfig::new().with_cc_enable_fault(
///                 PciFaultBehavior::Delay(Duration::from_millis(500)),
///             )
///         )
/// }
/// ```
#[derive(MeshPayload)]
pub struct PciFaultConfig {
    /// Fault to apply to cc.en() bit during enablement
    pub controller_management_fault_enable: PciFaultBehavior,
}

/// A fault config to trigger spurious namespace change notifications from the controller.
///
/// The fault controller listens on the provided channel for notifications containing
/// a `u32` value representing the NSID (Namespace Identifier) that has changed.
/// This does not actually modify the namespace; instead, it triggers the controller
/// to process a namespace change notification. The fault is modeled as an
/// RPC, which the controller completes once it has processed the change and sent
/// the corresponding Asynchronous Event Notification (AEN).
/// As per NVMe spec: If multiple namespace changes are notified, only the first triggers an AEN.
/// Subsequent changes do not trigger additional AENs until the driver issues a
/// GET_LOG_PAGE command. For implementation simplicity, namespace fault is not
/// gated by the `fault_active` flag. Since only test code can send
/// notifications on the fault channel, it is safe to bypass this check.
///
/// # Example
/// Send a namespace change notification for NSID 1 and wait for it to be processed.
/// ```no_run
/// use mesh::CellUpdater;
/// use nvme_resources::fault::NamespaceChange;
/// use nvme_resources::fault::FaultConfiguration;
/// use nvme_resources::fault::NamespaceFaultConfig;
/// use nvme_resources::NvmeFaultControllerHandle;
/// use guid::Guid;
/// use mesh::rpc::RpcSend;
///
/// pub async fn send_namespace_change_fault() {
///     let mut fault_start_updater = CellUpdater::new(false);
///     let (ns_change_send, ns_change_recv) = mesh::channel::<NamespaceChange>();
///     let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
///         .with_namespace_fault(
///             NamespaceFaultConfig::new(ns_change_recv),
///         );
///     // Complete setup
///     let fault_controller_handle = NvmeFaultControllerHandle {
///         subsystem_id: Guid::new_random(),
///         msix_count: 10,
///         max_io_queues: 10,
///         namespaces: vec![
///             // Define `NamespaceDefinitions` here
///         ],
///         fault_config: fault_configuration,
///     };
///
///     // Send the namespace change notification and await processing.
///     ns_change_send.call(NamespaceChange::ChangeNotification, 1).await.unwrap();
/// }
/// ```
#[derive(MeshPayload)]
pub struct NamespaceFaultConfig {
    /// Receiver for changed namespace notifications
    pub recv_changed_namespace: mesh::Receiver<NamespaceChange>,
}

/// A fault configuration to inject faults into the admin submission and completion queues.
///
/// This struct maintains a mapping from [`CommandMatch`] to [`AdminQueueFaultBehavior`] for
/// submission and completion queues. When a command match is found, (and `fault_active == true`)
/// the associated fault is applied.
/// Both submission and completion queue faults match on commands
/// because completions do not contain enough identifying information to
/// match against. If there is more than one match for a given command, the
/// match defined first is prioritized. Faults are added via the
/// `with_submission_queue_fault` and `with_completion_queue_fault` methods and
/// can be chained. AdminQueueFaultConfig::new() creates an empty fault.
///
/// # Panics
/// Panics if a duplicate `CommandMatch` is added for either submission or
/// completion queues
///
/// # Example
/// Panic on CREATE_IO_COMPLETION_QUEUE and delay before sending completion for 500ms after
/// GET_LOG_PAGE command is processed.
/// ```no_run
/// use mesh::CellUpdater;
/// use nvme_resources::fault::AdminQueueFaultConfig;
/// use nvme_resources::fault::CommandMatch;
/// use nvme_resources::fault::FaultConfiguration;
/// use nvme_resources::fault::AdminQueueFaultBehavior;
/// use nvme_spec::Command;
/// use std::time::Duration;
/// use zerocopy::FromZeros;
/// use zerocopy::IntoBytes;
///
/// pub fn build_admin_queue_fault() -> FaultConfiguration {
///     let mut fault_start_updater = CellUpdater::new(false);
///
///     // Setup command matches
///     let mut command_io_queue = Command::new_zeroed();
///     let mut command_log_page = Command::new_zeroed();
///     let mut mask = Command::new_zeroed();
///
///     command_io_queue.cdw0 = command_io_queue.cdw0.with_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0);
///     command_log_page.cdw0 = command_log_page.cdw0.with_opcode(nvme_spec::AdminOpcode::GET_LOG_PAGE.0);
///     mask.cdw0 = mask.cdw0.with_opcode(u8::MAX);
///
///     return FaultConfiguration::new(fault_start_updater.cell())
///         .with_admin_queue_fault(
///             AdminQueueFaultConfig::new().with_submission_queue_fault(
///                 CommandMatch {
///                     command: command_io_queue,
///                     mask: mask.as_bytes().try_into().expect("mask should be 64 bytes"),
///                 },
///                 AdminQueueFaultBehavior::Panic("Received a CREATE_IO_COMPLETION_QUEUE command".to_string()),
///             ).with_completion_queue_fault(
///                 CommandMatch {
///                     command: command_log_page,
///                     mask: mask.as_bytes().try_into().expect("mask should be 64 bytes"),
///                 },
///                 AdminQueueFaultBehavior::Delay(Duration::from_millis(500)),
///             )
///         );
/// }
/// ```
#[derive(MeshPayload)]
pub struct AdminQueueFaultConfig {
    /// A map of NVME opcodes to the submission fault behavior for each. (This
    /// would ideally be a `HashMap`, but `mesh` doesn't support that type.
    /// Given that this is not performance sensitive, the lookup is okay)
    pub admin_submission_queue_faults: Vec<(CommandMatch, AdminQueueFaultBehavior<Command>)>,
    /// A map of NVME opcodes to the completion fault behavior for each.
    pub admin_completion_queue_faults: Vec<(CommandMatch, AdminQueueFaultBehavior<Completion>)>,
}

/// A fault configuration to inject faults into the io completions.
///
/// This struct maintains a mapping from [`CommandMatch`] to [`IoQueueFaultBehavior`] for
/// completions. When a command match is found, (and `fault_active == true`)
/// the associated fault is applied.
/// If there is more than one match for a given command, the
/// match defined first is prioritized. Faults are added via the
/// `with_completion_queue_fault` method and calls
/// can be chained. IoQueueFaultConfig::new() creates an empty fault.
///
/// # Panics
/// Panics if a duplicate `CommandMatch` is added
///
/// # Example
/// Panic when RESERVATION_REPORT command is seen.
/// ```no_run
/// use mesh::CellUpdater;
/// use nvme_resources::fault::IoQueueFaultConfig;
/// use nvme_resources::fault::CommandMatch;
/// use nvme_resources::fault::FaultConfiguration;
/// use nvme_resources::fault::IoQueueFaultBehavior;
/// use nvme_spec::Command;
/// use nvme_spec::nvm;
/// use zerocopy::FromZeros;
/// use zerocopy::IntoBytes;
///
/// pub fn build_admin_queue_fault() -> FaultConfiguration {
///     let mut fault_start_updater = CellUpdater::new(false);
///
///     // Setup command matches
///     let mut command_io_queue = Command::new_zeroed();
///     let mut command_log_page = Command::new_zeroed();
///     let mut mask = Command::new_zeroed();
///
///     command_io_queue.cdw0 = command_io_queue.cdw0.with_opcode(nvm::NvmOpcode::RESERVATION_REPORT.0);
///     mask.cdw0 = mask.cdw0.with_opcode(u8::MAX);
///
///     return FaultConfiguration::new(fault_start_updater.cell())
///         .with_io_queue_fault(
///             IoQueueFaultConfig::new(fault_start_updater.cell()).with_completion_queue_fault(
///                 CommandMatch {
///                     command: command_io_queue,
///                     mask: mask.as_bytes().try_into().expect("mask should be 64 bytes"),
///                 },
///                 IoQueueFaultBehavior::Panic("Received a RESERVATION_REPORT command".to_string()),
///             )
///         );
/// }
/// ```
#[derive(MeshPayload, Clone)]
pub struct IoQueueFaultConfig {
    /// A map of NVME opcodes to the completion fault behavior for each.
    pub io_completion_queue_faults: Vec<(CommandMatch, IoQueueFaultBehavior)>,
    /// Fault active state. (Repeated here because FaultConfiguration is not Cloneable).
    pub fault_active: Cell<bool>,
}

/// A versatile definition to command match [`NVMe commands`](nvme_spec::Command)
///
/// Matches NVMe commands using a 512-bit mask: (command_bytes & mask) == (pattern_bytes & mask).
/// A convenient way to build the patterns is to treat both the command and the mask as
/// `nvme_spec::Command` and max out the fields in the mask that should be
/// matched.
///
/// # Example
/// Builds a command match that matches on all CREATE_IO_COMPLETION_QUEUE admin commands.
/// ```no_run
/// use nvme_resources::fault::CommandMatch;
/// use nvme_spec::Command;
/// use zerocopy::FromZeros;
/// use zerocopy::IntoBytes;
///
/// pub fn build_command_match() -> CommandMatch {
///     let mut command = Command::new_zeroed();
///     let mut mask = Command::new_zeroed();
///     command.cdw0 = command.cdw0.with_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0);
///     mask.cdw0 = mask.cdw0.with_opcode(u8::MAX);
///     CommandMatch {
///         command,
///         mask: mask.as_bytes().try_into().expect("mask should be 64 bytes"),
///     }
/// }
/// ```
#[derive(Clone, MeshPayload, PartialEq)]
pub struct CommandMatch {
    /// Command to match against
    pub command: Command,
    /// Bitmask that defines the bits to match against
    pub mask: [u8; 64],
}

/// Fault configuration for the NVMe fault controller.
///
/// This struct defines behaviors that inject faults into the NVMe fault controller logic,
/// such as delaying or dropping commands, triggering namespace change notifications,
/// or customizing completion payloads. Fault injection is controlled by the
/// `fault_active` flag, unless specified otherwise in the fault description.
/// `fault_active` is managed by the test via [`mesh::CellUpdater`]. An
/// exception to the `fault_active` check is the [`NamespaceFaultConfig`] which
/// is processed regardless of `fault_active` state. (See `nvme_test` crate for
/// details on how the faults are applied.)
///
/// # Example
/// Panic when a command that matches CREATE_IO_COMPLETION_QUEUE is seen in the
/// admin queue:
/// ```no_run
/// use mesh::CellUpdater;
/// use nvme_resources::fault::FaultConfiguration;
/// use nvme_resources::fault::AdminQueueFaultConfig;
/// use nvme_resources::fault::CommandMatch;
/// use nvme_spec::Command;
/// use nvme_resources::fault::AdminQueueFaultBehavior;
/// use nvme_resources::NvmeFaultControllerHandle;
/// use guid::Guid;
/// use zerocopy::FromZeros;
/// use zerocopy::IntoBytes;
///
/// pub fn example_fault() {
///     let mut fault_start_updater = CellUpdater::new(false);
///
///     // Setup command matches
///     let mut command = Command::new_zeroed();
///     let mut mask = Command::new_zeroed();
///
///     command.cdw0 = command.cdw0.with_opcode(nvme_spec::AdminOpcode::CREATE_IO_COMPLETION_QUEUE.0);
///     mask.cdw0 = mask.cdw0.with_opcode(u8::MAX);
///
///     let fault_configuration = FaultConfiguration::new(fault_start_updater.cell())
///         .with_admin_queue_fault(
///             AdminQueueFaultConfig::new().with_submission_queue_fault(
///                 CommandMatch {
///                     command: command,
///                     mask: mask.as_bytes().try_into().expect("mask should be 64 bytes"),
///                 },
///                 AdminQueueFaultBehavior::Panic("Received a CREATE_IO_COMPLETION_QUEUE command".to_string()),
///             )
///         );
///     let fault_controller_handle = NvmeFaultControllerHandle {
///         subsystem_id: Guid::new_random(),
///         msix_count: 10,
///         max_io_queues: 10,
///         namespaces: vec![
///             // Define NamespaceDefinitions here
///         ],
///         fault_config: fault_configuration,
///     };
///     // Pass the controller handle in to the vm config to create and attach the fault controller. At this point the fault is inactive.
///     fault_start_updater.set(true); // Activate the fault injection.
///     // ... run test ...
///     fault_start_updater.set(false); // Deactivate the fault injection.
/// }
/// ```
#[derive(MeshPayload)]
pub struct FaultConfiguration {
    /// Fault active state
    pub fault_active: Cell<bool>,
    /// Fault to apply to the admin queues
    pub admin_fault: AdminQueueFaultConfig,
    /// Fault to apply to management layer of the controller. Option because it
    /// needs to be extracted by the PCI layer during initialization.
    pub pci_fault: Option<PciFaultConfig>,
    /// Fault for test triggered namespace change notifications
    pub namespace_fault: NamespaceFaultConfig,
    /// Fault to apply to all IO queues
    pub io_fault: Arc<IoQueueFaultConfig>,
}

impl FaultConfiguration {
    /// Create a new empty fault configuration
    pub fn new(fault_active: Cell<bool>) -> Self {
        // Ideally the faults should begin life as Option::None.
        // For now, use a dummy mesh channel for namespace fault to avoid
        // test setup complexity & special cases in the AdminHandler run loop.
        Self {
            fault_active: fault_active.clone(),
            admin_fault: AdminQueueFaultConfig::new(),
            pci_fault: Some(PciFaultConfig::new()),
            namespace_fault: NamespaceFaultConfig::new(mesh::channel().1),
            io_fault: Arc::new(IoQueueFaultConfig::new(fault_active)),
        }
    }

    /// Add a PCI fault configuration to the fault configuration
    pub fn with_pci_fault(mut self, pci_fault: PciFaultConfig) -> Self {
        self.pci_fault = Some(pci_fault);
        self
    }

    /// Add an admin queue fault configuration to the fault configuration
    pub fn with_admin_queue_fault(mut self, admin_fault: AdminQueueFaultConfig) -> Self {
        self.admin_fault = admin_fault;
        self
    }

    /// Add an IO queue fault configuration to the fault configuration
    pub fn with_io_queue_fault(mut self, io_fault: IoQueueFaultConfig) -> Self {
        self.io_fault = Arc::new(io_fault);
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

    /// Add a [`CommandMatch`] -> [`AdminQueueFaultBehavior`] mapping for the submission queue.
    ///
    /// # Panics
    /// Panics if an identical [`CommandMatch`] has already been configured.
    pub fn with_submission_queue_fault(
        mut self,
        pattern: CommandMatch,
        behaviour: AdminQueueFaultBehavior<Command>,
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

    /// Add a [`CommandMatch`] -> [`AdminQueueFaultBehavior`] mapping for the completion queue.
    ///
    /// # Panics
    /// Panics if an identical [`CommandMatch`] has already been configured.
    pub fn with_completion_queue_fault(
        mut self,
        pattern: CommandMatch,
        behaviour: AdminQueueFaultBehavior<Completion>,
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

impl IoQueueFaultConfig {
    /// Create an empty IO queue fault configuration
    pub fn new(fault_active: Cell<bool>) -> Self {
        Self {
            io_completion_queue_faults: vec![],
            fault_active,
        }
    }

    /// Add a [`CommandMatch`] -> [`IoQueueFaultBehavior`] mapping for the completion queue.
    ///
    /// # Panics
    /// Panics if an identical [`CommandMatch`] has already been configured.
    pub fn with_completion_queue_fault(
        mut self,
        pattern: CommandMatch,
        behaviour: IoQueueFaultBehavior,
    ) -> Self {
        if self
            .io_completion_queue_faults
            .iter()
            .any(|(c, _)| pattern == *c)
        {
            panic!(
                "Duplicate completion queue fault for Compare {:?} and Mask {:?}",
                pattern.command, pattern.mask
            );
        }

        self.io_completion_queue_faults.push((pattern, behaviour));
        self
    }
}
