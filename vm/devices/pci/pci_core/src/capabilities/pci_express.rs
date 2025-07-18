// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI Express Capability with Function Level Reset (FLR) support.

use super::PciCapability;
use crate::spec::caps::CapabilityId;
use crate::spec::caps::pci_express;
use crate::spec::caps::pci_express::PciExpressCapabilityHeader;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;

/// Callback interface for handling Function Level Reset (FLR) events.
pub trait FlrHandler: Send + Sync + Inspect {
    /// Called when Function Level Reset is initiated.
    fn initiate_flr(&self);
}

#[derive(Debug)]
struct PciExpressState {
    device_control: pci_express::DeviceControl,
    device_status: pci_express::DeviceStatus,
}

impl Inspect for PciExpressState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("device_control", &format!("{:?}", self.device_control))
            .field("device_status", &format!("{:?}", self.device_status));
    }
}

impl PciExpressState {
    fn new() -> Self {
        Self {
            device_control: pci_express::DeviceControl::new(),
            device_status: pci_express::DeviceStatus::new(),
        }
    }
}

/// PCI Express capability with Function Level Reset support.
pub struct PciExpressCapability {
    device_capabilities: pci_express::DeviceCapabilities,
    state: Arc<Mutex<PciExpressState>>,
    flr_handler: Option<Arc<dyn FlrHandler>>,
}

impl Inspect for PciExpressCapability {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field(
                "device_capabilities",
                &format!("{:?}", self.device_capabilities),
            )
            .field(
                "state",
                &inspect::adhoc(|req| self.state.lock().inspect(req)),
            );
    }
}

impl PciExpressCapability {
    /// Creates a new PCI Express capability with FLR support.
    ///
    /// # Arguments
    /// * `flr_supported` - Whether Function Level Reset is supported
    /// * `flr_handler` - Optional handler to be called when FLR is initiated
    pub fn new(flr_supported: bool, flr_handler: Option<Arc<dyn FlrHandler>>) -> Self {
        let device_capabilities = pci_express::DeviceCapabilities::new()
            .with_function_level_reset(flr_supported)
            .with_max_payload_size(0) // 128 bytes
            .with_phantom_functions(0)
            .with_ext_tag_field(false)
            .with_endpoint_l0s_latency(0)
            .with_endpoint_l1_latency(0)
            .with_role_based_error(0)
            .with_captured_slot_power_limit(0)
            .with_captured_slot_power_scale(0);

        Self {
            device_capabilities,
            state: Arc::new(Mutex::new(PciExpressState::new())),
            flr_handler,
        }
    }

    fn handle_device_control_write(&mut self, new_control: pci_express::DeviceControl) {
        let mut state = self.state.lock();

        // Check if FLR was initiated
        if new_control.initiate_function_level_reset()
            && !state.device_control.initiate_function_level_reset()
        {
            if let Some(handler) = &self.flr_handler {
                handler.initiate_flr();
            }
        }

        // Update the control register but clear the FLR bit as it's self-clearing
        let mut updated_control = new_control;
        updated_control.set_initiate_function_level_reset(false);
        state.device_control = updated_control;
    }
}

impl PciCapability for PciExpressCapability {
    fn label(&self) -> &str {
        "pci-express"
    }

    fn len(&self) -> usize {
        // We only implement the basic PCIe capability structure:
        // 0x00: PCIe Capabilities (2 bytes) + Next Pointer (1 byte) + Capability ID (1 byte)
        // 0x04: Device Capabilities (4 bytes)
        // 0x08: Device Control (2 bytes) + Device Status (2 bytes)
        // Total: 12 bytes (0x0C)
        0x0C
    }

    fn read_u32(&self, offset: u16) -> u32 {
        match PciExpressCapabilityHeader(offset) {
            PciExpressCapabilityHeader::PCIE_CAPS => {
                // PCIe Capabilities Register (16 bits) + Next Pointer (8 bits) + Capability ID (8 bits)
                // For basic endpoint: Version=2, Device/Port Type=0 (PCI Express Endpoint)
                let pcie_caps: u16 = 0x0002; // Version 2, Device/Port Type 0
                (pcie_caps as u32) << 16 | (0x00 << 8) | CapabilityId::PCI_EXPRESS.0 as u32
            }
            PciExpressCapabilityHeader::DEVICE_CAPS => self.device_capabilities.into_bits(),
            PciExpressCapabilityHeader::DEVICE_CTL_STS => {
                let state = self.state.lock();
                let device_control = state.device_control.into_bits() as u32;
                let device_status = state.device_status.into_bits() as u32;
                device_control | (device_status << 16)
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, "unhandled pci express capability read");
                0
            }
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        match PciExpressCapabilityHeader(offset) {
            PciExpressCapabilityHeader::PCIE_CAPS => {
                // PCIe Capabilities register is read-only
                tracelimit::warn_ratelimited!(offset, val, "write to read-only pcie capabilities");
            }
            PciExpressCapabilityHeader::DEVICE_CAPS => {
                // Device Capabilities register is read-only
                tracelimit::warn_ratelimited!(
                    offset,
                    val,
                    "write to read-only device capabilities"
                );
            }
            PciExpressCapabilityHeader::DEVICE_CTL_STS => {
                // Lower 16 bits are Device Control (read-write)
                // Upper 16 bits are Device Status (read-write, but some bits are read-only)
                let new_control = pci_express::DeviceControl::from_bits(val as u16);
                self.handle_device_control_write(new_control);

                // Handle Device Status - most bits are write-1-to-clear
                let new_status = pci_express::DeviceStatus::from_bits((val >> 16) as u16);
                let mut state = self.state.lock();
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
            _ => {
                tracelimit::warn_ratelimited!(
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
        }
    }

    impl SaveRestore for PciExpressCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let state = self.state.lock();
            Ok(state::SavedState {
                device_control: state.device_control.into_bits(),
                device_status: state.device_status.into_bits(),
            })
        }

        fn restore(&mut self, saved_state: Self::SavedState) -> Result<(), RestoreError> {
            let mut state = self.state.lock();
            state.device_control =
                pci_express::DeviceControl::from_bits(saved_state.device_control);
            state.device_status = pci_express::DeviceStatus::from_bits(saved_state.device_status);
            Ok(())
        }
    }
}
