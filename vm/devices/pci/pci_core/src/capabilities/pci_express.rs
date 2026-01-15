// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express Capability with Function Level Reset (FLR) support.

use super::PciCapability;
use crate::spec::caps::CapabilityId;
use crate::spec::caps::pci_express;
use crate::spec::caps::pci_express::{
    LinkSpeed, LinkWidth, PciExpressCapabilityHeader, SupportedLinkSpeedsVector,
};
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;

/// FLR bit is the 28th bit in the Device Capabilities register (0 indexed).
pub const PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK: u32 = 1 << 28;

/// Callback interface for handling Function Level Reset (FLR) events.
pub trait FlrHandler: Send + Sync + Inspect {
    /// Called when Function Level Reset is initiated.
    fn initiate_flr(&self);
}

#[derive(Debug, Inspect)]
struct PciExpressState {
    device_control: pci_express::DeviceControl,
    device_status: pci_express::DeviceStatus,
    link_control: pci_express::LinkControl,
    link_status: pci_express::LinkStatus,
    slot_control: pci_express::SlotControl,
    slot_status: pci_express::SlotStatus,
    root_control: pci_express::RootControl,
    root_status: pci_express::RootStatus,
    device_control_2: pci_express::DeviceControl2,
    device_status_2: pci_express::DeviceStatus2,
    link_control_2: pci_express::LinkControl2,
    link_status_2: pci_express::LinkStatus2,
    slot_control_2: pci_express::SlotControl2,
    slot_status_2: pci_express::SlotStatus2,
}

impl PciExpressState {
    fn new() -> Self {
        Self {
            device_control: pci_express::DeviceControl::new(),
            device_status: pci_express::DeviceStatus::new(),
            link_control: pci_express::LinkControl::new(),
            link_status: pci_express::LinkStatus::new()
                .with_current_link_speed(LinkSpeed::Speed32_0GtS.into_bits() as u16)
                .with_negotiated_link_width(LinkWidth::X16.into_bits() as u16),
            slot_control: pci_express::SlotControl::new(),
            slot_status: pci_express::SlotStatus::new(),
            root_control: pci_express::RootControl::new(),
            root_status: pci_express::RootStatus::new(),
            device_control_2: pci_express::DeviceControl2::new(),
            device_status_2: pci_express::DeviceStatus2::new(),
            link_control_2: pci_express::LinkControl2::new()
                .with_target_link_speed(LinkSpeed::Speed32_0GtS.into_bits() as u16),
            link_status_2: pci_express::LinkStatus2::new(),
            slot_control_2: pci_express::SlotControl2::new(),
            slot_status_2: pci_express::SlotStatus2::new(),
        }
    }
}

#[derive(Inspect)]
/// Configurable PCI Express capability.
pub struct PciExpressCapability {
    pcie_capabilities: pci_express::PciExpressCapabilities,
    device_capabilities: pci_express::DeviceCapabilities,
    link_capabilities: pci_express::LinkCapabilities,
    slot_capabilities: pci_express::SlotCapabilities,
    root_capabilities: pci_express::RootCapabilities,
    device_capabilities_2: pci_express::DeviceCapabilities2,
    link_capabilities_2: pci_express::LinkCapabilities2,
    slot_capabilities_2: pci_express::SlotCapabilities2,
    state: Arc<Mutex<PciExpressState>>,
    #[inspect(skip)]
    flr_handler: Option<Arc<dyn FlrHandler>>,
}

impl PciExpressCapability {
    /// Creates a new PCI Express capability with FLR support.
    ///
    /// # Arguments
    /// * `typ` - The spec-defined device or port type.
    /// * `flr_handler` - Optional handler to be called when FLR is initiated. This emulator will report that FLR is supported if flr_handler = Some(_)
    pub fn new(typ: pci_express::DevicePortType, flr_handler: Option<Arc<dyn FlrHandler>>) -> Self {
        Self {
            pcie_capabilities: pci_express::PciExpressCapabilities::new()
                .with_capability_version(2)
                .with_device_port_type(typ),
            device_capabilities: pci_express::DeviceCapabilities::new()
                .with_function_level_reset(flr_handler.is_some()),
            link_capabilities: pci_express::LinkCapabilities::new()
                .with_max_link_speed(LinkSpeed::Speed32_0GtS.into_bits()) // PCIe 32.0 GT/s speed
                .with_max_link_width(LinkWidth::X16.into_bits()), // x16 link width
            slot_capabilities: pci_express::SlotCapabilities::new(),
            root_capabilities: pci_express::RootCapabilities::new(),
            device_capabilities_2: pci_express::DeviceCapabilities2::new(),
            link_capabilities_2: pci_express::LinkCapabilities2::new()
                .with_supported_link_speeds_vector(SupportedLinkSpeedsVector::UpToGen5.into_bits()), // Support speeds up to PCIe Gen 5 (32.0 GT/s)
            slot_capabilities_2: pci_express::SlotCapabilities2::new(),
            state: Arc::new(Mutex::new(PciExpressState::new())),
            flr_handler,
        }
    }

    fn handle_device_control_status_write(&mut self, val: u32) {
        // Device Control (2 bytes) + Device Status (2 bytes)
        let new_control = pci_express::DeviceControl::from_bits(val as u16);
        let mut state = self.state.lock();

        // Check if FLR was initiated
        let old_flr = state.device_control.initiate_function_level_reset();
        let new_flr = new_control.initiate_function_level_reset();

        // DEVNOTE: It is "safe" to drop a new FLR request if there is still a previous
        // FLR request in progress. The PCIe spec indicates that such behavior is undefined,
        // so we choose to ignore the new FLR request.
        if new_flr && !old_flr {
            if let Some(handler) = &self.flr_handler {
                handler.initiate_flr();
            }
        }

        // Update the control register but clear the FLR bit as it's self-clearing
        state.device_control = new_control.with_initiate_function_level_reset(false);

        // Handle Device Status - most bits are write-1-to-clear
        let new_status = pci_express::DeviceStatus::from_bits((val >> 16) as u16);
        let mut current_status = state.device_status;

        // Clear bits that were written as 1 (write-1-to-clear semantics)
        if new_status.correctable_error_detected() {
            current_status.set_correctable_error_detected(false);
        }
        if new_status.non_fatal_error_detected() {
            current_status.set_non_fatal_error_detected(false);
        }
        if new_status.fatal_error_detected() {
            current_status.set_fatal_error_detected(false);
        }
        if new_status.unsupported_request_detected() {
            current_status.set_unsupported_request_detected(false);
        }

        state.device_status = current_status;
    }

    fn handle_slot_control_status_write(&mut self, val: u32) {
        // Slot Control (2 bytes) + Slot Status (2 bytes)
        let new_slot_control = pci_express::SlotControl::from_bits(val as u16);
        let mut state = self.state.lock();

        // Mask slot control bits based on slot capabilities
        // Only allow writes to bits that correspond to capabilities that are present
        let mut masked_control = new_slot_control;

        // If attention button is not present, attention button enable should be read-only (hardwired to 0)
        if !self.slot_capabilities.attention_button_present() {
            masked_control.set_attention_button_pressed_enable(false);
        }

        // If power controller is not present, power controller control should be read-only (hardwired to 0)
        if !self.slot_capabilities.power_controller_present() {
            masked_control.set_power_controller_control(false);
        }

        // If MRL sensor is not present, MRL sensor changed enable should be read-only (hardwired to 0)
        if !self.slot_capabilities.mrl_sensor_present() {
            masked_control.set_mrl_sensor_changed_enable(false);
        }

        // If attention indicator is not present, attention indicator control should be read-only (hardwired to 00b)
        if !self.slot_capabilities.attention_indicator_present() {
            masked_control.set_attention_indicator_control(0);
        }

        // If power indicator is not present, power indicator control should be read-only (hardwired to 00b)
        if !self.slot_capabilities.power_indicator_present() {
            masked_control.set_power_indicator_control(0);
        }

        // If hotplug is not capable, hotplug interrupt enable should be read-only (hardwired to 0)
        if !self.slot_capabilities.hot_plug_capable() {
            masked_control.set_hot_plug_interrupt_enable(false);
        }

        // If electromechanical interlock is not present, interlock control should be read-only (hardwired to 0)
        if !self.slot_capabilities.electromechanical_interlock_present() {
            masked_control.set_electromechanical_interlock_control(false);
        }

        // If no command completed support, command completed interrupt enable should be read-only (hardwired to 0)
        if self.slot_capabilities.no_command_completed_support() {
            masked_control.set_command_completed_interrupt_enable(false);
        }

        state.slot_control = masked_control;

        // Slot Status upper 16 bits - handle RW1C and RO bits properly
        let new_slot_status = pci_express::SlotStatus::from_bits((val >> 16) as u16);
        let mut current_slot_status = state.slot_status;

        // RW1C bits: writing 1 clears the bit, writing 0 leaves it unchanged
        // Clear bits where a 1 was written (RW1C behavior)
        if new_slot_status.attention_button_pressed() {
            current_slot_status.set_attention_button_pressed(false);
        }
        if new_slot_status.power_fault_detected() {
            current_slot_status.set_power_fault_detected(false);
        }
        if new_slot_status.mrl_sensor_changed() {
            current_slot_status.set_mrl_sensor_changed(false);
        }
        if new_slot_status.presence_detect_changed() {
            current_slot_status.set_presence_detect_changed(false);
        }
        if new_slot_status.command_completed() {
            current_slot_status.set_command_completed(false);
        }
        if new_slot_status.data_link_layer_state_changed() {
            current_slot_status.set_data_link_layer_state_changed(false);
        }

        // RO bits (mrl_sensor_state, presence_detect_state, electromechanical_interlock_status)
        // are not modified - they remain as they were

        state.slot_status = current_slot_status;
    }

    fn handle_link_control_status_write(&mut self, val: u32) {
        // Link Control (2 bytes) + Link Status (2 bytes)
        let new_link_control = pci_express::LinkControl::from_bits(val as u16);
        let mut state = self.state.lock();

        // Apply the new link control but ensure retrain_link always reads as 0
        let mut masked_control = new_link_control;
        masked_control.set_retrain_link(false); // retrain_link always reads as 0

        state.link_control = masked_control;
        // Link Status upper 16 bits - read-only, ignore any writes
    }

    fn handle_link_control_2_write(&mut self, val: u32) {
        // Link Control 2 (2 bytes) + Link Status 2 (2 bytes)
        let new_link_control_2 = pci_express::LinkControl2::from_bits(val as u16);
        let mut state = self.state.lock();

        // Validate that target_link_speed doesn't exceed max_link_speed from Link Capabilities
        let max_speed = self.link_capabilities.max_link_speed();
        let requested_speed = new_link_control_2.target_link_speed();

        // Clamp the target link speed to not exceed the maximum supported speed
        let actual_speed = if requested_speed > max_speed as u16 {
            max_speed as u16
        } else {
            requested_speed
        };

        // Update Link Control 2 with the validated speed
        state.link_control_2 = new_link_control_2.with_target_link_speed(actual_speed);

        // Update Link Status to reflect the target link speed as current link speed
        // This simulates the link retraining and speed negotiation completing immediately
        state.link_status = state.link_status.with_current_link_speed(actual_speed);

        // Link Status 2 upper 16 bits - mostly read-only, so we don't modify it
    }

    /// Enable hotplug support for this PCIe capability.
    /// This configures the appropriate registers to support hotpluggable devices.
    /// Panics if called on device types other than RootPort or DownstreamSwitchPort.
    ///
    /// # Arguments
    /// * `slot_number` - The physical slot number to assign to this hotplug-capable port
    pub fn with_hotplug_support(mut self, slot_number: u32) -> Self {
        use pci_express::DevicePortType;

        // Validate that hotplug is only enabled for appropriate port types
        let port_type = self.pcie_capabilities.device_port_type();
        match port_type {
            DevicePortType::RootPort | DevicePortType::DownstreamSwitchPort => {
                // Valid port types for hotplug support
            }
            DevicePortType::Endpoint | DevicePortType::UpstreamSwitchPort => {
                panic!(
                    "Hotplug support is not valid for device port type {:?}. \
                     Only RootPort and DownstreamSwitchPort support hotplug.",
                    port_type
                );
            }
        }

        // Enable slot implemented in PCIe capabilities when hotplug is enabled
        self.pcie_capabilities = self.pcie_capabilities.with_slot_implemented(true);

        // Enable hotplug capabilities in slot capabilities register
        self.slot_capabilities = self
            .slot_capabilities
            .with_hot_plug_surprise(true)
            .with_hot_plug_capable(true)
            .with_physical_slot_number(slot_number);

        // Enable Data Link Layer Link Active Reporting when hotplug is enabled
        self.link_capabilities = self
            .link_capabilities
            .with_data_link_layer_link_active_reporting(true);

        self
    }

    /// Set the presence detect state for the slot.
    /// This method only has effect if the slot is implemented (slot_implemented = true).
    /// If slot is not implemented, the call is silently ignored, as the spec says
    /// "If this register is implemented but the Slot Implemented bit is Clear,
    /// the field behavior of this entire register with the exception of the DLLSC bit is undefined."
    ///
    /// # Arguments
    /// * `present` - true if a device is present in the slot, false if the slot is empty
    pub fn set_presence_detect_state(&self, present: bool) {
        if !self.pcie_capabilities.slot_implemented() {
            // Silently ignore if slot is not implemented
            return;
        }

        let mut state = self.state.lock();
        state.slot_status =
            state
                .slot_status
                .with_presence_detect_state(if present { 1 } else { 0 });
    }
}

impl PciCapability for PciExpressCapability {
    fn label(&self) -> &str {
        "pci-express"
    }

    fn capability_id(&self) -> CapabilityId {
        CapabilityId::PCI_EXPRESS
    }

    fn len(&self) -> usize {
        // Implement the full PCI Express Capability structure (PCI Spec, Section 7.5.3):
        // 0x00: PCIe Capabilities (2 bytes) + Next Pointer (1 byte) + Capability ID (1 byte)
        // 0x04: Device Capabilities (4 bytes)
        // 0x08: Device Control (2 bytes) + Device Status (2 bytes)
        // 0x0C: Link Capabilities (4 bytes)
        // 0x10: Link Control (2 bytes) + Link Status (2 bytes)
        // 0x14: Slot Capabilities (4 bytes)
        // 0x18: Slot Control (2 bytes) + Slot Status (2 bytes)
        // 0x1C: Root Control (2 bytes) + Root Capabilities (2 bytes)
        // 0x20: Root Status (4 bytes)
        // 0x24: Device Capabilities 2 (4 bytes)
        // 0x28: Device Control 2 (2 bytes) + Device Status 2 (2 bytes)
        // 0x2C: Link Capabilities 2 (4 bytes)
        // 0x30: Link Control 2 (2 bytes) + Link Status 2 (2 bytes)
        // 0x34: Slot Capabilities 2 (4 bytes)
        // 0x38: Slot Control 2 (2 bytes) + Slot Status 2 (2 bytes)
        // Total: 60 bytes (0x3C)
        0x3C
    }

    fn read_u32(&self, offset: u16) -> u32 {
        let label = self.label();
        match PciExpressCapabilityHeader(offset) {
            PciExpressCapabilityHeader::PCIE_CAPS => {
                // PCIe Capabilities Register (16 bits) + Next Pointer (8 bits) + Capability ID (8 bits)
                (self.pcie_capabilities.into_bits() as u32) << 16
                    | CapabilityId::PCI_EXPRESS.0 as u32
            }
            PciExpressCapabilityHeader::DEVICE_CAPS => self.device_capabilities.into_bits(),
            PciExpressCapabilityHeader::DEVICE_CTL_STS => {
                let state = self.state.lock();
                let device_control = state.device_control.into_bits() as u32;
                let device_status = state.device_status.into_bits() as u32;
                device_control | (device_status << 16)
            }
            PciExpressCapabilityHeader::LINK_CAPS => self.link_capabilities.into_bits(),
            PciExpressCapabilityHeader::LINK_CTL_STS => {
                // Link Control (2 bytes) + Link Status (2 bytes)
                let state = self.state.lock();
                state.link_control.into_bits() as u32
                    | ((state.link_status.into_bits() as u32) << 16)
            }
            PciExpressCapabilityHeader::SLOT_CAPS => self.slot_capabilities.into_bits(),
            PciExpressCapabilityHeader::SLOT_CTL_STS => {
                // Slot Control (2 bytes) + Slot Status (2 bytes)
                let state = self.state.lock();
                state.slot_control.into_bits() as u32
                    | ((state.slot_status.into_bits() as u32) << 16)
            }
            PciExpressCapabilityHeader::ROOT_CTL_CAPS => {
                // Root Control (2 bytes) + Root Capabilities (2 bytes)
                let state = self.state.lock();
                state.root_control.into_bits() as u32
                    | ((self.root_capabilities.into_bits() as u32) << 16)
            }
            PciExpressCapabilityHeader::ROOT_STS => {
                // Root Status (4 bytes)
                let state = self.state.lock();
                state.root_status.into_bits()
            }
            PciExpressCapabilityHeader::DEVICE_CAPS_2 => self.device_capabilities_2.into_bits(),
            PciExpressCapabilityHeader::DEVICE_CTL_STS_2 => {
                // Device Control 2 (2 bytes) + Device Status 2 (2 bytes)
                let state = self.state.lock();
                state.device_control_2.into_bits() as u32
                    | ((state.device_status_2.into_bits() as u32) << 16)
            }
            PciExpressCapabilityHeader::LINK_CAPS_2 => self.link_capabilities_2.into_bits(),
            PciExpressCapabilityHeader::LINK_CTL_STS_2 => {
                // Link Control 2 (2 bytes) + Link Status 2 (2 bytes)
                let state = self.state.lock();
                state.link_control_2.into_bits() as u32
                    | ((state.link_status_2.into_bits() as u32) << 16)
            }
            PciExpressCapabilityHeader::SLOT_CAPS_2 => self.slot_capabilities_2.into_bits(),
            PciExpressCapabilityHeader::SLOT_CTL_STS_2 => {
                // Slot Control 2 (2 bytes) + Slot Status 2 (2 bytes)
                let state = self.state.lock();
                state.slot_control_2.into_bits() as u32
                    | ((state.slot_status_2.into_bits() as u32) << 16)
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    "unhandled pci express capability read"
                );
                0
            }
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        let label = self.label();
        match PciExpressCapabilityHeader(offset) {
            PciExpressCapabilityHeader::PCIE_CAPS => {
                // PCIe Capabilities register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "write to read-only pcie capabilities"
                );
            }
            PciExpressCapabilityHeader::DEVICE_CAPS => {
                // Device Capabilities register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "write to read-only device capabilities"
                );
            }
            PciExpressCapabilityHeader::DEVICE_CTL_STS => {
                self.handle_device_control_status_write(val);
            }
            PciExpressCapabilityHeader::LINK_CAPS => {
                // Link Capabilities register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "write to read-only link capabilities"
                );
            }
            PciExpressCapabilityHeader::LINK_CTL_STS => {
                self.handle_link_control_status_write(val);
            }
            PciExpressCapabilityHeader::SLOT_CAPS => {
                // Slot Capabilities register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "write to read-only slot capabilities"
                );
            }
            PciExpressCapabilityHeader::SLOT_CTL_STS => {
                self.handle_slot_control_status_write(val);
            }
            PciExpressCapabilityHeader::ROOT_CTL_CAPS => {
                // Root Control (2 bytes) + Root Capabilities (2 bytes)
                let mut state = self.state.lock();
                state.root_control = pci_express::RootControl::from_bits(val as u16);
                // Root Capabilities upper 16 bits are read-only
            }
            PciExpressCapabilityHeader::ROOT_STS => {
                // Root Status (4 bytes) - many bits are write-1-to-clear
                let mut state = self.state.lock();
                // For simplicity, we'll allow basic writes for now
                state.root_status = pci_express::RootStatus::from_bits(val);
            }
            PciExpressCapabilityHeader::DEVICE_CAPS_2 => {
                // Device Capabilities 2 register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "write to read-only device capabilities 2"
                );
            }
            PciExpressCapabilityHeader::DEVICE_CTL_STS_2 => {
                // Device Control 2 (2 bytes) + Device Status 2 (2 bytes)
                let mut state = self.state.lock();
                state.device_control_2 = pci_express::DeviceControl2::from_bits(val as u16);
                // Device Status 2 upper 16 bits - mostly read-only or write-1-to-clear
                state.device_status_2 = pci_express::DeviceStatus2::from_bits((val >> 16) as u16);
            }
            PciExpressCapabilityHeader::LINK_CAPS_2 => {
                // Link Capabilities 2 register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "write to read-only link capabilities 2"
                );
            }
            PciExpressCapabilityHeader::LINK_CTL_STS_2 => {
                self.handle_link_control_2_write(val);
            }
            PciExpressCapabilityHeader::SLOT_CAPS_2 => {
                // Slot Capabilities 2 register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "write to read-only slot capabilities 2"
                );
            }
            PciExpressCapabilityHeader::SLOT_CTL_STS_2 => {
                // Slot Control 2 (2 bytes) + Slot Status 2 (2 bytes)
                let mut state = self.state.lock();
                state.slot_control_2 = pci_express::SlotControl2::from_bits(val as u16);
                // Slot Status 2 upper 16 bits - mostly read-only or write-1-to-clear
                state.slot_status_2 = pci_express::SlotStatus2::from_bits((val >> 16) as u16);
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    val,
                    "unhandled pci express capability write"
                );
            }
        }
    }

    fn reset(&mut self) {
        let mut state = self.state.lock();
        *state = PciExpressState::new();
    }

    fn as_pci_express(&self) -> Option<&PciExpressCapability> {
        Some(self)
    }

    fn as_pci_express_mut(&mut self) -> Option<&mut PciExpressCapability> {
        Some(self)
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.capabilities.pci_express")]
        pub struct SavedState {
            #[mesh(1)]
            pub device_control: u16,
            #[mesh(2)]
            pub device_status: u16,
            #[mesh(3)]
            pub link_control: u16,
            #[mesh(4)]
            pub link_status: u16,
            #[mesh(5)]
            pub slot_control: u16,
            #[mesh(6)]
            pub slot_status: u16,
            #[mesh(7)]
            pub root_control: u16,
            #[mesh(8)]
            pub root_status: u32,
            #[mesh(9)]
            pub device_control_2: u16,
            #[mesh(10)]
            pub device_status_2: u16,
            #[mesh(11)]
            pub link_control_2: u16,
            #[mesh(12)]
            pub link_status_2: u16,
            #[mesh(13)]
            pub slot_control_2: u16,
            #[mesh(14)]
            pub slot_status_2: u16,
        }
    }

    impl SaveRestore for PciExpressCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let state = self.state.lock();
            let PciExpressState {
                device_control,
                device_status,
                link_control,
                link_status,
                slot_control,
                slot_status,
                root_control,
                root_status,
                device_control_2,
                device_status_2,
                link_control_2,
                link_status_2,
                slot_control_2,
                slot_status_2,
            } = &*state;

            Ok(state::SavedState {
                device_control: device_control.into_bits(),
                device_status: device_status.into_bits(),
                link_control: link_control.into_bits(),
                link_status: link_status.into_bits(),
                slot_control: slot_control.into_bits(),
                slot_status: slot_status.into_bits(),
                root_control: root_control.into_bits(),
                root_status: root_status.into_bits(),
                device_control_2: device_control_2.into_bits(),
                device_status_2: device_status_2.into_bits(),
                link_control_2: link_control_2.into_bits(),
                link_status_2: link_status_2.into_bits(),
                slot_control_2: slot_control_2.into_bits(),
                slot_status_2: slot_status_2.into_bits(),
            })
        }

        fn restore(&mut self, saved: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                device_control,
                device_status,
                link_control,
                link_status,
                slot_control,
                slot_status,
                root_control,
                root_status,
                device_control_2,
                device_status_2,
                link_control_2,
                link_status_2,
                slot_control_2,
                slot_status_2,
            } = saved;

            let mut state = self.state.lock();
            state.device_control = pci_express::DeviceControl::from_bits(device_control);
            state.device_status = pci_express::DeviceStatus::from_bits(device_status);
            state.link_control = pci_express::LinkControl::from_bits(link_control);
            state.link_status = pci_express::LinkStatus::from_bits(link_status);
            state.slot_control = pci_express::SlotControl::from_bits(slot_control);
            state.slot_status = pci_express::SlotStatus::from_bits(slot_status);
            state.root_control = pci_express::RootControl::from_bits(root_control);
            state.root_status = pci_express::RootStatus::from_bits(root_status);
            state.device_control_2 = pci_express::DeviceControl2::from_bits(device_control_2);
            state.device_status_2 = pci_express::DeviceStatus2::from_bits(device_status_2);
            state.link_control_2 = pci_express::LinkControl2::from_bits(link_control_2);
            state.link_status_2 = pci_express::LinkStatus2::from_bits(link_status_2);
            state.slot_control_2 = pci_express::SlotControl2::from_bits(slot_control_2);
            state.slot_status_2 = pci_express::SlotStatus2::from_bits(slot_status_2);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::caps::pci_express::DevicePortType;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug)]
    struct TestFlrHandler {
        flr_initiated: AtomicBool,
    }

    impl TestFlrHandler {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                flr_initiated: AtomicBool::new(false),
            })
        }

        fn was_flr_initiated(&self) -> bool {
            self.flr_initiated.load(Ordering::Acquire)
        }

        fn reset(&self) {
            self.flr_initiated.store(false, Ordering::Release);
        }
    }

    impl FlrHandler for TestFlrHandler {
        fn initiate_flr(&self) {
            self.flr_initiated.store(true, Ordering::Release);
        }
    }

    impl Inspect for TestFlrHandler {
        fn inspect(&self, req: inspect::Request<'_>) {
            req.respond()
                .field("flr_initiated", self.flr_initiated.load(Ordering::Acquire));
        }
    }

    #[test]
    fn test_pci_express_capability_read_u32_endpoint() {
        let flr_handler = TestFlrHandler::new();
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, Some(flr_handler));

        // Test PCIe Capabilities Register (offset 0x00)
        let caps_val = cap.read_u32(0x00);
        assert_eq!(caps_val & 0xFF, 0x10); // Capability ID = 0x10
        assert_eq!((caps_val >> 8) & 0xFF, 0x00); // Next Pointer = 0x00
        assert_eq!((caps_val >> 16) & 0xFFFF, 0x0002); // PCIe Caps: Version 2, Device/Port Type 0

        // Test Device Capabilities Register (offset 0x04)
        let device_caps_val = cap.read_u32(0x04);
        assert_eq!(
            device_caps_val & PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK,
            PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK
        ); // FLR bit should be set

        // Test Device Control/Status Register (offset 0x08) - should be zero initially
        let device_ctl_sts_val = cap.read_u32(0x08);
        assert_eq!(device_ctl_sts_val, 0); // Both control and status should be 0

        // Test Link Control/Status Register (offset 0x10) - should have link status initialized
        let link_ctl_sts_val = cap.read_u32(0x10);
        let expected_link_status = (LinkSpeed::Speed32_0GtS.into_bits() as u16)
            | ((LinkWidth::X16.into_bits() as u16) << 4); // current_link_speed + negotiated_link_width
        assert_eq!(link_ctl_sts_val, (expected_link_status as u32) << 16); // Link status is in upper 16 bits
    }

    #[test]
    fn test_pci_express_capability_read_u32_root_port() {
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        // Test PCIe Capabilities Register (offset 0x00)
        let caps_val = cap.read_u32(0x00);
        assert_eq!(caps_val & 0xFF, 0x10); // Capability ID = 0x10
        assert_eq!((caps_val >> 8) & 0xFF, 0x00); // Next Pointer = 0x00
        assert_eq!((caps_val >> 16) & 0xFFFF, 0x0042); // PCIe Caps: Version 2, Device/Port Type 4
    }

    #[test]
    fn test_pci_express_capability_read_u32_no_flr() {
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Test Device Capabilities Register (offset 0x04) - FLR should not be set
        let device_caps_val = cap.read_u32(0x04);
        assert_eq!(device_caps_val & PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK, 0);
    }

    #[test]
    fn test_pci_express_capability_write_u32_readonly_registers() {
        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Try to write to read-only PCIe Capabilities Register (offset 0x00)
        let original_caps = cap.read_u32(0x00);
        cap.write_u32(0x00, 0xFFFFFFFF);
        assert_eq!(cap.read_u32(0x00), original_caps); // Should be unchanged

        // Try to write to read-only Device Capabilities Register (offset 0x04)
        let original_device_caps = cap.read_u32(0x04);
        cap.write_u32(0x04, 0xFFFFFFFF);
        assert_eq!(cap.read_u32(0x04), original_device_caps); // Should be unchanged
    }

    #[test]
    fn test_pci_express_capability_write_u32_device_control() {
        let flr_handler = TestFlrHandler::new();
        let mut cap =
            PciExpressCapability::new(DevicePortType::Endpoint, Some(flr_handler.clone()));

        // Initial state should have FLR bit clear
        let initial_ctl_sts = cap.read_u32(0x08);
        assert_eq!(initial_ctl_sts & 0xFFFF, 0); // Device Control should be 0

        // Test writing to Device Control Register (lower 16 bits of offset 0x08)
        // Set some control bits but not FLR initially
        cap.write_u32(0x08, 0x0001); // Enable correctable error reporting (bit 0)
        let device_ctl_sts = cap.read_u32(0x08);
        assert_eq!(device_ctl_sts & 0xFFFF, 0x0001); // Device Control should be set
        assert!(!flr_handler.was_flr_initiated()); // FLR should not be triggered

        // Test FLR initiation (bit 15 of Device Control)
        flr_handler.reset();
        cap.write_u32(0x08, 0x8001); // Set FLR bit (bit 15) and other control bits
        let device_ctl_sts_after_flr = cap.read_u32(0x08);
        assert_eq!(device_ctl_sts_after_flr & 0xFFFF, 0x0001); // FLR bit should be cleared, others remain
        assert!(flr_handler.was_flr_initiated()); // FLR should be triggered

        // Test that writing FLR bit when it's already been triggered behaves correctly
        flr_handler.reset();
        // After the previous FLR, device_control should have bit 0 set but FLR clear
        // So writing 0x8000 (only FLR bit) should trigger FLR again
        cap.write_u32(0x08, 0x8000); // Set FLR bit only
        let device_ctl_sts_final = cap.read_u32(0x08);
        assert_eq!(device_ctl_sts_final & 0xFFFF, 0x0000); // All bits should be cleared (FLR self-clears, bit 0 was overwritten)
        assert!(flr_handler.was_flr_initiated()); // Should trigger because FLR transitioned from 0 to 1
    }

    #[test]
    fn test_pci_express_capability_write_u32_device_status() {
        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Manually set some status bits to test write-1-to-clear behavior
        {
            let mut state = cap.state.lock();
            state.device_status.set_correctable_error_detected(true);
            state.device_status.set_non_fatal_error_detected(true);
            state.device_status.set_fatal_error_detected(true);
            state.device_status.set_unsupported_request_detected(true);
        }

        // Check that status bits are set
        let device_ctl_sts = cap.read_u32(0x08);
        let status_bits = (device_ctl_sts >> 16) & 0xFFFF;
        assert_ne!(status_bits & 0x0F, 0); // Some status bits should be set

        // Write 1 to clear correctable error bit (bit 0 of status)
        cap.write_u32(0x08, 0x00010000); // Write 1 to bit 16 (correctable error in upper 16 bits)
        let device_ctl_sts_after = cap.read_u32(0x08);
        let status_bits_after = (device_ctl_sts_after >> 16) & 0xFFFF;
        assert_eq!(status_bits_after & 0x01, 0); // Correctable error bit should be cleared
        assert_ne!(status_bits_after & 0x0E, 0); // Other error bits should still be set

        // Clear all remaining error bits
        cap.write_u32(0x08, 0x000E0000); // Write 1 to bits 17-19 (other error bits)
        let final_status = (cap.read_u32(0x08) >> 16) & 0xFFFF;
        assert_eq!(final_status & 0x0F, 0); // All error bits should be cleared
    }

    #[test]
    fn test_pci_express_capability_write_u32_unhandled_offset() {
        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Writing to unhandled offset should not panic
        cap.write_u32(0x10, 0xFFFFFFFF);
        // Should not crash and should not affect other registers
        assert_eq!(cap.read_u32(0x08), 0); // Device Control/Status should still be 0
    }

    #[test]
    fn test_pci_express_capability_reset() {
        let flr_handler = TestFlrHandler::new();
        let mut cap =
            PciExpressCapability::new(DevicePortType::Endpoint, Some(flr_handler.clone()));

        // Set some state
        cap.write_u32(0x08, 0x0001); // Set some device control bits

        // Manually set some status bits
        {
            let mut state = cap.state.lock();
            state.device_status.set_correctable_error_detected(true);
        }

        // Verify state is set
        let device_ctl_sts = cap.read_u32(0x08);
        assert_ne!(device_ctl_sts, 0);

        // Reset the capability
        cap.reset();

        // Verify state is cleared
        let device_ctl_sts_after_reset = cap.read_u32(0x08);
        assert_eq!(device_ctl_sts_after_reset, 0);
    }

    #[test]
    fn test_pci_express_capability_extended_registers() {
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Test that extended registers return proper default values and don't crash
        // Link Capabilities should have default speed (Speed32_0GtS) and width (X16)
        let expected_link_caps =
            LinkSpeed::Speed32_0GtS.into_bits() | (LinkWidth::X16.into_bits() << 4); // speed + (width << 4) = 5 + (16 << 4) = 5 + 256 = 261
        assert_eq!(cap.read_u32(0x0C), expected_link_caps); // Link Capabilities
        // Link Control/Status should have Link Status with current_link_speed=5 and negotiated_link_width=16
        let expected_link_ctl_sts = (LinkSpeed::Speed32_0GtS.into_bits() as u16)
            | ((LinkWidth::X16.into_bits() as u16) << 4); // current_link_speed (bits 0-3) + negotiated_link_width (bits 4-9) = 5 + (16 << 4) = 5 + 256 = 261
        assert_eq!(cap.read_u32(0x10), (expected_link_ctl_sts as u32) << 16); // Link Control/Status (status in upper 16 bits)
        assert_eq!(cap.read_u32(0x14), 0); // Slot Capabilities
        assert_eq!(cap.read_u32(0x18), 0); // Slot Control/Status
        assert_eq!(cap.read_u32(0x1C), 0); // Root Control/Capabilities
        assert_eq!(cap.read_u32(0x20), 0); // Root Status
        assert_eq!(cap.read_u32(0x24), 0); // Device Capabilities 2
        assert_eq!(cap.read_u32(0x28), 0); // Device Control/Status 2
        // Link Capabilities 2 has supported_link_speeds_vector set to UpToGen5
        let expected_link_caps_2 = SupportedLinkSpeedsVector::UpToGen5.into_bits() << 1; // supported_link_speeds_vector at bits 1-7 = 31 << 1 = 62
        assert_eq!(cap.read_u32(0x2C), expected_link_caps_2); // Link Capabilities 2
        // Link Control/Status 2 - Link Control 2 should have target_link_speed set to Speed32_0GtS (5)
        let expected_link_ctl_sts_2 = LinkSpeed::Speed32_0GtS.into_bits() as u16; // target_link_speed in lower 4 bits = 5
        assert_eq!(cap.read_u32(0x30), expected_link_ctl_sts_2 as u32); // Link Control/Status 2
        assert_eq!(cap.read_u32(0x34), 0); // Slot Capabilities 2
        assert_eq!(cap.read_u32(0x38), 0); // Slot Control/Status 2
    }

    #[test]
    fn test_pci_express_capability_length() {
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, None);
        assert_eq!(cap.len(), 0x3C); // Should be 60 bytes (0x3C)
    }

    #[test]
    fn test_pci_express_capability_label() {
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, None);
        assert_eq!(cap.label(), "pci-express");
    }

    #[test]
    fn test_pci_express_capability_with_hotplug_support() {
        // Test with RootPort (should work)
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        let cap_with_hotplug = cap.with_hotplug_support(1);

        // Verify that the method doesn't crash and returns the capability
        assert_eq!(cap_with_hotplug.label(), "pci-express");
        assert_eq!(cap_with_hotplug.len(), 0x3C);

        // Verify hotplug capabilities are set
        assert!(cap_with_hotplug.slot_capabilities.hot_plug_surprise());
        assert!(cap_with_hotplug.slot_capabilities.hot_plug_capable());
        assert_eq!(cap_with_hotplug.slot_capabilities.physical_slot_number(), 1);

        // Verify that slot_implemented is set in PCIe capabilities
        assert!(
            cap_with_hotplug.pcie_capabilities.slot_implemented(),
            "slot_implemented should be true when hotplug is enabled"
        );

        // Test with DownstreamSwitchPort (should work)
        let cap2 = PciExpressCapability::new(DevicePortType::DownstreamSwitchPort, None);
        let cap2_with_hotplug = cap2.with_hotplug_support(2);

        assert!(cap2_with_hotplug.slot_capabilities.hot_plug_surprise());
        assert!(cap2_with_hotplug.slot_capabilities.hot_plug_capable());
        assert_eq!(
            cap2_with_hotplug.slot_capabilities.physical_slot_number(),
            2
        );

        // Verify that slot_implemented is set for downstream switch port too
        assert!(
            cap2_with_hotplug.pcie_capabilities.slot_implemented(),
            "slot_implemented should be true when hotplug is enabled"
        );

        // Test that non-hotplug capability doesn't have slot_implemented set
        let cap_no_hotplug = PciExpressCapability::new(DevicePortType::RootPort, None);
        assert!(
            !cap_no_hotplug.pcie_capabilities.slot_implemented(),
            "slot_implemented should be false when hotplug is not enabled"
        );
    }

    #[test]
    #[should_panic(expected = "Hotplug support is not valid for device port type Endpoint")]
    fn test_pci_express_capability_with_hotplug_support_endpoint_panics() {
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, None);
        cap.with_hotplug_support(1);
    }

    #[test]
    #[should_panic(
        expected = "Hotplug support is not valid for device port type UpstreamSwitchPort"
    )]
    fn test_pci_express_capability_with_hotplug_support_upstream_panics() {
        let cap = PciExpressCapability::new(DevicePortType::UpstreamSwitchPort, None);
        cap.with_hotplug_support(1);
    }

    #[test]
    fn test_slot_control_write_protection() {
        // Create a root port capability with hotplug support but limited slot capabilities
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        cap = cap.with_hotplug_support(1);

        // Modify slot capabilities to disable some features for testing
        cap.slot_capabilities.set_attention_button_present(false);
        cap.slot_capabilities.set_power_controller_present(false);
        cap.slot_capabilities.set_mrl_sensor_present(false);
        cap.slot_capabilities.set_attention_indicator_present(false);
        cap.slot_capabilities.set_power_indicator_present(false);
        cap.slot_capabilities
            .set_electromechanical_interlock_present(false);
        cap.slot_capabilities.set_no_command_completed_support(true);

        // Try to write to slot control register with all bits set
        let slot_ctl_sts_offset = 0x18; // SLOT_CTL_STS offset
        let val_to_write = 0xFFFFFFFF; // All bits set in both control and status

        cap.write_u32(slot_ctl_sts_offset, val_to_write);

        // Read back the slot control register (lower 16 bits)
        let read_back = cap.read_u32(slot_ctl_sts_offset);
        let slot_control_value = read_back as u16;
        let slot_control = pci_express::SlotControl::from_bits(slot_control_value);

        // Verify that features not present in capabilities were not set in control register
        assert!(
            !slot_control.attention_button_pressed_enable(),
            "Attention button enable should be 0 when capability not present"
        );
        assert!(
            !slot_control.power_controller_control(),
            "Power controller control should be 0 when capability not present"
        );
        assert!(
            !slot_control.mrl_sensor_changed_enable(),
            "MRL sensor changed enable should be 0 when capability not present"
        );
        assert_eq!(
            slot_control.attention_indicator_control(),
            0,
            "Attention indicator control should be 0 when capability not present"
        );
        assert_eq!(
            slot_control.power_indicator_control(),
            0,
            "Power indicator control should be 0 when capability not present"
        );
        assert!(
            !slot_control.electromechanical_interlock_control(),
            "Electromechanical interlock control should be 0 when capability not present"
        );
        assert!(
            !slot_control.command_completed_interrupt_enable(),
            "Command completed interrupt enable should be 0 when no command completed support"
        );

        // However, hotplug interrupt enable should be settable since hotplug is capable
        assert!(
            slot_control.hot_plug_interrupt_enable(),
            "Hotplug interrupt enable should be settable when hotplug capable"
        );
    }

    #[test]
    fn test_link_control_retrain_link_behavior() {
        // Test that retrain_link always reads as 0 regardless of what is written
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        let link_ctl_sts_offset = 0x10; // LINK_CTL_STS offset

        // Write a value with retrain_link bit set (bit 5)
        let write_val = 0x0020; // retrain_link bit (bit 5) = 1
        cap.write_u32(link_ctl_sts_offset, write_val);

        // Read back and verify retrain_link is always 0
        let read_back = cap.read_u32(link_ctl_sts_offset);
        let link_control = pci_express::LinkControl::from_bits(read_back as u16);

        assert!(
            !link_control.retrain_link(),
            "retrain_link should always read as 0"
        );

        // Verify other bits can still be set (except retrain_link)
        let write_val_2 = 0x0001; // aspm_control bit 0 = 1
        cap.write_u32(link_ctl_sts_offset, write_val_2);

        let read_back_2 = cap.read_u32(link_ctl_sts_offset);
        let link_control_2 = pci_express::LinkControl::from_bits(read_back_2 as u16);

        assert_eq!(
            link_control_2.aspm_control(),
            1,
            "Other control bits should be settable"
        );
        assert!(
            !link_control_2.retrain_link(),
            "retrain_link should still read as 0"
        );
    }

    #[test]
    fn test_hotplug_link_capabilities() {
        // Test that Data Link Layer Link Active Reporting is enabled with hotplug
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        let cap_with_hotplug = cap.with_hotplug_support(1);

        let link_caps_offset = 0x0C; // LINK_CAPS offset
        let link_caps = cap_with_hotplug.read_u32(link_caps_offset);
        let link_capabilities = pci_express::LinkCapabilities::from_bits(link_caps);

        // Verify that Data Link Layer Link Active Reporting is enabled
        assert!(
            link_capabilities.data_link_layer_link_active_reporting(),
            "Data Link Layer Link Active Reporting should be enabled for hotplug"
        );

        // Verify default speed and width are still correct
        assert_eq!(
            link_capabilities.max_link_speed(),
            LinkSpeed::Speed32_0GtS.into_bits(),
            "Max link speed should be Speed32_0GtS (PCIe 32.0 GT/s)"
        );
        assert_eq!(
            link_capabilities.max_link_width(),
            LinkWidth::X16.into_bits(),
            "Max link width should be X16 (x16)"
        );

        // Test that non-hotplug capability doesn't have Data Link Layer Link Active Reporting
        let cap_no_hotplug = PciExpressCapability::new(DevicePortType::RootPort, None);
        let link_caps_no_hotplug = cap_no_hotplug.read_u32(link_caps_offset);
        let link_capabilities_no_hotplug =
            pci_express::LinkCapabilities::from_bits(link_caps_no_hotplug);

        assert!(
            !link_capabilities_no_hotplug.data_link_layer_link_active_reporting(),
            "Data Link Layer Link Active Reporting should be disabled without hotplug"
        );
    }

    #[test]
    fn test_link_status_read_only() {
        // Test that Link Status register is read-only and cannot be modified by writes
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        let link_ctl_sts_offset = 0x10; // LINK_CTL_STS offset

        // Set some initial link status values (this would normally be done by hardware)
        {
            let mut state = cap.state.lock();
            state.link_status.set_current_link_speed(0b0001); // Set initial speed
            state.link_status.set_negotiated_link_width(0b000001); // Set initial width
            state.link_status.set_link_training(true); // Set link training active
            state.link_status.set_data_link_layer_link_active(true); // Set DLL active
        }

        // Read initial values
        let initial_read = cap.read_u32(link_ctl_sts_offset);
        let initial_link_status = pci_express::LinkStatus::from_bits((initial_read >> 16) as u16);

        // Verify initial values are set
        assert_eq!(
            initial_link_status.current_link_speed(),
            0b0001,
            "Initial link speed should be set"
        );
        assert_eq!(
            initial_link_status.negotiated_link_width(),
            0b000001,
            "Initial link width should be set"
        );
        assert!(
            initial_link_status.link_training(),
            "Initial link training should be active"
        );
        assert!(
            initial_link_status.data_link_layer_link_active(),
            "Initial DLL should be active"
        );

        // Try to write different values to Link Status (upper 16 bits) while also writing to Link Control
        let write_val = 0xFFFF0001; // Upper 16 bits all 1s (Link Status), lower 16 bits = 1 (Link Control)
        cap.write_u32(link_ctl_sts_offset, write_val);

        // Read back and verify Link Status hasn't changed
        let after_write = cap.read_u32(link_ctl_sts_offset);
        let final_link_status = pci_express::LinkStatus::from_bits((after_write >> 16) as u16);
        let final_link_control = pci_express::LinkControl::from_bits(after_write as u16);

        // Link Status should remain unchanged (read-only)
        assert_eq!(
            final_link_status.current_link_speed(),
            initial_link_status.current_link_speed(),
            "Link Status current_link_speed should be read-only"
        );
        assert_eq!(
            final_link_status.negotiated_link_width(),
            initial_link_status.negotiated_link_width(),
            "Link Status negotiated_link_width should be read-only"
        );
        assert_eq!(
            final_link_status.link_training(),
            initial_link_status.link_training(),
            "Link Status link_training should be read-only"
        );
        assert_eq!(
            final_link_status.data_link_layer_link_active(),
            initial_link_status.data_link_layer_link_active(),
            "Link Status data_link_layer_link_active should be read-only"
        );

        // But Link Control should be modifiable
        assert_eq!(
            final_link_control.aspm_control(),
            1,
            "Link Control should be writable"
        );
    }

    #[test]
    fn test_slot_status_rw1c_behavior() {
        // Create a root port capability with hotplug support
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        cap = cap.with_hotplug_support(1);

        let slot_ctl_sts_offset = 0x18; // SLOT_CTL_STS offset

        // First, simulate setting some status bits (this would normally be done by hardware)
        {
            let mut state = cap.state.lock();
            state.slot_status.set_attention_button_pressed(true);
            state.slot_status.set_power_fault_detected(true);
            state.slot_status.set_mrl_sensor_changed(true);
            state.slot_status.set_presence_detect_changed(true);
            state.slot_status.set_command_completed(true);
            state.slot_status.set_data_link_layer_state_changed(true);
            // Set some RO bits too
            state.slot_status.set_mrl_sensor_state(1);
            state.slot_status.set_presence_detect_state(1);
            state.slot_status.set_electromechanical_interlock_status(1);
        }

        // Read the initial status to verify all bits are set
        let initial_read = cap.read_u32(slot_ctl_sts_offset);
        let initial_status = pci_express::SlotStatus::from_bits((initial_read >> 16) as u16);
        assert!(
            initial_status.attention_button_pressed(),
            "Initial attention button pressed should be set"
        );
        assert!(
            initial_status.power_fault_detected(),
            "Initial power fault detected should be set"
        );
        assert!(
            initial_status.mrl_sensor_changed(),
            "Initial MRL sensor changed should be set"
        );
        assert!(
            initial_status.presence_detect_changed(),
            "Initial presence detect changed should be set"
        );
        assert!(
            initial_status.command_completed(),
            "Initial command completed should be set"
        );
        assert!(
            initial_status.data_link_layer_state_changed(),
            "Initial data link layer state changed should be set"
        );
        assert_eq!(
            initial_status.mrl_sensor_state(),
            1,
            "Initial MRL sensor state should be set"
        );
        assert_eq!(
            initial_status.presence_detect_state(),
            1,
            "Initial presence detect state should be set"
        );
        assert_eq!(
            initial_status.electromechanical_interlock_status(),
            1,
            "Initial electromechanical interlock status should be set"
        );

        // Write 1 to clear specific RW1C bits (upper 16 bits contain status)
        // Write 1s only to some RW1C bits to test selective clearing
        // Bit positions: attention_button_pressed(0), command_completed(4), data_link_layer_state_changed(8)
        let write_val = (0b0000_0001_0001_0001_u16 as u32) << 16; // Clear bits 0, 4, and 8
        cap.write_u32(slot_ctl_sts_offset, write_val);

        // Read back and verify RW1C behavior
        let after_write = cap.read_u32(slot_ctl_sts_offset);
        let final_status = pci_express::SlotStatus::from_bits((after_write >> 16) as u16);

        // RW1C bits that were written with 1 should be cleared
        assert!(
            !final_status.attention_button_pressed(),
            "Attention button pressed should be cleared after write-1"
        );
        assert!(
            !final_status.command_completed(),
            "Command completed should be cleared after write-1"
        );
        assert!(
            !final_status.data_link_layer_state_changed(),
            "Data link layer state changed should be cleared after write-1"
        );

        // RW1C bits that were written with 0 should remain unchanged
        assert!(
            final_status.power_fault_detected(),
            "Power fault detected should remain set (write-0)"
        );
        assert!(
            final_status.mrl_sensor_changed(),
            "MRL sensor changed should remain set (write-0)"
        );
        assert!(
            final_status.presence_detect_changed(),
            "Presence detect changed should remain set (write-0)"
        );

        // RO bits should remain unchanged regardless of what was written
        assert_eq!(
            final_status.mrl_sensor_state(),
            1,
            "MRL sensor state should remain unchanged (RO)"
        );
        assert_eq!(
            final_status.presence_detect_state(),
            1,
            "Presence detect state should remain unchanged (RO)"
        );
        assert_eq!(
            final_status.electromechanical_interlock_status(),
            1,
            "Electromechanical interlock status should remain unchanged (RO)"
        );
    }

    #[test]
    fn test_link_control_2_target_speed_validation() {
        // Test that target link speed is validated against max link speed and reflected in link status
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        let link_ctl_sts_2_offset = 0x30; // LINK_CTL_STS_2 offset

        // Initially, target link speed should be Speed32_0GtS (5) and current link speed should match
        let initial_read = cap.read_u32(link_ctl_sts_2_offset);
        let initial_link_control_2 = pci_express::LinkControl2::from_bits(initial_read as u16);
        assert_eq!(
            initial_link_control_2.target_link_speed(),
            LinkSpeed::Speed32_0GtS.into_bits() as u16,
            "Initial target link speed should be Speed32_0GtS"
        );

        // Check that link status reflects this speed
        let link_ctl_sts_offset = 0x10; // LINK_CTL_STS offset
        let link_ctl_sts = cap.read_u32(link_ctl_sts_offset);
        let link_status = pci_express::LinkStatus::from_bits((link_ctl_sts >> 16) as u16);
        assert_eq!(
            link_status.current_link_speed(),
            LinkSpeed::Speed32_0GtS.into_bits() as u16,
            "Initial current link speed should match target speed"
        );
        assert_eq!(
            link_status.negotiated_link_width(),
            LinkWidth::X16.into_bits() as u16,
            "Initial negotiated link width should be X16"
        );

        // Test writing a valid speed (Speed16_0GtS = 4) that's less than max speed (Speed32_0GtS = 5)
        let valid_speed = LinkSpeed::Speed16_0GtS.into_bits() as u16; // 4
        cap.write_u32(link_ctl_sts_2_offset, valid_speed as u32);

        // Verify target link speed was set correctly
        let after_valid_write = cap.read_u32(link_ctl_sts_2_offset);
        let link_control_2_after_valid =
            pci_express::LinkControl2::from_bits(after_valid_write as u16);
        assert_eq!(
            link_control_2_after_valid.target_link_speed(),
            valid_speed,
            "Target link speed should be set to requested valid speed"
        );

        // Verify current link speed was updated in link status
        let link_ctl_sts_after_valid = cap.read_u32(link_ctl_sts_offset);
        let link_status_after_valid =
            pci_express::LinkStatus::from_bits((link_ctl_sts_after_valid >> 16) as u16);
        assert_eq!(
            link_status_after_valid.current_link_speed(),
            valid_speed,
            "Current link speed should be updated to match target speed"
        );

        // Test writing an invalid speed (Speed64_0GtS = 6) that exceeds max speed (Speed32_0GtS = 5)
        let invalid_speed = LinkSpeed::Speed64_0GtS.into_bits() as u16; // 6
        cap.write_u32(link_ctl_sts_2_offset, invalid_speed as u32);

        // Verify target link speed was clamped to max speed
        let after_invalid_write = cap.read_u32(link_ctl_sts_2_offset);
        let link_control_2_after_invalid =
            pci_express::LinkControl2::from_bits(after_invalid_write as u16);
        let max_speed = LinkSpeed::Speed32_0GtS.into_bits() as u16; // 5
        assert_eq!(
            link_control_2_after_invalid.target_link_speed(),
            max_speed,
            "Target link speed should be clamped to max supported speed"
        );

        // Verify current link speed was updated to the clamped value
        let link_ctl_sts_after_invalid = cap.read_u32(link_ctl_sts_offset);
        let link_status_after_invalid =
            pci_express::LinkStatus::from_bits((link_ctl_sts_after_invalid >> 16) as u16);
        assert_eq!(
            link_status_after_invalid.current_link_speed(),
            max_speed,
            "Current link speed should be updated to clamped max speed"
        );

        // Verify that link width remains unchanged throughout
        assert_eq!(
            link_status_after_valid.negotiated_link_width(),
            LinkWidth::X16.into_bits() as u16,
            "Negotiated link width should remain unchanged"
        );
        assert_eq!(
            link_status_after_invalid.negotiated_link_width(),
            LinkWidth::X16.into_bits() as u16,
            "Negotiated link width should remain unchanged"
        );
    }

    #[test]
    fn test_with_hotplug_support_slot_number() {
        // Test that slot numbers are properly set when enabling hotplug support

        // Test with slot number 5
        let cap1 = PciExpressCapability::new(DevicePortType::RootPort, None);
        let cap1_with_hotplug = cap1.with_hotplug_support(5);

        assert!(cap1_with_hotplug.slot_capabilities.hot_plug_capable());
        assert_eq!(
            cap1_with_hotplug.slot_capabilities.physical_slot_number(),
            5
        );

        // Test with slot number 0
        let cap2 = PciExpressCapability::new(DevicePortType::DownstreamSwitchPort, None);
        let cap2_with_hotplug = cap2.with_hotplug_support(0);

        assert!(cap2_with_hotplug.slot_capabilities.hot_plug_capable());
        assert_eq!(
            cap2_with_hotplug.slot_capabilities.physical_slot_number(),
            0
        );

        // Test with a larger slot number
        let cap3 = PciExpressCapability::new(DevicePortType::RootPort, None);
        let cap3_with_hotplug = cap3.with_hotplug_support(255);

        assert!(cap3_with_hotplug.slot_capabilities.hot_plug_capable());
        assert_eq!(
            cap3_with_hotplug.slot_capabilities.physical_slot_number(),
            255
        );
    }

    #[test]
    fn test_slot_implemented_flag_in_pcie_capabilities_register() {
        // Test that slot_implemented bit is correctly set in the PCIe Capabilities register
        // when hotplug support is enabled

        // Test without hotplug - slot_implemented should be false
        let cap_no_hotplug = PciExpressCapability::new(DevicePortType::RootPort, None);
        let caps_val_no_hotplug = cap_no_hotplug.read_u32(0x00);
        let pcie_caps_no_hotplug = (caps_val_no_hotplug >> 16) as u16;
        let slot_implemented_bit = (pcie_caps_no_hotplug >> 8) & 0x1; // slot_implemented is bit 8 of PCIe capabilities
        assert_eq!(
            slot_implemented_bit, 0,
            "slot_implemented should be 0 when hotplug is not enabled"
        );

        // Test with hotplug - slot_implemented should be true
        let cap_with_hotplug = cap_no_hotplug.with_hotplug_support(1);
        let caps_val_with_hotplug = cap_with_hotplug.read_u32(0x00);
        let pcie_caps_with_hotplug = (caps_val_with_hotplug >> 16) as u16;
        let slot_implemented_bit_hotplug = (pcie_caps_with_hotplug >> 8) & 0x1; // slot_implemented is bit 8 of PCIe capabilities
        assert_eq!(
            slot_implemented_bit_hotplug, 1,
            "slot_implemented should be 1 when hotplug is enabled"
        );
    }

    #[test]
    fn test_set_presence_detect_state() {
        // Test setting presence detect state on a hotplug-capable port
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None).with_hotplug_support(1);

        // Initially, presence detect state should be 0 (no device present)
        let initial_slot_status = cap.read_u32(0x18); // Slot Control + Slot Status
        let initial_presence_detect = (initial_slot_status >> 22) & 0x1; // presence_detect_state is bit 6 of slot status (upper 16 bits)
        assert_eq!(
            initial_presence_detect, 0,
            "Initial presence detect state should be 0"
        );

        // Set device as present
        cap.set_presence_detect_state(true);
        let present_slot_status = cap.read_u32(0x18);
        let present_presence_detect = (present_slot_status >> 22) & 0x1;
        assert_eq!(
            present_presence_detect, 1,
            "Presence detect state should be 1 when device is present"
        );

        // Set device as not present
        cap.set_presence_detect_state(false);
        let absent_slot_status = cap.read_u32(0x18);
        let absent_presence_detect = (absent_slot_status >> 22) & 0x1;
        assert_eq!(
            absent_presence_detect, 0,
            "Presence detect state should be 0 when device is not present"
        );
    }

    #[test]
    fn test_set_presence_detect_state_without_slot_implemented() {
        // Test that setting presence detect state is silently ignored when slot is not implemented
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        // Should not panic and should be silently ignored
        cap.set_presence_detect_state(true);
        cap.set_presence_detect_state(false);
    }
}
