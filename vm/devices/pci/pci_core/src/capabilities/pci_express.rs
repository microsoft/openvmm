// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express Capability with Function Level Reset (FLR) support.

use super::PciCapability;
use crate::spec::caps::CapabilityId;
use crate::spec::caps::pci_express;
use crate::spec::caps::pci_express::LinkSpeed;
use crate::spec::caps::pci_express::LinkWidth;
use crate::spec::caps::pci_express::MaxEndEndTlpPrefixes;
use crate::spec::caps::pci_express::PciExpressCapabilityHeader;
use crate::spec::caps::pci_express::SupportedLinkSpeedsVector;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;

/// FLR bit is the 28th bit in the Device Capabilities register (0 indexed).
pub const PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK: u32 = 1 << 28;

/// Power Indicator Control value for "on" (PCIe Base 7.5.3.10).
const POWER_INDICATOR_ON: u16 = 0b01;
/// Power Indicator Control value for "off" (PCIe Base 7.5.3.10).
const POWER_INDICATOR_OFF: u16 = 0b11;
/// Power Indicator Control value for "blink" (PCIe Base 7.5.3.10).
const POWER_INDICATOR_BLINK: u16 = 0b10;

/// Callback interface for handling Function Level Reset (FLR) events.
pub trait FlrHandler: Send + Sync + Inspect {
    /// Called when Function Level Reset is initiated.
    fn initiate_flr(&self);
}

/// Failure to start orderly PCIe ejection.
#[derive(Debug, thiserror::Error)]
pub enum PcieEjectError {
    /// The port does not implement hotplug.
    #[error("port is not hotplug capable")]
    NotHotplugCapable,
    /// The port does not advertise attention button ejection.
    #[error("port does not support attention button ejection")]
    AttentionButtonUnsupported,
    /// No endpoint is physically present.
    #[error("slot is empty")]
    EmptySlot,
    /// Required guest hotplug interrupts are disabled.
    #[error("guest has not enabled orderly PCIe ejection")]
    InterruptDisabled,
    /// The guest is in the middle of a hot-plug operation on this slot (power indicator
    /// blinking), so a press now would cancel that operation instead of ejecting.
    #[error("guest is busy with this slot (power indicator blinking)")]
    GuestBusy,
}

/// Why the hot-plug interrupt condition is being re-evaluated.
///
/// Only selects which Inspect counter a sent interrupt is charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotplugNotifyCause {
    /// A hot-plug event changed Slot Status (a device was added or removed).
    Event,
    /// The guest wrote the port's configuration space, possibly Slot Control or Slot Status.
    GuestWrite,
}

#[derive(Debug, Inspect)]
struct PciExpressState {
    registers: PciExpressRegisters,
    presence_detect_state: bool,
    /// The hot-plug interrupt condition as last evaluated (PCIe Base 6.7.3.4): Hot-Plug
    /// Interrupt Enable set and at least one pending hot-plug event whose own enable bit is
    /// set. An MSI is due when it goes from false to true.
    hotplug_notify_level: bool,
    /// Hot-plug interrupts sent because an event raised the condition.
    hotplug_msi_on_event: u64,
    /// Hot-plug interrupts sent because a guest write raised the condition, i.e. for an event
    /// that arrived while the guest had the interrupt (or that event's enable) off.
    hotplug_msi_on_guest_write: u64,
    /// An attention button press was sent for an orderly removal that has not finished.
    ///
    /// Read together with the guest-visible slot state on a retry (see
    /// [`PciExpressCapability::request_hotplug_eject`]): while the press is still pending or
    /// the guest is counting down, a retry must not press again, because in Linux pciehp a
    /// second press while the power indicator blinks cancels the pending power-off
    /// (pciehp_handle_button_press, BLINKINGOFF_STATE).
    eject_requested: bool,
    /// Signalled when the guest has turned slot power and the power indicator off.
    ///
    /// Not saved: the waiter lives in the removal task, which never spans a save (the
    /// worker defers save and restart while a removal is pending).
    #[inspect(skip)]
    eject_completion: Option<mesh::OneshotSender<()>>,
}

#[derive(Debug, Inspect)]
struct PciExpressRegisters {
    device_control: pci_express::DeviceControl,
    link_control: pci_express::LinkControl,
    slot_control: pci_express::SlotControl,
    slot_status_events: pci_express::SlotStatus,
    root_control: pci_express::RootControl,
    device_control_2: pci_express::DeviceControl2,
    link_control_2: pci_express::LinkControl2,
}

impl PciExpressState {
    fn new() -> Self {
        Self {
            registers: PciExpressRegisters::new(),
            presence_detect_state: false,
            hotplug_notify_level: false,
            hotplug_msi_on_event: 0,
            hotplug_msi_on_guest_write: 0,
            eject_requested: false,
            eject_completion: None,
        }
    }

    fn reset_registers(&mut self) {
        let Self {
            registers,
            presence_detect_state: _,
            hotplug_notify_level,
            // Lifetime counters, kept across reset.
            hotplug_msi_on_event: _,
            hotplug_msi_on_guest_write: _,
            eject_requested,
            eject_completion,
        } = self;
        *registers = PciExpressRegisters::new();
        // The reset cleared Slot Control and Slot Status, so the condition is false.
        *hotplug_notify_level = false;
        // A reset guest no longer runs the driver that was asked to eject, so the request
        // is void. Dropping the sender fails the waiter, which then reports an error
        // instead of detaching a device the rebooted guest may already be probing again.
        *eject_requested = false;
        *eject_completion = None;
    }

    /// Forget an orderly removal request: the card is gone, so there is nothing left to
    /// eject, and a stale Attention Button Pressed would make the guest act on the next
    /// card put into this slot.
    fn clear_eject_request(&mut self) {
        self.eject_requested = false;
        self.eject_completion = None;
        self.registers
            .slot_status_events
            .set_attention_button_pressed(false);
    }

    /// Slot power and the power indicator are both off: the guest finished its side of an
    /// orderly removal and the endpoint can be detached without it noticing.
    ///
    /// Both are required: Linux writes Power Controller Control off first and the indicator
    /// off about a second later (pciehp_ctrl.c remove_board), so power alone would detach a
    /// little early.
    fn slot_powered_off(&self) -> bool {
        self.registers.slot_control.power_controller_control()
            && self.registers.slot_control.power_indicator_control() == POWER_INDICATOR_OFF
    }

    /// The hot-plug interrupt condition of PCIe Base 6.7.3.4 for the current registers.
    ///
    /// Each event status bit counts only while its own enable bit is set. Bits 0-4 of Slot
    /// Status and Slot Control pair up one to one; Data Link Layer State Changed pairs status
    /// bit 8 with control bit 12.
    fn hotplug_condition(registers: &PciExpressRegisters) -> bool {
        let ctl = registers.slot_control;
        let sta = registers.slot_status_events;
        ctl.hot_plug_interrupt_enable()
            && ((sta.attention_button_pressed() && ctl.attention_button_pressed_enable())
                || (sta.power_fault_detected() && ctl.power_fault_detected_enable())
                || (sta.mrl_sensor_changed() && ctl.mrl_sensor_changed_enable())
                || (sta.presence_detect_changed() && ctl.presence_detect_changed_enable())
                || (sta.command_completed() && ctl.command_completed_interrupt_enable())
                || (sta.data_link_layer_state_changed()
                    && ctl.data_link_layer_state_changed_enable()))
    }

    fn slot_status(
        &self,
        slot_implemented: bool,
        downstream_port: bool,
    ) -> pci_express::SlotStatus {
        self.registers
            .slot_status_events
            .with_mrl_sensor_state(0)
            .with_presence_detect_state(
                (if slot_implemented {
                    self.presence_detect_state
                } else {
                    downstream_port
                })
                .into(),
            )
            .with_electromechanical_interlock_status(0)
    }

    fn link_status(&self) -> pci_express::LinkStatus {
        pci_express::LinkStatus::new()
            .with_current_link_speed(LinkSpeed::Speed32_0GtS)
            .with_negotiated_link_width(LinkWidth::X16)
            // A powered-off slot has no link even with a card in it. This is what lets the
            // guest see the link go down when it powers a slot off (and up when it powers
            // it on again). Power Controller Control reads 0 on a port without a power
            // controller, so those ports keep following presence alone.
            .with_data_link_layer_link_active(
                self.presence_detect_state
                    && !self.registers.slot_control.power_controller_control(),
            )
    }
}

impl PciExpressRegisters {
    fn new() -> Self {
        Self {
            device_control: pci_express::DeviceControl::new()
                .with_enable_relaxed_ordering(true)
                .with_enable_no_snoop(true)
                .with_max_read_request_size(0b010),
            link_control: pci_express::LinkControl::new(),
            slot_control: pci_express::SlotControl::new(),
            slot_status_events: pci_express::SlotStatus::new(),
            root_control: pci_express::RootControl::new(),
            device_control_2: pci_express::DeviceControl2::new(),
            link_control_2: pci_express::LinkControl2::new()
                .with_target_link_speed(LinkSpeed::Speed32_0GtS),
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
        // ARI Forwarding is only meaningful on downstream-facing ports (root port /
        // switch downstream port) which turn Type 1 config requests into Type 0
        // requests to the connected device. Advertising it as supported lets an
        // ARI-aware guest enable ARI Forwarding so that endpoint functions numbered
        // greater than 7 (e.g. SR-IOV VFs) enumerate correctly instead of being
        // aliased under multiple device numbers. See PCIe Base Spec 7.0 §6.13.
        let ari_forwarding_supported = matches!(
            typ,
            pci_express::DevicePortType::RootPort
                | pci_express::DevicePortType::DownstreamSwitchPort
        );
        let function_level_reset =
            typ == pci_express::DevicePortType::Endpoint && flr_handler.is_some();
        Self {
            pcie_capabilities: pci_express::PciExpressCapabilities::new()
                .with_capability_version(2)
                .with_device_port_type(typ),
            device_capabilities: pci_express::DeviceCapabilities::new()
                .with_role_based_error(true)
                .with_function_level_reset(function_level_reset),
            // TODO: Advertising 32 GT/s is not automatically PCIe spec compliant. PCIe 7.0
            // requires additional support, including 10-bit Tag Completer and the Secondary
            // PCI Express, Data Link Feature (for downstream ports), Physical Layer 16.0 GT/s,
            // and Physical Layer 32.0 GT/s Extended Capabilities.
            link_capabilities: pci_express::LinkCapabilities::new()
                .with_max_link_speed(LinkSpeed::Speed32_0GtS)
                .with_max_link_width(LinkWidth::X16)
                .with_aspm_optionality_compliance(true),
            slot_capabilities: pci_express::SlotCapabilities::new(),
            root_capabilities: pci_express::RootCapabilities::new(),
            device_capabilities_2: pci_express::DeviceCapabilities2::new()
                .with_ari_forwarding_supported(ari_forwarding_supported),
            link_capabilities_2: pci_express::LinkCapabilities2::new()
                .with_supported_link_speeds_vector(SupportedLinkSpeedsVector::UpToGen5), // Support speeds up to PCIe Gen 5 (32.0 GT/s)
            slot_capabilities_2: pci_express::SlotCapabilities2::new(),
            state: Arc::new(Mutex::new(PciExpressState::new())),
            flr_handler,
        }
    }

    fn handle_device_control_status_write(&mut self, val: ByteEnabledDwordWrite) {
        // Device Control (2 bytes) + Device Status (2 bytes)
        let mut state = self.state.lock();
        let new_control = pci_express::DeviceControl::from_bits(
            val.merge_low(state.registers.device_control.into_bits()),
        );

        if new_control.initiate_function_level_reset()
            && self.device_capabilities.function_level_reset()
        {
            if let Some(handler) = &self.flr_handler {
                handler.initiate_flr();
            }
        }

        state.registers.device_control = pci_express::DeviceControl::from_bits(
            new_control.into_bits() & self.device_control_writable_mask(),
        );
    }

    fn device_control_writable_mask(&self) -> u16 {
        pci_express::DeviceControl::new()
            .with_correctable_error_reporting_enable(true)
            .with_non_fatal_error_reporting_enable(true)
            .with_fatal_error_reporting_enable(true)
            .with_unsupported_request_reporting_enable(true)
            .with_enable_relaxed_ordering(true)
            .with_max_payload_size(0b111)
            .with_extended_tag_enable(self.device_capabilities.ext_tag_field())
            .with_phantom_functions_enable(self.device_capabilities.phantom_functions() != 0)
            .with_enable_no_snoop(true)
            .with_max_read_request_size(0b111)
            .into_bits()
    }

    fn handle_slot_control_status_write(&mut self, val: ByteEnabledDwordWrite) {
        // Slot Control (2 bytes) + Slot Status (2 bytes)
        let mut state = self.state.lock();
        let link_was_active = state.link_status().data_link_layer_link_active();

        let new_slot_control = pci_express::SlotControl::from_bits(
            val.merge_low(state.registers.slot_control.into_bits()),
        );

        state.registers.slot_control = pci_express::SlotControl::from_bits(
            new_slot_control.into_bits() & self.slot_control_writable_mask(),
        );

        let written_status = pci_express::SlotStatus::from_bits(val.extract_high());
        state.registers.slot_status_events = pci_express::SlotStatus::from_bits(
            state.registers.slot_status_events.into_bits()
                & !(written_status.into_bits() & self.slot_status_rw1c_mask()),
        );

        // Powering a populated slot off or on changes the link, and a link change is an
        // event the guest asked to hear about through Data Link Layer State Changed. Set it
        // after the RW1C clear above so a write that both acknowledges the old event and
        // flips power keeps the new one. The port's cfg_write re-evaluates the interrupt
        // condition right after this, so an enabled event raises its MSI there.
        if link_was_active != state.link_status().data_link_layer_link_active()
            && self
                .link_capabilities
                .data_link_layer_link_active_reporting()
        {
            state
                .registers
                .slot_status_events
                .set_data_link_layer_state_changed(true);
        }

        // The guest's half of an orderly removal is this write: it powers the slot off
        // after unbinding its driver (Linux pciehp_ctrl.c remove_board). Detaching needs the
        // device unit stopped, which is async, so this only signals the waiter.
        if state.slot_powered_off() {
            state.eject_requested = false;
            if let Some(completion) = state.eject_completion.take() {
                completion.send(());
            }
        }
    }

    fn slot_control_writable_mask(&self) -> u16 {
        let slot_implemented = self.pcie_capabilities.slot_implemented();
        let hotplug_capable = slot_implemented && self.slot_capabilities.hot_plug_capable();
        pci_express::SlotControl::new()
            .with_attention_button_pressed_enable(
                slot_implemented && self.slot_capabilities.attention_button_present(),
            )
            .with_power_fault_detected_enable(
                slot_implemented && self.slot_capabilities.power_controller_present(),
            )
            .with_mrl_sensor_changed_enable(
                slot_implemented && self.slot_capabilities.mrl_sensor_present(),
            )
            .with_presence_detect_changed_enable(hotplug_capable)
            .with_command_completed_interrupt_enable(
                hotplug_capable && !self.slot_capabilities.no_command_completed_support(),
            )
            .with_hot_plug_interrupt_enable(hotplug_capable)
            .with_attention_indicator_control(
                if slot_implemented && self.slot_capabilities.attention_indicator_present() {
                    0b11
                } else {
                    0
                },
            )
            .with_power_indicator_control(
                if slot_implemented && self.slot_capabilities.power_indicator_present() {
                    0b11
                } else {
                    0
                },
            )
            .with_power_controller_control(
                slot_implemented && self.slot_capabilities.power_controller_present(),
            )
            .with_data_link_layer_state_changed_enable(
                self.link_capabilities
                    .data_link_layer_link_active_reporting(),
            )
            .with_in_band_pd_disable(
                slot_implemented && self.slot_capabilities_2.in_band_pd_disable_supported(),
            )
            .into_bits()
    }

    fn slot_status_rw1c_mask(&self) -> u16 {
        let slot_implemented = self.pcie_capabilities.slot_implemented();
        let hotplug_capable = slot_implemented && self.slot_capabilities.hot_plug_capable();
        pci_express::SlotStatus::new()
            .with_attention_button_pressed(
                slot_implemented && self.slot_capabilities.attention_button_present(),
            )
            .with_power_fault_detected(
                slot_implemented && self.slot_capabilities.power_controller_present(),
            )
            .with_mrl_sensor_changed(
                slot_implemented && self.slot_capabilities.mrl_sensor_present(),
            )
            .with_presence_detect_changed(hotplug_capable)
            .with_command_completed(
                hotplug_capable && !self.slot_capabilities.no_command_completed_support(),
            )
            .with_data_link_layer_state_changed(
                self.link_capabilities
                    .data_link_layer_link_active_reporting(),
            )
            .into_bits()
    }

    fn handle_link_control_status_write(&mut self, val: ByteEnabledDwordWrite) {
        // Link Control (2 bytes) + Link Status (2 bytes)
        let mut state = self.state.lock();

        let new_link_control = pci_express::LinkControl::from_bits(
            val.merge_low(state.registers.link_control.into_bits()),
        );

        state.registers.link_control = pci_express::LinkControl::from_bits(
            new_link_control.into_bits() & self.link_control_writable_mask(),
        );
    }

    fn link_control_writable_mask(&self) -> u16 {
        let port_type = self.pcie_capabilities.device_port_type();
        let downstream_port = Self::is_downstream_port(port_type);
        // The PCIe spec requires Link Disable to be writable on downstream ports.
        // Keep it read-only because this emulator intentionally does not disable
        // downstream transaction forwarding when the bit is set.
        pci_express::LinkControl::new()
            .with_aspm_control(0b11)
            .with_read_completion_boundary(matches!(
                port_type,
                pci_express::DevicePortType::Endpoint
            ) as u16)
            .with_common_clock_configuration(true)
            .with_extended_synch(true)
            .with_enable_clock_power_management(
                matches!(
                    port_type,
                    pci_express::DevicePortType::Endpoint
                        | pci_express::DevicePortType::UpstreamSwitchPort
                ) && self.link_capabilities.clock_power_management(),
            )
            .with_link_bandwidth_management_interrupt_enable(
                downstream_port
                    && self
                        .link_capabilities
                        .link_bandwidth_notification_capability(),
            )
            .with_link_autonomous_bandwidth_interrupt_enable(
                downstream_port
                    && self
                        .link_capabilities
                        .link_bandwidth_notification_capability(),
            )
            .with_drs_signaling_control(
                if downstream_port && self.link_capabilities_2.drs_supported() {
                    0b11
                } else {
                    0
                },
            )
            .into_bits()
    }

    fn handle_link_control_2_write(&mut self, val: ByteEnabledDwordWrite) {
        // Link Control 2 (2 bytes) + Link Status 2 (2 bytes)
        let mut state = self.state.lock();

        let new_link_control_2 = pci_express::LinkControl2::from_bits(
            val.merge_low(state.registers.link_control_2.into_bits()),
        );

        state.registers.link_control_2 = pci_express::LinkControl2::from_bits(
            new_link_control_2.into_bits() & Self::link_control_2_writable_mask(),
        );
    }

    fn link_control_2_writable_mask() -> u16 {
        pci_express::LinkControl2::new()
            .with_target_link_speed(LinkSpeed::from_bits(0b1111))
            .with_enter_compliance(true)
            .with_hardware_autonomous_speed_disable(true)
            .with_transmit_margin(0b111)
            .with_enter_modified_compliance(true)
            .with_compliance_sos(true)
            .with_compliance_preset_de_emphasis(0b1111)
            .into_bits()
    }

    fn root_control_writable_mask(&self) -> u16 {
        let root_port =
            self.pcie_capabilities.device_port_type() == pci_express::DevicePortType::RootPort;
        pci_express::RootControl::new()
            .with_system_error_on_correctable_error_enable(root_port)
            .with_system_error_on_non_fatal_error_enable(root_port)
            .with_system_error_on_fatal_error_enable(root_port)
            .with_pme_interrupt_enable(root_port)
            .with_crs_software_visibility_enable(
                root_port && self.root_capabilities.crs_software_visibility(),
            )
            .into_bits()
    }

    fn device_control_2_writable_mask(&self) -> u16 {
        pci_express::DeviceControl2::new()
            .with_ari_forwarding_enable(self.device_capabilities_2.ari_forwarding_supported())
            .into_bits()
    }

    fn is_downstream_port(port_type: pci_express::DevicePortType) -> bool {
        matches!(
            port_type,
            pci_express::DevicePortType::RootPort
                | pci_express::DevicePortType::DownstreamSwitchPort
        )
    }

    /// Enable hotplug support for this PCIe capability.
    /// This configures the appropriate registers to support hotpluggable devices.
    /// Panics if called on device types other than RootPort or DownstreamSwitchPort.
    ///
    /// # Arguments
    /// * `slot_number` - The physical slot number to assign to this hotplug-capable port
    pub fn with_hotplug_support(mut self, slot_number: u32) -> Self {
        let port_type = self.pcie_capabilities.device_port_type();
        assert!(
            Self::is_downstream_port(port_type),
            "Hotplug support is not valid for device port type {port_type:?}. \
             Only RootPort and DownstreamSwitchPort support hotplug."
        );

        // Enable slot implemented in PCIe capabilities when hotplug is enabled
        self.pcie_capabilities = self.pcie_capabilities.with_slot_implemented(true);

        // Enable hotplug capabilities in slot capabilities register.
        //
        // We advertise no_command_completed_support because our emulation
        // applies Slot Control changes instantly (no hardware delay). This
        // tells the guest's pciehp driver to skip waiting for command_completed
        // after writing Slot Control (PCIe spec §7.5.3.9).
        //
        // Without this, a naive command_completed implementation that sets
        // the bit on every Slot Control write creates an interrupt storm:
        // the guest clears command_completed via RW1C (which is itself a
        // Slot Control write), re-triggering command_completed in a loop.
        // A correct implementation for ports with real delay would need to
        // diff old vs new Slot Control values and only signal completion
        // when control bits actually change, not on RW1C status clears.
        self.slot_capabilities = self
            .slot_capabilities
            .with_hot_plug_surprise(true)
            .with_hot_plug_capable(true)
            .with_no_command_completed_support(true)
            .with_physical_slot_number(slot_number);

        // Enable Data Link Layer Link Active Reporting when hotplug is enabled
        self.link_capabilities = self
            .link_capabilities
            .with_data_link_layer_link_active_reporting(true);

        self
    }

    /// Enables attention button, power controller and power indicator support.
    ///
    /// This is the PCIe Base 6.7 "standard usage model" for orderly removal: the host
    /// presses the button, the guest stops its driver and powers the slot off, and only
    /// then is the card pulled. Opt-in, because a guest honouring the model waits five
    /// seconds after a press before acting (Linux pciehp_ctrl.c pciehp_handle_button_press),
    /// on hot-add as well as on removal. Ports without it keep surprise semantics.
    ///
    /// Hot-Plug Surprise stays advertised: the host can still pull a card without asking
    /// (a caller's fallback, or a guest that never answers), and the guest must keep
    /// tolerating that.
    ///
    /// # Panics
    ///
    /// Panics if hotplug support has not been configured.
    pub fn with_attention_button(mut self) -> Self {
        assert!(self.slot_capabilities.hot_plug_capable());
        self.slot_capabilities = self
            .slot_capabilities
            .with_attention_button_present(true)
            .with_power_controller_present(true)
            .with_power_indicator_present(true);
        // Start in the state reset() produces: an empty slot is off.
        let mut state = self.state.lock();
        self.apply_reset_power_state(&mut state);
        drop(state);
        self
    }

    /// Enable TLP prefixing support for this PCIe capability.
    ///
    /// This configures the appropriate registers to indicate that the function supports
    /// end-to-end TLP prefixes. We do not currently implement TLP prefixing in endpoint
    /// DMA APIs but this still may be required for ports upstream of passthrough devices
    /// that support TLP prefixing (ex. for guest controlled PASID with a virtual IOMMU).
    pub fn with_tlp_prefixing_supported(mut self, max_prefixes: MaxEndEndTlpPrefixes) -> Self {
        self.device_capabilities_2 = self
            .device_capabilities_2
            .with_extended_fmt_field_supported(true)
            .with_end_end_tlp_prefix_supported(true)
            .with_max_end_end_tlp_prefixes(max_prefixes);
        self
    }

    /// Set the physical presence state used to derive slot and link status.
    ///
    /// # Arguments
    /// * `present` - true if a device is present in the slot, false if the slot is empty
    pub fn set_presence_detect_state(&self, present: bool) {
        let mut state = self.state.lock();
        state.presence_detect_state = present;
        if !present {
            state.clear_eject_request();
        }
        // A card attached at build time is powered from the start, as reset() would leave
        // it. Without this a static card on an attention-button port would sit in a slot
        // that reports power off and link down until the first reset, and a guest with
        // native hotplug would treat it as needing a power-on (a five-second button wait).
        self.apply_reset_power_state(&mut state);
    }

    /// Power Controller and Power Indicator as they come out of reset: following presence,
    /// and only where the capability is advertised. See [`PciCapability::reset`].
    fn apply_reset_power_state(&self, state: &mut PciExpressState) {
        let present = state.presence_detect_state;
        if self.slot_capabilities.power_controller_present() {
            state
                .registers
                .slot_control
                .set_power_controller_control(!present);
        }
        if self.slot_capabilities.power_indicator_present() {
            state
                .registers
                .slot_control
                .set_power_indicator_control(if present {
                    POWER_INDICATOR_ON
                } else {
                    POWER_INDICATOR_OFF
                });
        }
    }

    /// Set the RW1C changed bits in Slot Status to signal a hotplug event.
    /// Call this only for runtime hotplug events, not build-time device attachment.
    pub fn set_hotplug_changed_bits(&self) {
        if !self.pcie_capabilities.slot_implemented() || !self.slot_capabilities.hot_plug_capable()
        {
            return;
        }

        let mut state = self.state.lock();
        state
            .registers
            .slot_status_events
            .set_presence_detect_changed(true);
        state
            .registers
            .slot_status_events
            .set_data_link_layer_state_changed(true);
    }

    /// Atomically update presence detect state, link active state, and
    /// changed bits for a hotplug event.
    pub fn set_hotplug_state(&self, present: bool) {
        if !self.pcie_capabilities.slot_implemented() || !self.slot_capabilities.hot_plug_capable()
        {
            return;
        }

        let mut state = self.state.lock();
        if state.presence_detect_state == present {
            return;
        }
        let link_was_active = state.link_status().data_link_layer_link_active();
        state.presence_detect_state = present;
        if !present {
            state.clear_eject_request();
        }

        // A card inserted into a powered-off slot on an attention-button port also presses
        // the button. Linux pciehp enables Attention Button Pressed instead of Presence
        // Detect Changed when the port has a button (pciehp_hpc.c pcie_enable_notification),
        // and the link cannot come up in an unpowered slot, so without the press the guest
        // hears nothing.
        if present
            && self.slot_capabilities.attention_button_present()
            && state.registers.slot_control.power_controller_control()
        {
            state
                .registers
                .slot_status_events
                .set_attention_button_pressed(true);
        }

        state
            .registers
            .slot_status_events
            .set_presence_detect_changed(true);
        // Only a real link change is reported: inserting into a powered-off slot leaves
        // the link down, and the guest raises it itself when it powers the slot on.
        if link_was_active != state.link_status().data_link_layer_link_active() {
            state
                .registers
                .slot_status_events
                .set_data_link_layer_state_changed(true);
        }
    }

    /// Requests orderly ejection without changing physical presence.
    ///
    /// Whether to press the attention button is decided from what the guest shows, not
    /// from whether a press was sent before:
    ///
    /// * An earlier press still unacknowledged (Attention Button Pressed set) or a guest
    ///   counting down (power indicator blinking): only the waiter is replaced. A second
    ///   press within the guest's five-second window cancels the removal (Linux pciehp_ctrl.c
    ///   pciehp_handle_button_press, BLINKINGOFF_STATE).
    /// * An earlier press acknowledged with the indicator not blinking: the guest dropped it
    ///   (pciehp ignores a press in POWERON_STATE and POWEROFF_STATE, acknowledging it all the
    ///   same), so the button is pressed again. Trusting the old press instead would leave
    ///   every retry waiting on a removal that never started, until a VM reset.
    /// * No earlier press and the indicator blinking: the guest is busy with this slot (a
    ///   hot-add countdown or power-on), so the request fails with
    ///   [`PcieEjectError::GuestBusy`]. A press then would cancel the guest's operation.
    /// * Otherwise the button is pressed.
    ///
    /// The one window this cannot see is a guest between acknowledging a press and setting
    /// the indicator to blink (pciehp's interrupt handler, then its thread). A retry lands
    /// only after the caller's eject timeout, long after that window.
    ///
    /// The caller re-evaluates the hot-plug interrupt condition afterwards to send the MSI.
    ///
    /// # Returns
    ///
    /// A receiver signalled when slot power and the power indicator are off. It is ready
    /// at once if both already are. Reset or physical removal fails a pending receiver.
    ///
    /// # Errors
    ///
    /// [`PcieEjectError`] if hotplug or attention button support is missing, the slot is
    /// empty, the guest is busy with the slot, or a new press is needed while the guest has
    /// Attention Button Pressed Enable off. A press the guest cannot hear would only run out
    /// the caller's timeout.
    pub fn request_hotplug_eject(&self) -> Result<mesh::OneshotReceiver<()>, PcieEjectError> {
        if !self.slot_capabilities.hot_plug_capable() {
            return Err(PcieEjectError::NotHotplugCapable);
        }
        if !self.slot_capabilities.attention_button_present() {
            return Err(PcieEjectError::AttentionButtonUnsupported);
        }
        let mut state = self.state.lock();
        if !state.presence_detect_state {
            return Err(PcieEjectError::EmptySlot);
        }
        let (send, recv) = mesh::oneshot();
        if state.slot_powered_off() {
            send.send(());
            return Ok(recv);
        }
        let control = state.registers.slot_control;
        let blinking = control.power_indicator_control() == POWER_INDICATOR_BLINK;
        let press_unread = state
            .registers
            .slot_status_events
            .attention_button_pressed();
        // The guest has the earlier press or is acting on it: wait again, press nothing.
        let rearm_only = state.eject_requested && (press_unread || blinking);
        if !rearm_only {
            if blinking {
                return Err(PcieEjectError::GuestBusy);
            }
            // Attention Button Pressed Enable alone decides whether the guest listens.
            // Hot-Plug Interrupt Enable only selects interrupt over polling: pciehp in poll
            // mode leaves it off and still reads the press (pciehp_hpc.c
            // pcie_enable_notification). Without an interrupt the MSI is simply not sent.
            if !control.attention_button_pressed_enable() {
                return Err(PcieEjectError::InterruptDisabled);
            }
            state
                .registers
                .slot_status_events
                .set_attention_button_pressed(true);
            state.eject_requested = true;
        }
        state.eject_completion = Some(send);
        Ok(recv)
    }

    /// Whether slot power and the power indicator are both off.
    ///
    /// The detach path re-checks this under the same lock that disconnects the endpoint:
    /// the guest may have powered the slot back on between the completion signal and the
    /// detach (a cancelled removal, or a reboot that re-enabled the slot).
    pub fn hotplug_eject_complete(&self) -> bool {
        self.state.lock().slot_powered_off()
    }

    /// Whether an orderly removal request is pending (button pressed, slot not yet off).
    pub fn hotplug_eject_requested(&self) -> bool {
        self.state.lock().eject_requested
    }

    /// Returns whether the hot plug interrupt is enabled in Slot Control.
    pub fn hot_plug_interrupt_enabled(&self) -> bool {
        self.state
            .lock()
            .registers
            .slot_control
            .hot_plug_interrupt_enable()
    }

    /// Re-evaluate the hot-plug interrupt condition and report whether a hot-plug MSI is due.
    ///
    /// Returns true only when the condition goes from false to true (PCIe Base 6.7.3.4), so an
    /// event that arrives while an earlier one is still pending raises no second interrupt.
    /// The caller sends the MSI.
    ///
    /// Call this after every hot-plug event and after every guest configuration write: an
    /// event that arrived while the guest had the interrupt off is due as soon as the guest
    /// enables it. Writes that do not touch the slot registers leave the condition unchanged.
    pub fn update_hotplug_notify(&self, cause: HotplugNotifyCause) -> bool {
        let mut state = self.state.lock();
        let now = PciExpressState::hotplug_condition(&state.registers);
        let rose = now && !state.hotplug_notify_level;
        state.hotplug_notify_level = now;
        if rose {
            match cause {
                HotplugNotifyCause::Event => state.hotplug_msi_on_event += 1,
                HotplugNotifyCause::GuestWrite => state.hotplug_msi_on_guest_write += 1,
            }
        }
        rose
    }

    /// Returns whether ARI Forwarding is enabled in Device Control 2.
    ///
    /// When set on a downstream-facing port, the port no longer enforces the
    /// requirement that the Device Number be 0 when turning a Type 1
    /// configuration request into a Type 0 request, allowing the full 8-bit
    /// ARI function number to reach the connected device. See PCIe Base Spec
    /// 7.0 §6.13.
    pub fn ari_forwarding_enable(&self) -> bool {
        self.state
            .lock()
            .registers
            .device_control_2
            .ari_forwarding_enable()
    }

    /// Returns a reference to the slot capabilities register.
    pub fn slot_capabilities(&self) -> &pci_express::SlotCapabilities {
        &self.slot_capabilities
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

    fn read(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        let state = self.state.lock();
        let label = self.label();
        match PciExpressCapabilityHeader(offset) {
            PciExpressCapabilityHeader::PCIE_CAPS => {
                // PCIe Capabilities Register (16 bits) + Next Pointer (8 bits) + Capability ID (8 bits)
                value.set_low_high(
                    CapabilityId::PCI_EXPRESS.0.into(),
                    self.pcie_capabilities.into_bits(),
                )
            }
            PciExpressCapabilityHeader::DEVICE_CAPS => {
                value.set(self.device_capabilities.into_bits())
            }
            PciExpressCapabilityHeader::DEVICE_CTL_STS => {
                // Device Control (2 bytes) + Device Status (2 bytes)
                value.set_low_high(state.registers.device_control.into_bits(), 0);
            }
            PciExpressCapabilityHeader::LINK_CAPS => value.set(self.link_capabilities.into_bits()),
            PciExpressCapabilityHeader::LINK_CTL_STS => {
                // Link Control (2 bytes) + Link Status (2 bytes)
                value.set_low_high(
                    state.registers.link_control.into_bits(),
                    state.link_status().into_bits(),
                );
            }
            PciExpressCapabilityHeader::SLOT_CAPS => value.set(self.slot_capabilities.into_bits()),
            PciExpressCapabilityHeader::SLOT_CTL_STS => {
                // Slot Control (2 bytes) + Slot Status (2 bytes)
                value.set_low_high(
                    state.registers.slot_control.into_bits(),
                    state
                        .slot_status(
                            self.pcie_capabilities.slot_implemented(),
                            Self::is_downstream_port(self.pcie_capabilities.device_port_type()),
                        )
                        .into_bits(),
                );
            }
            PciExpressCapabilityHeader::ROOT_CTL_CAPS => {
                // Root Control (2 bytes) + Root Capabilities (2 bytes)
                value.set_low_high(
                    state.registers.root_control.into_bits(),
                    self.root_capabilities.into_bits(),
                );
            }
            PciExpressCapabilityHeader::ROOT_STS => {
                value.set(0);
            }
            PciExpressCapabilityHeader::DEVICE_CAPS_2 => {
                value.set(self.device_capabilities_2.into_bits())
            }
            PciExpressCapabilityHeader::DEVICE_CTL_STS_2 => {
                // Device Control 2 (2 bytes) + Device Status 2 (2 bytes)
                value.set_low_high(state.registers.device_control_2.into_bits(), 0);
            }
            PciExpressCapabilityHeader::LINK_CAPS_2 => {
                value.set(self.link_capabilities_2.into_bits())
            }
            PciExpressCapabilityHeader::LINK_CTL_STS_2 => {
                // Link Control 2 (2 bytes) + Link Status 2 (2 bytes)
                value.set_low_high(state.registers.link_control_2.into_bits(), 0);
            }
            PciExpressCapabilityHeader::SLOT_CAPS_2 => {
                value.set(self.slot_capabilities_2.into_bits())
            }
            PciExpressCapabilityHeader::SLOT_CTL_STS_2 => {
                value.set(0);
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    "unhandled pci express capability read"
                );
                value.set(0);
            }
        }
    }

    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite) {
        let label = self.label();
        match PciExpressCapabilityHeader(offset) {
            PciExpressCapabilityHeader::PCIE_CAPS => {
                // PCIe Capabilities register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    ?val,
                    "write to read-only pcie capabilities"
                );
            }
            PciExpressCapabilityHeader::DEVICE_CAPS => {
                // Device Capabilities register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    ?val,
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
                    ?val,
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
                    ?val,
                    "write to read-only slot capabilities"
                );
            }
            PciExpressCapabilityHeader::SLOT_CTL_STS => {
                self.handle_slot_control_status_write(val);
            }
            PciExpressCapabilityHeader::ROOT_CTL_CAPS => {
                // Root Control (2 bytes) + Root Capabilities (2 bytes)
                let mut state = self.state.lock();
                let new_control = pci_express::RootControl::from_bits(
                    val.merge_low(state.registers.root_control.into_bits()),
                );
                state.registers.root_control = pci_express::RootControl::from_bits(
                    new_control.into_bits() & self.root_control_writable_mask(),
                );
                // Root Capabilities upper 16 bits are read-only
            }
            PciExpressCapabilityHeader::ROOT_STS => {
                // Root Status is not modeled and remains hardwired to zero.
            }
            PciExpressCapabilityHeader::DEVICE_CAPS_2 => {
                // Device Capabilities 2 register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    ?val,
                    "write to read-only device capabilities 2"
                );
            }
            PciExpressCapabilityHeader::DEVICE_CTL_STS_2 => {
                // Device Control 2 (2 bytes) + Device Status 2 (2 bytes)
                let mut state = self.state.lock();
                let new_control = pci_express::DeviceControl2::from_bits(
                    val.merge_low(state.registers.device_control_2.into_bits()),
                );
                state.registers.device_control_2 = pci_express::DeviceControl2::from_bits(
                    new_control.into_bits() & self.device_control_2_writable_mask(),
                );
            }
            PciExpressCapabilityHeader::LINK_CAPS_2 => {
                // Link Capabilities 2 register is read-only
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    ?val,
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
                    ?val,
                    "write to read-only slot capabilities 2"
                );
            }
            PciExpressCapabilityHeader::SLOT_CTL_STS_2 => {
                // Slot Control 2 and Slot Status 2 are reserved and hardwired to zero.
            }
            _ => {
                tracelimit::warn_ratelimited!(
                    ?label,
                    offset,
                    ?val,
                    "unhandled pci express capability write"
                );
            }
        }
    }

    fn reset(&mut self) {
        let mut state = self.state.lock();
        state.reset_registers();
        // PCIe Base 7.5.3.10 leaves the reset value of Power Controller Control to the
        // implementation. Follow presence: a populated slot comes out of reset powered so
        // firmware and the booting guest can use the card, an empty one comes out off so a
        // later insertion waits for the guest to power it. Each control is touched only when
        // its capability is advertised; a port without a power controller must keep reading
        // 0 there, or its link would report down.
        self.apply_reset_power_state(&mut state);
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
            pub link_control: u16,
            #[mesh(3)]
            pub slot_control: u16,
            #[mesh(4)]
            pub slot_status_events: u16,
            #[mesh(5)]
            pub root_control: u16,
            #[mesh(6)]
            pub device_control_2: u16,
            #[mesh(7)]
            pub link_control_2: u16,
            /// An attention button press is outstanding. Saved so a restored VM does not
            /// press again on a retry, which would cancel the guest's pending removal.
            ///
            /// Optional so a blob saved before the field existed restores: it reads as
            /// `None`, which lands on the cold default (no request), the state that blob's
            /// VM had, since it could not request an orderly removal at all.
            #[mesh(8)]
            pub eject_requested: Option<bool>,
        }
    }

    impl SaveRestore for PciExpressCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let state = self.state.lock();
            let PciExpressState {
                registers,
                presence_detect_state: _,
                // Not saved: restore recomputes the level from the saved registers.
                hotplug_notify_level: _,
                hotplug_msi_on_event: _,
                hotplug_msi_on_guest_write: _,
                eject_requested,
                // The waiter belongs to a removal task, which a save never spans.
                eject_completion: _,
            } = &*state;
            let PciExpressRegisters {
                device_control,
                link_control,
                slot_control,
                slot_status_events,
                root_control,
                device_control_2,
                link_control_2,
            } = registers;
            Ok(state::SavedState {
                device_control: device_control.into_bits(),
                link_control: link_control.into_bits(),
                slot_control: slot_control.into_bits(),
                slot_status_events: slot_status_events.into_bits(),
                root_control: root_control.into_bits(),
                device_control_2: device_control_2.into_bits(),
                link_control_2: link_control_2.into_bits(),
                eject_requested: Some(*eject_requested),
            })
        }

        fn restore(&mut self, saved: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                device_control,
                link_control,
                slot_control,
                slot_status_events,
                root_control,
                device_control_2,
                link_control_2,
                eject_requested: saved_eject_requested,
            } = saved;
            let mut state = self.state.lock();
            let PciExpressState {
                registers,
                presence_detect_state,
                hotplug_notify_level,
                hotplug_msi_on_event: _,
                hotplug_msi_on_guest_write: _,
                eject_requested,
                eject_completion,
            } = &mut *state;
            // Presence is not saved: it comes from the topology the restored VM was built
            // with, which holds only statically attached devices. A request for a card that
            // is not there after restore is dropped together with its button event.
            *eject_completion = None;
            *eject_requested = saved_eject_requested.unwrap_or(false)
                && *presence_detect_state
                && self.slot_capabilities.attention_button_present();
            let mut slot_status_events = pci_express::SlotStatus::from_bits(
                slot_status_events & self.slot_status_rw1c_mask(),
            );
            if !*presence_detect_state {
                slot_status_events.set_attention_button_pressed(false);
            }
            *registers = PciExpressRegisters {
                device_control: pci_express::DeviceControl::from_bits(
                    device_control & self.device_control_writable_mask(),
                ),
                link_control: pci_express::LinkControl::from_bits(
                    link_control & self.link_control_writable_mask(),
                ),
                slot_control: pci_express::SlotControl::from_bits(
                    slot_control & self.slot_control_writable_mask(),
                ),
                slot_status_events,
                root_control: pci_express::RootControl::from_bits(
                    root_control & self.root_control_writable_mask(),
                ),
                device_control_2: pci_express::DeviceControl2::from_bits(
                    device_control_2 & self.device_control_2_writable_mask(),
                ),
                link_control_2: pci_express::LinkControl2::from_bits(
                    link_control_2 & Self::link_control_2_writable_mask(),
                ),
            };
            // Take the restored level as already notified: an interrupt raised before the save
            // reached the guest then, so sending it again would be a spurious second one.
            *hotplug_notify_level = PciExpressState::hotplug_condition(registers);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::caps::pci_express::DevicePortType;
    use crate::test_helpers::read_cap_u32;
    use crate::test_helpers::write_cap_u32;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigByteEnable;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

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
    fn test_ari_forwarding_supported_by_port_type() {
        // ARI Forwarding Supported (Device Capabilities 2, bit 5 / 0x20) must be
        // advertised on downstream-facing ports and never on endpoints/upstream ports.
        for (typ, expected) in [
            (DevicePortType::RootPort, true),
            (DevicePortType::DownstreamSwitchPort, true),
            (DevicePortType::UpstreamSwitchPort, false),
            (DevicePortType::Endpoint, false),
        ] {
            let name = format!("{typ:?}");
            let cap = PciExpressCapability::new(typ, None);
            let device_caps_2 = read_cap_u32(&cap, 0x24);
            let ari_supported = device_caps_2 & 0x20 != 0;
            assert_eq!(ari_supported, expected, "unexpected ARI support for {name}");
        }
    }

    #[test]
    fn test_ari_forwarding_enable_is_guest_writable() {
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        // Enable bit starts cleared.
        assert!(!cap.ari_forwarding_enable());

        // Guest sets ARI Forwarding Enable (Device Control 2, bit 5 / 0x20).
        write_cap_u32(&mut cap, 0x28, 0x0020);
        assert!(cap.ari_forwarding_enable());
        assert_eq!(read_cap_u32(&cap, 0x28) & 0x0020, 0x0020);

        // Guest clears it again.
        write_cap_u32(&mut cap, 0x28, 0x0000);
        assert!(!cap.ari_forwarding_enable());
    }

    #[test]
    fn test_tlp_prefixing_supported_max_prefixes() {
        for max_prefixes in [
            MaxEndEndTlpPrefixes::One,
            MaxEndEndTlpPrefixes::Two,
            MaxEndEndTlpPrefixes::Three,
            MaxEndEndTlpPrefixes::Four,
        ] {
            let cap = PciExpressCapability::new(DevicePortType::Endpoint, None)
                .with_tlp_prefixing_supported(max_prefixes);
            let device_caps_2 =
                pci_express::DeviceCapabilities2::from_bits(read_cap_u32(&cap, 0x24));

            assert!(device_caps_2.extended_fmt_field_supported());
            assert!(device_caps_2.end_end_tlp_prefix_supported());
            assert_eq!(
                device_caps_2.max_end_end_tlp_prefixes().into_bits(),
                max_prefixes.into_bits(),
                "unexpected max TLP prefix encoding for {max_prefixes:?}"
            );
        }
    }

    #[test]
    fn test_pci_express_capability_read_endpoint() {
        let flr_handler = TestFlrHandler::new();
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, Some(flr_handler));

        // Test PCIe Capabilities Register (offset 0x00)
        let caps_val = read_cap_u32(&cap, 0x00);
        assert_eq!(caps_val & 0xFF, 0x10); // Capability ID = 0x10
        assert_eq!((caps_val >> 8) & 0xFF, 0x00); // Next Pointer = 0x00
        assert_eq!((caps_val >> 16) & 0xFFFF, 0x0002); // PCIe Caps: Version 2, Device/Port Type 0

        // Test Device Capabilities Register (offset 0x04)
        let device_caps_val = read_cap_u32(&cap, 0x04);
        assert_eq!(
            device_caps_val & PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK,
            PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK
        ); // FLR bit should be set

        // Test the specification-defined Device Control reset value.
        let device_ctl_sts_val = read_cap_u32(&cap, 0x08);
        assert_eq!(device_ctl_sts_val, 0x2810);

        // Test Link Control/Status Register (offset 0x10) - should have link status initialized
        let link_ctl_sts_val = read_cap_u32(&cap, 0x10);
        let expected_link_status = (LinkSpeed::Speed32_0GtS.into_bits() as u16)
            | ((LinkWidth::X16.into_bits() as u16) << 4); // current_link_speed + negotiated_link_width
        assert_eq!(link_ctl_sts_val, (expected_link_status as u32) << 16); // Link status is in upper 16 bits
    }

    #[test]
    fn test_pci_express_capability_read_root_port() {
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        // Test PCIe Capabilities Register (offset 0x00)
        let caps_val = read_cap_u32(&cap, 0x00);
        assert_eq!(caps_val & 0xFF, 0x10); // Capability ID = 0x10
        assert_eq!((caps_val >> 8) & 0xFF, 0x00); // Next Pointer = 0x00
        assert_eq!((caps_val >> 16) & 0xFFFF, 0x0042); // PCIe Caps: Version 2, Device/Port Type 4
    }

    #[test]
    fn test_pcie_open_enums_preserve_reserved_values() {
        assert_eq!(LinkSpeed::from_bits(0).into_bits(), 0);
        assert_eq!(LinkWidth::from_bits(0b11_1111).into_bits(), 0b11_1111);
        assert_eq!(
            SupportedLinkSpeedsVector::from_bits(0b101_0101).into_bits(),
            0b101_0101
        );

        let unknown_port_type = DevicePortType(0b1111);
        let capabilities =
            pci_express::PciExpressCapabilities::new().with_device_port_type(unknown_port_type);
        assert_eq!(capabilities.device_port_type(), unknown_port_type);
    }

    #[test]
    fn test_pci_express_capability_read_no_flr() {
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Test Device Capabilities Register (offset 0x04) - FLR should not be set
        let device_caps_val = read_cap_u32(&cap, 0x04);
        assert_eq!(device_caps_val & PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK, 0);
    }

    #[test]
    fn test_flr_is_only_advertised_by_endpoints() {
        let flr_handler = TestFlrHandler::new();
        let mut cap =
            PciExpressCapability::new(DevicePortType::RootPort, Some(flr_handler.clone()));

        assert_eq!(
            read_cap_u32(&cap, 0x04) & PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK,
            0
        );
        write_cap_u32(&mut cap, 0x08, 0x8000);
        assert!(!flr_handler.was_flr_initiated());
    }

    #[test]
    fn test_pci_express_capability_write_readonly_registers() {
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        // Try to write to read-only PCIe Capabilities Register (offset 0x00)
        let original_caps = read_cap_u32(&cap, 0x00);
        write_cap_u32(&mut cap, 0x00, 0xFFFFFFFF);
        assert_eq!(read_cap_u32(&cap, 0x00), original_caps); // Should be unchanged

        // Try to write to read-only Device Capabilities Register (offset 0x04)
        let original_device_caps = read_cap_u32(&cap, 0x04);
        write_cap_u32(&mut cap, 0x04, 0xFFFFFFFF);
        assert_eq!(read_cap_u32(&cap, 0x04), original_device_caps); // Should be unchanged
    }

    #[test]
    fn test_pci_express_capability_write_device_control() {
        let flr_handler = TestFlrHandler::new();
        let mut cap =
            PciExpressCapability::new(DevicePortType::Endpoint, Some(flr_handler.clone()));

        // Initial state should have FLR clear and the defined control defaults.
        let initial_ctl_sts = read_cap_u32(&cap, 0x08);
        assert_eq!(initial_ctl_sts & 0xFFFF, 0x2810);

        // Test writing to Device Control Register (lower 16 bits of offset 0x08)
        // Set some control bits but not FLR initially
        write_cap_u32(&mut cap, 0x08, 0x0001); // Enable correctable error reporting (bit 0)
        let device_ctl_sts = read_cap_u32(&cap, 0x08);
        assert_eq!(device_ctl_sts & 0xFFFF, 0x0001); // Device Control should be set
        assert!(!flr_handler.was_flr_initiated()); // FLR should not be triggered

        // Test FLR initiation (bit 15 of Device Control)
        flr_handler.reset();
        write_cap_u32(&mut cap, 0x08, 0x8001); // Set FLR bit (bit 15) and other control bits
        let device_ctl_sts_after_flr = read_cap_u32(&cap, 0x08);
        assert_eq!(device_ctl_sts_after_flr & 0xFFFF, 0x0001); // FLR bit should be cleared, others remain
        assert!(flr_handler.was_flr_initiated()); // FLR should be triggered

        // Test that writing FLR bit when it's already been triggered behaves correctly
        flr_handler.reset();
        // After the previous FLR, device_control should have bit 0 set but FLR clear
        // So writing 0x8000 (only FLR bit) should trigger FLR again
        write_cap_u32(&mut cap, 0x08, 0x8000); // Set FLR bit only
        let device_ctl_sts_final = read_cap_u32(&cap, 0x08);
        assert_eq!(device_ctl_sts_final & 0xFFFF, 0x0000); // All bits should be cleared (FLR self-clears, bit 0 was overwritten)
        assert!(flr_handler.was_flr_initiated()); // Should trigger because FLR transitioned from 0 to 1
    }

    #[test]
    fn test_unimplemented_status_registers_ignore_writes() {
        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        write_cap_u32(&mut cap, 0x08, 0xffff_0000);
        write_cap_u32(&mut cap, 0x20, 0xffff_ffff);
        write_cap_u32(&mut cap, 0x28, 0xffff_0000);
        write_cap_u32(&mut cap, 0x30, 0xffff_0000);
        write_cap_u32(&mut cap, 0x38, 0xffff_ffff);

        assert_eq!(read_cap_u32(&cap, 0x08) >> 16, 0);
        assert_eq!(read_cap_u32(&cap, 0x20), 0);
        assert_eq!(read_cap_u32(&cap, 0x28) >> 16, 0);
        assert_eq!(read_cap_u32(&cap, 0x30) >> 16, 0);
        assert_eq!(read_cap_u32(&cap, 0x38), 0);
    }

    #[test]
    fn test_unsupported_control_fields_ignore_all_ones_write() {
        let mut root_port = PciExpressCapability::new(DevicePortType::RootPort, None);

        write_cap_u32(&mut root_port, 0x08, u32::MAX);
        write_cap_u32(&mut root_port, 0x10, u32::MAX);
        write_cap_u32(&mut root_port, 0x1c, u32::MAX);
        write_cap_u32(&mut root_port, 0x28, u32::MAX);
        write_cap_u32(&mut root_port, 0x30, u32::MAX);

        assert_eq!(read_cap_u32(&root_port, 0x08), 0x0000_78ff);
        assert_eq!(read_cap_u32(&root_port, 0x10) & 0xffff, 0x00c3);
        assert_eq!(read_cap_u32(&root_port, 0x1c), 0x0000_000f);
        assert_eq!(read_cap_u32(&root_port, 0x28), 0x0000_0020);
        assert_eq!(read_cap_u32(&root_port, 0x30), 0x0000_ffbf);

        let mut endpoint = PciExpressCapability::new(DevicePortType::Endpoint, None);
        write_cap_u32(&mut endpoint, 0x10, u32::MAX);
        write_cap_u32(&mut endpoint, 0x1c, u32::MAX);
        write_cap_u32(&mut endpoint, 0x28, u32::MAX);
        assert_eq!(read_cap_u32(&endpoint, 0x10) & 0xffff, 0x00cb);
        assert_eq!(read_cap_u32(&endpoint, 0x1c), 0);
        assert_eq!(read_cap_u32(&endpoint, 0x28), 0);
    }

    #[test]
    fn test_pci_express_capability_byte_write_control() {
        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        cap.write(
            0x08,
            ByteEnabledDwordWrite::new(
                0x0000_0001,
                PciConfigByteEnable::from_offset_len(0x08, 1).unwrap(),
            ),
        );

        let device_ctl_sts = read_cap_u32(&cap, 0x08);
        assert_eq!(device_ctl_sts & 0xffff, 0x2801);
        assert_eq!(device_ctl_sts & 0xffff_0000, 0);

        cap.write(
            0x08,
            ByteEnabledDwordWrite::new(
                0x0001_0000,
                PciConfigByteEnable::from_offset_len(0x08, 1).unwrap(),
            ),
        );

        let status_after = read_cap_u32(&cap, 0x08) & 0xffff_0000;
        assert_eq!(status_after, 0);
    }

    #[test]
    fn test_pci_express_capability_write_unhandled_offset() {
        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Writing to unhandled offset should not panic
        write_cap_u32(&mut cap, 0x10, 0xFFFFFFFF);
        // Should not crash and should not affect other registers
        assert_eq!(read_cap_u32(&cap, 0x08), 0x2810);
    }

    #[test]
    fn test_pci_express_capability_reset() {
        let mut cap =
            PciExpressCapability::new(DevicePortType::RootPort, None).with_hotplug_support(1);
        cap.set_presence_detect_state(true);

        // Set some state
        write_cap_u32(&mut cap, 0x08, 0x0001); // Set some device control bits

        // Verify state is set
        let device_ctl_sts = read_cap_u32(&cap, 0x08);
        assert_ne!(device_ctl_sts, 0);
        let slot_status =
            pci_express::SlotStatus::from_bits((read_cap_u32(&cap, 0x18) >> 16) as u16);
        let link_status =
            pci_express::LinkStatus::from_bits((read_cap_u32(&cap, 0x10) >> 16) as u16);
        assert_eq!(slot_status.presence_detect_state(), 1);
        assert!(link_status.data_link_layer_link_active());

        // Reset the capability
        cap.reset();

        // Guest-controlled and event state resets, while externally managed
        // physical state remains unchanged.
        let device_ctl_sts_after_reset = read_cap_u32(&cap, 0x08);
        assert_eq!(device_ctl_sts_after_reset, 0x2810);
        let slot_status =
            pci_express::SlotStatus::from_bits((read_cap_u32(&cap, 0x18) >> 16) as u16);
        let link_status =
            pci_express::LinkStatus::from_bits((read_cap_u32(&cap, 0x10) >> 16) as u16);
        assert_eq!(slot_status.presence_detect_state(), 1);
        assert!(link_status.data_link_layer_link_active());
    }

    #[test]
    fn test_pci_express_capability_extended_registers() {
        let cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Test that extended registers return proper default values and don't crash
        // Link Capabilities should have default speed (Speed32_0GtS) and width (X16)
        let expected_link_caps =
            LinkSpeed::Speed32_0GtS.into_bits() | (LinkWidth::X16.into_bits() << 4) | (1 << 22);
        assert_eq!(read_cap_u32(&cap, 0x0C), expected_link_caps); // Link Capabilities
        // Link Control/Status should have Link Status with current_link_speed=5 and negotiated_link_width=16
        let expected_link_ctl_sts = (LinkSpeed::Speed32_0GtS.into_bits() as u16)
            | ((LinkWidth::X16.into_bits() as u16) << 4); // current_link_speed (bits 0-3) + negotiated_link_width (bits 4-9) = 5 + (16 << 4) = 5 + 256 = 261
        assert_eq!(
            read_cap_u32(&cap, 0x10),
            (expected_link_ctl_sts as u32) << 16
        ); // Link Control/Status (status in upper 16 bits)
        assert_eq!(read_cap_u32(&cap, 0x14), 0); // Slot Capabilities
        assert_eq!(read_cap_u32(&cap, 0x18), 0); // Slot Control/Status
        assert_eq!(read_cap_u32(&cap, 0x1C), 0); // Root Control/Capabilities
        assert_eq!(read_cap_u32(&cap, 0x20), 0); // Root Status
        assert_eq!(read_cap_u32(&cap, 0x24), 0); // Device Capabilities 2
        assert_eq!(read_cap_u32(&cap, 0x28), 0); // Device Control/Status 2
        // Link Capabilities 2 has supported_link_speeds_vector set to UpToGen5
        let expected_link_caps_2 = SupportedLinkSpeedsVector::UpToGen5.into_bits() << 1; // supported_link_speeds_vector at bits 1-7 = 31 << 1 = 62
        assert_eq!(read_cap_u32(&cap, 0x2C), expected_link_caps_2); // Link Capabilities 2
        // Link Control/Status 2 - Link Control 2 should have target_link_speed set to Speed32_0GtS (5)
        let expected_link_ctl_sts_2 = LinkSpeed::Speed32_0GtS.into_bits() as u16; // target_link_speed in lower 4 bits = 5
        assert_eq!(read_cap_u32(&cap, 0x30), expected_link_ctl_sts_2 as u32); // Link Control/Status 2
        assert_eq!(read_cap_u32(&cap, 0x34), 0); // Slot Capabilities 2
        assert_eq!(read_cap_u32(&cap, 0x38), 0); // Slot Control/Status 2
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

        write_cap_u32(&mut cap, slot_ctl_sts_offset, val_to_write);

        // Read back the slot control register (lower 16 bits)
        let read_back = read_cap_u32(&cap, slot_ctl_sts_offset);
        let slot_control_value = read_back as u16;
        let slot_control = pci_express::SlotControl::from_bits(slot_control_value);

        // Verify that features not present in capabilities were not set in control register
        assert!(
            !slot_control.attention_button_pressed_enable(),
            "Attention button enable should be 0 when capability not present"
        );
        assert!(
            !slot_control.power_fault_detected_enable(),
            "Power fault enable should be 0 without a power controller"
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
        assert!(!slot_control.auto_slot_power_limit_enable());
        assert!(!slot_control.in_band_pd_disable());

        // Native hotplug and DLL active reporting are advertised.
        assert!(slot_control.presence_detect_changed_enable());
        assert!(
            slot_control.hot_plug_interrupt_enable(),
            "Hotplug interrupt enable should be settable when hotplug capable"
        );
        assert!(slot_control.data_link_layer_state_changed_enable());
    }

    #[test]
    fn test_link_control_retrain_link_behavior() {
        // Test that retrain_link always reads as 0 regardless of what is written
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        let link_ctl_sts_offset = 0x10; // LINK_CTL_STS offset

        // Write a value with retrain_link bit set (bit 5)
        let write_val = 0x0020; // retrain_link bit (bit 5) = 1
        write_cap_u32(&mut cap, link_ctl_sts_offset, write_val);

        // Read back and verify retrain_link is always 0
        let read_back = read_cap_u32(&cap, link_ctl_sts_offset);
        let link_control = pci_express::LinkControl::from_bits(read_back as u16);

        assert!(
            !link_control.retrain_link(),
            "retrain_link should always read as 0"
        );

        // Verify other bits can still be set (except retrain_link)
        let write_val_2 = 0x0001; // aspm_control bit 0 = 1
        write_cap_u32(&mut cap, link_ctl_sts_offset, write_val_2);

        let read_back_2 = read_cap_u32(&cap, link_ctl_sts_offset);
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
    fn test_link_disable_is_read_only() {
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        cap.set_presence_detect_state(true);

        write_cap_u32(&mut cap, 0x10, 0x0010);

        let link_control = pci_express::LinkControl::from_bits(read_cap_u32(&cap, 0x10) as u16);
        assert!(!link_control.link_disable());
        assert!(
            pci_express::LinkStatus::from_bits((read_cap_u32(&cap, 0x10) >> 16) as u16)
                .data_link_layer_link_active()
        );
    }

    #[test]
    fn test_hotplug_link_capabilities() {
        // Test that Data Link Layer Link Active Reporting is enabled with hotplug
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        let cap_with_hotplug = cap.with_hotplug_support(1);

        let link_caps_offset = 0x0C; // LINK_CAPS offset
        let link_caps = read_cap_u32(&cap_with_hotplug, link_caps_offset);
        let link_capabilities = pci_express::LinkCapabilities::from_bits(link_caps);

        // Verify that Data Link Layer Link Active Reporting is enabled
        assert!(
            link_capabilities.data_link_layer_link_active_reporting(),
            "Data Link Layer Link Active Reporting should be enabled for hotplug"
        );

        // Verify default speed and width are still correct
        assert_eq!(
            link_capabilities.max_link_speed(),
            LinkSpeed::Speed32_0GtS,
            "Max link speed should be Speed32_0GtS (PCIe 32.0 GT/s)"
        );
        assert_eq!(
            link_capabilities.max_link_width(),
            LinkWidth::X16,
            "Max link width should be X16 (x16)"
        );

        // Test that non-hotplug capability doesn't have Data Link Layer Link Active Reporting
        let cap_no_hotplug = PciExpressCapability::new(DevicePortType::RootPort, None);
        let link_caps_no_hotplug = read_cap_u32(&cap_no_hotplug, link_caps_offset);
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
        let mut cap =
            PciExpressCapability::new(DevicePortType::RootPort, None).with_hotplug_support(1);
        cap.set_presence_detect_state(true);

        let link_ctl_sts_offset = 0x10; // LINK_CTL_STS offset

        // Read initial values
        let initial_read = read_cap_u32(&cap, link_ctl_sts_offset);
        let initial_link_status = pci_express::LinkStatus::from_bits((initial_read >> 16) as u16);

        // Verify initial values are set
        assert_eq!(
            initial_link_status.current_link_speed(),
            LinkSpeed::Speed32_0GtS,
            "Initial link speed should be set"
        );
        assert_eq!(
            initial_link_status.negotiated_link_width(),
            LinkWidth::X16,
            "Initial link width should be set"
        );
        assert!(!initial_link_status.link_training());
        assert!(
            initial_link_status.data_link_layer_link_active(),
            "Initial DLL should be active"
        );

        // Try to write different values to Link Status (upper 16 bits) while also writing to Link Control
        let write_val = 0xFFFF0001; // Upper 16 bits all 1s (Link Status), lower 16 bits = 1 (Link Control)
        write_cap_u32(&mut cap, link_ctl_sts_offset, write_val);

        // Read back and verify Link Status hasn't changed
        let after_write = read_cap_u32(&cap, link_ctl_sts_offset);
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
        assert!(!final_link_status.link_training());
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
        cap.set_hotplug_state(true);

        // Hotplug sets the two event latches this emulator implements.
        let initial_read = read_cap_u32(&cap, slot_ctl_sts_offset);
        let initial_status = pci_express::SlotStatus::from_bits((initial_read >> 16) as u16);
        assert!(initial_status.presence_detect_changed());
        assert!(initial_status.data_link_layer_state_changed());
        assert_eq!(initial_status.presence_detect_state(), 1);

        // Clear DLLSC while leaving PDC set.
        let clear_dllsc = pci_express::SlotStatus::new()
            .with_data_link_layer_state_changed(true)
            .into_bits();
        write_cap_u32(&mut cap, slot_ctl_sts_offset, u32::from(clear_dllsc) << 16);
        let status = pci_express::SlotStatus::from_bits(
            (read_cap_u32(&cap, slot_ctl_sts_offset) >> 16) as u16,
        );
        assert!(status.presence_detect_changed());
        assert!(!status.data_link_layer_state_changed());
        assert_eq!(status.presence_detect_state(), 1);

        // Clear PDC. The read-only physical presence bit remains set.
        let clear_pdc = pci_express::SlotStatus::new()
            .with_presence_detect_changed(true)
            .into_bits();
        write_cap_u32(&mut cap, slot_ctl_sts_offset, u32::from(clear_pdc) << 16);
        let status = pci_express::SlotStatus::from_bits(
            (read_cap_u32(&cap, slot_ctl_sts_offset) >> 16) as u16,
        );
        assert!(!status.presence_detect_changed());
        assert_eq!(status.presence_detect_state(), 1);

        // Re-applying the same physical state is not a new hotplug event.
        cap.set_hotplug_state(true);
        let status = pci_express::SlotStatus::from_bits(
            (read_cap_u32(&cap, slot_ctl_sts_offset) >> 16) as u16,
        );
        assert!(!status.presence_detect_changed());
        assert!(!status.data_link_layer_state_changed());
    }

    #[test]
    fn test_link_control_2_target_speed_validation() {
        // Target Link Speed is guest control state. Current Link Speed reports
        // negotiated hardware state and does not change until link retraining.
        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        let link_ctl_sts_2_offset = 0x30; // LINK_CTL_STS_2 offset

        // Initially, target link speed should be Speed32_0GtS (5) and current link speed should match
        let initial_read = read_cap_u32(&cap, link_ctl_sts_2_offset);
        let initial_link_control_2 = pci_express::LinkControl2::from_bits(initial_read as u16);
        assert_eq!(
            initial_link_control_2.target_link_speed(),
            LinkSpeed::Speed32_0GtS,
            "Initial target link speed should be Speed32_0GtS"
        );

        // Check that link status reflects this speed
        let link_ctl_sts_offset = 0x10; // LINK_CTL_STS offset
        let link_ctl_sts = read_cap_u32(&cap, link_ctl_sts_offset);
        let link_status = pci_express::LinkStatus::from_bits((link_ctl_sts >> 16) as u16);
        assert_eq!(
            link_status.current_link_speed(),
            LinkSpeed::Speed32_0GtS,
            "Initial current link speed should match target speed"
        );
        assert_eq!(
            link_status.negotiated_link_width(),
            LinkWidth::X16,
            "Initial negotiated link width should be X16"
        );

        // Test writing a valid speed (Speed16_0GtS = 4) that's less than max speed (Speed32_0GtS = 5)
        let valid_speed = LinkSpeed::Speed16_0GtS;
        write_cap_u32(&mut cap, link_ctl_sts_2_offset, valid_speed.into_bits());

        // Verify target link speed was set correctly
        let after_valid_write = read_cap_u32(&cap, link_ctl_sts_2_offset);
        let link_control_2_after_valid =
            pci_express::LinkControl2::from_bits(after_valid_write as u16);
        assert_eq!(
            link_control_2_after_valid.target_link_speed(),
            valid_speed,
            "Target link speed should be set to requested valid speed"
        );

        // Writing Target Link Speed alone does not initiate retraining.
        let link_ctl_sts_after_valid = read_cap_u32(&cap, link_ctl_sts_offset);
        let link_status_after_valid =
            pci_express::LinkStatus::from_bits((link_ctl_sts_after_valid >> 16) as u16);
        assert_eq!(
            link_status_after_valid.current_link_speed(),
            LinkSpeed::Speed32_0GtS,
            "Target Link Speed must not directly change negotiated link speed"
        );

        // Unsupported Target Link Speed encodings have undefined behavior. The
        // register preserves the guest value rather than inventing a clamp.
        let invalid_speed = LinkSpeed::Speed64_0GtS;
        write_cap_u32(&mut cap, link_ctl_sts_2_offset, invalid_speed.into_bits());

        let after_invalid_write = read_cap_u32(&cap, link_ctl_sts_2_offset);
        let link_control_2_after_invalid =
            pci_express::LinkControl2::from_bits(after_invalid_write as u16);
        assert_eq!(
            link_control_2_after_invalid.target_link_speed(),
            invalid_speed,
            "Target link speed should preserve the guest value"
        );

        // The unsupported target still does not alter negotiated status.
        let link_ctl_sts_after_invalid = read_cap_u32(&cap, link_ctl_sts_offset);
        let link_status_after_invalid =
            pci_express::LinkStatus::from_bits((link_ctl_sts_after_invalid >> 16) as u16);
        assert_eq!(
            link_status_after_invalid.current_link_speed(),
            LinkSpeed::Speed32_0GtS,
            "Target Link Speed must not directly change negotiated link speed"
        );

        // Verify that link width remains unchanged throughout
        assert_eq!(
            link_status_after_valid.negotiated_link_width(),
            LinkWidth::X16,
            "Negotiated link width should remain unchanged"
        );
        assert_eq!(
            link_status_after_invalid.negotiated_link_width(),
            LinkWidth::X16,
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
        let caps_val_no_hotplug = read_cap_u32(&cap_no_hotplug, 0x00);
        let pcie_caps_no_hotplug = (caps_val_no_hotplug >> 16) as u16;
        let slot_implemented_bit = (pcie_caps_no_hotplug >> 8) & 0x1; // slot_implemented is bit 8 of PCIe capabilities
        assert_eq!(
            slot_implemented_bit, 0,
            "slot_implemented should be 0 when hotplug is not enabled"
        );

        // Test with hotplug - slot_implemented should be true
        let cap_with_hotplug = cap_no_hotplug.with_hotplug_support(1);
        let caps_val_with_hotplug = read_cap_u32(&cap_with_hotplug, 0x00);
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
        let initial_slot_status = read_cap_u32(&cap, 0x18); // Slot Control + Slot Status
        let initial_presence_detect = (initial_slot_status >> 22) & 0x1; // presence_detect_state is bit 6 of slot status (upper 16 bits)
        assert_eq!(
            initial_presence_detect, 0,
            "Initial presence detect state should be 0"
        );

        // Set device as present
        cap.set_presence_detect_state(true);
        let present_slot_status = read_cap_u32(&cap, 0x18);
        let present_presence_detect = (present_slot_status >> 22) & 0x1;
        assert_eq!(
            present_presence_detect, 1,
            "Presence detect state should be 1 when device is present"
        );

        // Set device as not present
        cap.set_presence_detect_state(false);
        let absent_slot_status = read_cap_u32(&cap, 0x18);
        let absent_presence_detect = (absent_slot_status >> 22) & 0x1;
        assert_eq!(
            absent_presence_detect, 0,
            "Presence detect state should be 0 when device is not present"
        );
    }

    #[test]
    fn test_set_presence_detect_state_without_slot_implemented() {
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        let slot_status =
            pci_express::SlotStatus::from_bits((read_cap_u32(&cap, 0x18) >> 16) as u16);
        assert_eq!(slot_status.presence_detect_state(), 1);

        cap.set_presence_detect_state(true);
        assert!(
            pci_express::LinkStatus::from_bits((read_cap_u32(&cap, 0x10) >> 16) as u16,)
                .data_link_layer_link_active()
        );
        assert_eq!(
            pci_express::SlotStatus::from_bits((read_cap_u32(&cap, 0x18) >> 16) as u16)
                .presence_detect_state(),
            1
        );

        cap.set_presence_detect_state(false);
        assert!(
            !pci_express::LinkStatus::from_bits((read_cap_u32(&cap, 0x10) >> 16) as u16,)
                .data_link_layer_link_active()
        );
    }

    #[test]
    fn test_save_restore_default_state() {
        use vmcore::save_restore::SaveRestore;

        // Create a capability with default state
        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Save the default state
        let saved = cap.save().expect("save should succeed");

        // Verify default state values
        assert_eq!(saved.device_control, 0x2810);
        assert_eq!(saved.link_control, 0);
        assert_eq!(saved.slot_control, 0);
        assert_eq!(saved.slot_status_events, 0);
        assert_eq!(saved.root_control, 0);
        assert_eq!(saved.device_control_2, 0);
        // Link control 2 has default target_link_speed
        let expected_link_control_2 = LinkSpeed::Speed32_0GtS.into_bits() as u16;
        assert_eq!(saved.link_control_2, expected_link_control_2);
    }

    #[test]
    fn test_save_restore_modified_state() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        // Modify state by writing to registers
        // Write to Device Control (offset 0x08, lower 16 bits)
        write_cap_u32(&mut cap, 0x08, 0x0005); // Set some device control bits

        // Write to Link Control (offset 0x10, lower 16 bits)
        write_cap_u32(&mut cap, 0x10, 0x0003); // Set ASPM control bits

        // Write to Device Control 2 (offset 0x28, lower 16 bits)
        write_cap_u32(&mut cap, 0x28, 0x0020); // Enable advertised ARI forwarding

        // Save the modified state
        let saved = cap.save().expect("save should succeed");

        // Verify the saved state reflects the modifications
        assert_eq!(saved.device_control, 0x0005);
        assert_eq!(saved.link_control, 0x0003);
        assert_eq!(saved.device_control_2, 0x0020);
    }

    #[test]
    fn test_save_restore_roundtrip() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);

        // Modify various state registers
        write_cap_u32(&mut cap, 0x08, 0x000F); // Device Control
        write_cap_u32(&mut cap, 0x10, 0x0043); // Link Control
        write_cap_u32(&mut cap, 0x28, 0x0020); // Device Control 2
        write_cap_u32(&mut cap, 0x30, 0x0004); // Link Control 2 (target speed = 4)

        // Save the state
        let saved = cap.save().expect("save should succeed");

        // Create a new capability and restore the saved state
        let mut cap2 = PciExpressCapability::new(DevicePortType::RootPort, None);
        cap2.restore(saved).expect("restore should succeed");

        // Verify restored state by reading registers
        let device_ctl_sts = read_cap_u32(&cap2, 0x08);
        assert_eq!(
            device_ctl_sts & 0xFFFF,
            0x000F,
            "Device control should be restored"
        );

        let link_ctl_sts = read_cap_u32(&cap2, 0x10);
        assert_eq!(
            link_ctl_sts & 0xFFFF,
            0x0043,
            "Link control should be restored"
        );

        let device_ctl_sts_2 = read_cap_u32(&cap2, 0x28);
        assert_eq!(
            device_ctl_sts_2 & 0xFFFF,
            0x0020,
            "Device control 2 should be restored"
        );

        let link_ctl_sts_2 = read_cap_u32(&cap2, 0x30);
        assert_eq!(
            link_ctl_sts_2 & 0xFFFF,
            0x0004,
            "Link control 2 should be restored"
        );
    }

    #[test]
    fn test_save_restore_with_status_bits() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        cap = cap.with_hotplug_support(1);

        cap.set_presence_detect_state(true);

        // Set a real hardware-produced slot event latch.
        {
            let mut state = cap.state.lock();
            state
                .registers
                .slot_status_events
                .set_presence_detect_changed(true);
        }

        // Save the state
        let saved = cap.save().expect("save should succeed");

        // Verify status bits are in saved state
        let saved_slot_status = pci_express::SlotStatus::from_bits(saved.slot_status_events);
        assert!(saved_slot_status.presence_detect_changed());
        assert_eq!(saved_slot_status.presence_detect_state(), 0);

        // Restore into a port with different physical state. The snapshot's
        // source-side presence must not overwrite destination topology.
        let mut cap2 = PciExpressCapability::new(DevicePortType::RootPort, None);
        cap2 = cap2.with_hotplug_support(1);
        cap2.restore(saved).expect("restore should succeed");

        // Read back and verify the event latch was restored.
        let slot_ctl_sts = read_cap_u32(&cap2, 0x18);
        let restored_slot_status = pci_express::SlotStatus::from_bits((slot_ctl_sts >> 16) as u16);
        assert!(
            restored_slot_status.presence_detect_changed(),
            "Slot status should be restored"
        );
        assert_eq!(
            restored_slot_status.presence_detect_state(),
            0,
            "Presence detect state should be preserved independently of saved state"
        );
    }

    #[test]
    fn test_restore_masks_unsupported_fields() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        let mut saved = cap.save().expect("save should succeed");
        saved.device_control = u16::MAX;
        saved.link_control = u16::MAX;
        saved.slot_control = u16::MAX;
        saved.slot_status_events = u16::MAX;
        saved.root_control = u16::MAX;
        saved.device_control_2 = u16::MAX;
        saved.link_control_2 = u16::MAX;

        let mut cap2 = PciExpressCapability::new(DevicePortType::RootPort, None);
        cap2.restore(saved).expect("restore should succeed");

        let saved2 = cap2.save().expect("second save should succeed");
        assert_eq!(saved2.device_control, 0x78ff);
        assert_eq!(saved2.link_control, 0x00c3);
        assert_eq!(saved2.slot_control, 0);
        assert_eq!(saved2.slot_status_events, 0);
        assert_eq!(saved2.root_control, 0x000f);
        assert_eq!(saved2.device_control_2, 0x0020);
        assert_eq!(saved2.link_control_2, 0xffbf);
    }

    #[test]
    fn test_save_after_reset() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = PciExpressCapability::new(DevicePortType::Endpoint, None);

        // Modify state
        write_cap_u32(&mut cap, 0x08, 0x00FF);
        write_cap_u32(&mut cap, 0x10, 0x00FF);

        // Reset
        cap.reset();

        // Save after reset
        let saved = cap.save().expect("save should succeed");

        // Verify state is back to defaults
        assert_eq!(saved.device_control, 0x2810);
        assert_eq!(saved.link_control, 0);
        assert_eq!(saved.slot_status_events, 0);
    }

    // --- Orderly removal (attention button, power controller, power indicator) ---
    //
    // Offset 0x18 of the capability is the Slot Control (low 16 bits) / Slot Status (high 16
    // bits) dword; offset 0x10 holds Link Status in its high 16 bits.

    fn eject_control() -> pci_express::SlotControl {
        pci_express::SlotControl::new()
            .with_hot_plug_interrupt_enable(true)
            .with_attention_button_pressed_enable(true)
            .with_power_indicator_control(0b01)
    }

    fn attention_port(port_type: DevicePortType) -> PciExpressCapability {
        let cap = PciExpressCapability::new(port_type, None)
            .with_hotplug_support(1)
            .with_attention_button();
        cap.set_presence_detect_state(true);
        cap
    }

    fn slot_status(cap: &PciExpressCapability) -> pci_express::SlotStatus {
        pci_express::SlotStatus::from_bits((read_cap_u32(cap, 0x18) >> 16) as u16)
    }

    fn link_active(cap: &PciExpressCapability) -> bool {
        pci_express::LinkStatus::from_bits((read_cap_u32(cap, 0x10) >> 16) as u16)
            .data_link_layer_link_active()
    }

    fn poll_eject(
        completion: &mut mesh::OneshotReceiver<()>,
    ) -> std::task::Poll<Result<(), mesh::RecvError>> {
        Future::poll(
            std::pin::Pin::new(completion),
            &mut std::task::Context::from_waker(std::task::Waker::noop()),
        )
    }

    const ABP_CLEAR: u32 = 1 << 16;

    /// Slot Control as Linux leaves it during the five-second countdown that follows a
    /// press it acted on (pciehp_handle_button_press): the power indicator blinks.
    fn counting_down() -> pci_express::SlotControl {
        eject_control().with_power_indicator_control(0b10)
    }

    #[test]
    fn orderly_eject_presses_the_button_and_completes_only_when_power_and_indicator_are_off() {
        for port_type in [
            DevicePortType::RootPort,
            DevicePortType::DownstreamSwitchPort,
        ] {
            let mut cap = attention_port(port_type);
            write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
            let mut completion = cap.request_hotplug_eject().unwrap();
            assert!(poll_eject(&mut completion).is_pending());
            let status = slot_status(&cap);
            assert!(
                status.attention_button_pressed(),
                "the request presses the button"
            );
            assert!(
                !status.presence_detect_changed(),
                "an orderly request is not a removal: presence is untouched"
            );
            assert_eq!(status.presence_detect_state(), 1);
            assert!(
                link_active(&cap),
                "the card stays reachable while the guest tears down"
            );

            // The guest acknowledges the press: still nothing to detach.
            write_cap_u32(
                &mut cap,
                0x18,
                u32::from(eject_control().into_bits()) | ABP_CLEAR,
            );
            assert!(poll_eject(&mut completion).is_pending());

            // Power off first (Linux remove_board), indicator still on: not done yet.
            let power_off = eject_control().with_power_controller_control(true);
            write_cap_u32(&mut cap, 0x18, u32::from(power_off.into_bits()));
            assert!(poll_eject(&mut completion).is_pending());
            assert!(!link_active(&cap), "an unpowered slot has no link");
            assert!(
                slot_status(&cap).data_link_layer_state_changed(),
                "powering the slot off is a link change the guest can be told about"
            );

            // Indicator off: the guest is done.
            write_cap_u32(
                &mut cap,
                0x18,
                u32::from(power_off.with_power_indicator_control(0b11).into_bits()),
            );
            assert!(matches!(
                poll_eject(&mut completion),
                std::task::Poll::Ready(Ok(()))
            ));
            assert!(cap.hotplug_eject_complete());
            assert!(!cap.hotplug_eject_requested());
            assert!(
                cap.state.lock().presence_detect_state,
                "detaching is the caller's step"
            );
        }
    }

    #[test]
    fn orderly_eject_retry_does_not_press_the_button_again() {
        // A second press inside the guest's five-second window cancels the removal in
        // Linux pciehp, so a retried request must only re-arm the waiter.
        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
        drop(cap.request_hotplug_eject().unwrap());
        write_cap_u32(
            &mut cap,
            0x18,
            u32::from(counting_down().into_bits()) | ABP_CLEAR,
        );
        let mut retry = cap.request_hotplug_eject().unwrap();
        assert!(poll_eject(&mut retry).is_pending());
        assert!(
            !slot_status(&cap).attention_button_pressed(),
            "the retry must not press again"
        );
        write_cap_u32(
            &mut cap,
            0x18,
            u32::from(
                eject_control()
                    .with_power_controller_control(true)
                    .with_power_indicator_control(0b11)
                    .into_bits(),
            ),
        );
        assert!(matches!(
            poll_eject(&mut retry),
            std::task::Poll::Ready(Ok(()))
        ));
    }

    #[test]
    fn orderly_eject_retry_needs_no_enabled_interrupt() {
        // A retry during the countdown presses nothing, so it does not depend on which
        // hot-plug interrupts the guest has enabled at that moment.
        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
        drop(cap.request_hotplug_eject().unwrap());
        let masked = counting_down()
            .with_hot_plug_interrupt_enable(false)
            .with_attention_button_pressed_enable(false);
        write_cap_u32(&mut cap, 0x18, u32::from(masked.into_bits()) | ABP_CLEAR);
        assert!(cap.request_hotplug_eject().is_ok());
        assert!(!slot_status(&cap).attention_button_pressed());
    }

    #[test]
    fn orderly_eject_presses_again_when_the_guest_dropped_the_press() {
        // Linux acts on a press only in ON_STATE or OFF_STATE and drops it in POWERON and
        // POWEROFF (pciehp_handle_button_press), acknowledging Attention Button Pressed all
        // the same. The indicator then stays on and nothing will ever power the slot off,
        // so a retry must press again rather than wait on a removal that never started.
        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
        drop(cap.request_hotplug_eject().unwrap());
        write_cap_u32(
            &mut cap,
            0x18,
            u32::from(eject_control().into_bits()) | ABP_CLEAR,
        );
        assert!(!slot_status(&cap).attention_button_pressed());
        let mut retry = cap.request_hotplug_eject().unwrap();
        assert!(poll_eject(&mut retry).is_pending());
        assert!(
            slot_status(&cap).attention_button_pressed(),
            "a dropped press is pressed again"
        );
        assert!(cap.hotplug_eject_requested());
    }

    #[test]
    fn orderly_eject_retry_before_the_guest_acknowledged_does_not_press_again() {
        // The first press is still pending in Slot Status: the guest has not read it yet,
        // so it will still act on it.
        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
        drop(cap.request_hotplug_eject().unwrap());
        let mut retry = cap.request_hotplug_eject().unwrap();
        assert!(poll_eject(&mut retry).is_pending());
        assert!(slot_status(&cap).attention_button_pressed());
        assert!(cap.hotplug_eject_requested());
    }

    #[test]
    fn orderly_eject_refuses_a_new_press_while_the_guest_is_busy() {
        // A blinking indicator means the guest is counting down to a power change or
        // carrying one out (pciehp blinks in BLINKINGON/BLINKINGOFF and while powering on
        // in board_added). A press then cancels the countdown or is dropped, so it is
        // refused.
        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(counting_down().into_bits()));
        assert!(matches!(
            cap.request_hotplug_eject(),
            Err(PcieEjectError::GuestBusy)
        ));
        assert!(!cap.hotplug_eject_requested());
        assert!(!slot_status(&cap).attention_button_pressed());
    }

    #[test]
    fn orderly_eject_in_poll_mode_needs_only_the_button_enable() {
        // pciehp in poll mode enables Attention Button Pressed but not Hot-Plug Interrupt
        // Enable (pciehp_hpc.c pcie_enable_notification) and reads Slot Status on a timer,
        // so it still hears the press.
        let mut cap = attention_port(DevicePortType::RootPort);
        let polled = eject_control().with_hot_plug_interrupt_enable(false);
        write_cap_u32(&mut cap, 0x18, u32::from(polled.into_bits()));
        let mut completion = cap.request_hotplug_eject().unwrap();
        assert!(poll_eject(&mut completion).is_pending());
        assert!(slot_status(&cap).attention_button_pressed());
    }

    #[test]
    fn orderly_eject_refuses_what_it_cannot_do() {
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None);
        assert!(matches!(
            cap.request_hotplug_eject(),
            Err(PcieEjectError::NotHotplugCapable)
        ));
        let cap = cap.with_hotplug_support(1);
        assert!(matches!(
            cap.request_hotplug_eject(),
            Err(PcieEjectError::AttentionButtonUnsupported)
        ));
        let cap = cap.with_attention_button();
        assert!(matches!(
            cap.request_hotplug_eject(),
            Err(PcieEjectError::EmptySlot)
        ));
        cap.set_presence_detect_state(true);
        // A powered (reset) slot whose guest never enabled the button interrupt: a press
        // nobody hears would only run out the caller's timeout.
        let mut cap = cap;
        cap.reset();
        assert!(matches!(
            cap.request_hotplug_eject(),
            Err(PcieEjectError::InterruptDisabled)
        ));
        assert!(!cap.hotplug_eject_requested());
        assert!(!slot_status(&cap).attention_button_pressed());
    }

    #[test]
    fn orderly_eject_of_an_already_off_slot_completes_at_once() {
        // A slot the guest already powered off needs no round trip.
        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(
            &mut cap,
            0x18,
            u32::from(
                pci_express::SlotControl::new()
                    .with_power_controller_control(true)
                    .with_power_indicator_control(0b11)
                    .into_bits(),
            ),
        );
        let mut completion = cap.request_hotplug_eject().unwrap();
        assert!(matches!(
            poll_eject(&mut completion),
            std::task::Poll::Ready(Ok(()))
        ));
        assert!(!slot_status(&cap).attention_button_pressed());
    }

    #[test]
    fn reset_cancels_a_pending_eject() {
        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
        let mut completion = cap.request_hotplug_eject().unwrap();
        cap.reset();
        assert!(matches!(
            poll_eject(&mut completion),
            std::task::Poll::Ready(Err(_))
        ));
        assert!(!cap.hotplug_eject_requested());
        assert!(!slot_status(&cap).attention_button_pressed());
        assert!(
            link_active(&cap),
            "a populated slot comes out of reset powered"
        );
    }

    #[test]
    fn physical_removal_clears_the_eject_request_and_its_button_event() {
        for hotplug_event in [false, true] {
            let mut cap = attention_port(DevicePortType::RootPort);
            write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
            let mut completion = cap.request_hotplug_eject().unwrap();
            if hotplug_event {
                cap.set_hotplug_state(false);
            } else {
                cap.set_presence_detect_state(false);
            }
            assert!(matches!(
                poll_eject(&mut completion),
                std::task::Poll::Ready(Err(_))
            ));
            assert!(!cap.hotplug_eject_requested());
            assert!(
                !slot_status(&cap).attention_button_pressed(),
                "a stale press would eject the next card put into the slot"
            );
        }
    }

    #[test]
    fn hot_add_into_a_powered_off_attention_slot_presses_the_button() {
        // Linux enables Attention Button Pressed instead of Presence Detect Changed on a
        // port with a button, and an unpowered slot has no link, so the press is the only
        // thing that tells it a card arrived.
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None)
            .with_hotplug_support(1)
            .with_attention_button();
        assert!(
            cap.state
                .lock()
                .registers
                .slot_control
                .power_controller_control()
        );
        cap.set_hotplug_state(true);
        let status = slot_status(&cap);
        assert!(status.attention_button_pressed());
        assert!(status.presence_detect_changed());
        assert!(
            !status.data_link_layer_state_changed(),
            "the link stays down until the guest powers the slot"
        );
        assert!(!link_active(&cap));
    }

    #[test]
    fn hot_add_without_attention_button_is_unchanged() {
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None).with_hotplug_support(1);
        cap.set_hotplug_state(true);
        let status = slot_status(&cap);
        assert!(!status.attention_button_pressed());
        assert!(status.presence_detect_changed());
        assert!(status.data_link_layer_state_changed());
        assert!(link_active(&cap));
    }

    #[test]
    fn reset_power_controls_follow_capabilities_and_presence() {
        for port_type in [
            DevicePortType::RootPort,
            DevicePortType::DownstreamSwitchPort,
        ] {
            for power_controller in [false, true] {
                for power_indicator in [false, true] {
                    let mut cap =
                        PciExpressCapability::new(port_type, None).with_hotplug_support(1);
                    cap.slot_capabilities = cap
                        .slot_capabilities
                        .with_power_controller_present(power_controller)
                        .with_power_indicator_present(power_indicator);
                    for present in [false, true] {
                        cap.set_presence_detect_state(present);
                        cap.reset();
                        let control =
                            pci_express::SlotControl::from_bits(read_cap_u32(&cap, 0x18) as u16);
                        assert_eq!(
                            control.power_controller_control(),
                            power_controller && !present
                        );
                        let expected_indicator = match (power_indicator, present) {
                            (false, _) => 0,
                            (true, true) => 0b01,
                            (true, false) => 0b11,
                        };
                        assert_eq!(control.power_indicator_control(), expected_indicator);
                        // A port without a power controller keeps following presence.
                        assert_eq!(link_active(&cap), present);
                    }
                }
            }
        }
    }

    #[test]
    fn save_restore_mid_eject_keeps_the_request_without_a_second_press() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
        drop(cap.request_hotplug_eject().unwrap());
        write_cap_u32(
            &mut cap,
            0x18,
            u32::from(counting_down().into_bits()) | ABP_CLEAR,
        );
        let saved = cap.save().unwrap();
        assert_eq!(saved.eject_requested, Some(true));

        let mut restored = PciExpressCapability::new(DevicePortType::RootPort, None)
            .with_hotplug_support(1)
            .with_attention_button();
        restored.set_presence_detect_state(true);
        restored.restore(saved).unwrap();
        assert!(restored.hotplug_eject_requested());
        let mut retry = restored.request_hotplug_eject().unwrap();
        assert!(poll_eject(&mut retry).is_pending());
        assert!(
            !slot_status(&restored).attention_button_pressed(),
            "the restored request must not press again"
        );
    }

    #[test]
    fn restore_of_a_blob_without_the_eject_field_lands_on_no_request() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(counting_down().into_bits()));
        let mut saved = cap.save().unwrap();
        // What a blob from a build without orderly removal decodes to.
        saved.eject_requested = None;

        let mut restored = attention_port(DevicePortType::RootPort);
        restored.restore(saved).unwrap();
        assert!(!restored.hotplug_eject_requested());
    }

    #[test]
    fn restore_drops_an_eject_request_for_an_empty_slot() {
        use vmcore::save_restore::SaveRestore;

        let mut cap = attention_port(DevicePortType::RootPort);
        write_cap_u32(&mut cap, 0x18, u32::from(eject_control().into_bits()));
        drop(cap.request_hotplug_eject().unwrap());
        let saved = cap.save().unwrap();
        assert_eq!(saved.eject_requested, Some(true));

        // Hot-added cards are not part of the topology a restored VM is built from.
        let mut restored = PciExpressCapability::new(DevicePortType::RootPort, None)
            .with_hotplug_support(1)
            .with_attention_button();
        restored.restore(saved).unwrap();
        assert!(!restored.hotplug_eject_requested());
        assert!(!slot_status(&restored).attention_button_pressed());
    }

    #[test]
    fn a_card_attached_at_build_time_is_powered_on_an_attention_port() {
        let cap = PciExpressCapability::new(DevicePortType::RootPort, None)
            .with_hotplug_support(1)
            .with_attention_button();
        cap.set_presence_detect_state(true);
        let control = pci_express::SlotControl::from_bits(read_cap_u32(&cap, 0x18) as u16);
        assert!(!control.power_controller_control());
        assert_eq!(control.power_indicator_control(), 0b01);
        assert!(link_active(&cap));
        assert!(!slot_status(&cap).attention_button_pressed());
    }
}
