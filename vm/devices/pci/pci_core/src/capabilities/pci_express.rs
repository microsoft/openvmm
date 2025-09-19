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
}

impl PciExpressState {
    fn new() -> Self {
        Self {
            device_control: pci_express::DeviceControl::new(),
            device_status: pci_express::DeviceStatus::new(),
        }
    }
}

#[derive(Inspect)]
/// Configurable PCI Express capability.
pub struct PciExpressCapability {
    device_capabilities: pci_express::DeviceCapabilities,
    state: Arc<Mutex<PciExpressState>>,
    #[inspect(skip)]
    flr_handler: Option<Arc<dyn FlrHandler>>,
}

impl PciExpressCapability {
    /// Creates a new PCI Express capability with FLR support.
    ///
    /// # Arguments
    /// * `flr_handler` - Optional handler to be called when FLR is initiated. This emulator will report that FLR is supported if flr_handler = Some(_)
    pub fn new(flr_handler: Option<Arc<dyn FlrHandler>>) -> Self {
        Self {
            device_capabilities: pci_express::DeviceCapabilities::new()
                .with_function_level_reset(flr_handler.is_some()),
            state: Arc::new(Mutex::new(PciExpressState::new())),
            flr_handler,
        }
    }

    fn handle_device_control_write(&mut self, new_control: pci_express::DeviceControl) {
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
        let label = self.label();
        match PciExpressCapabilityHeader(offset) {
            PciExpressCapabilityHeader::PCIE_CAPS => {
                // PCIe Capabilities Register (16 bits) + Next Pointer (8 bits) + Capability ID (8 bits)
                // For basic endpoint: Version=2, Device/Port Type=0 (PCI Express Endpoint)
                let pcie_caps: u16 = 0x0002; // Version 2, Device/Port Type 0
                (pcie_caps as u32) << 16 | CapabilityId::PCI_EXPRESS.0 as u32
            }
            PciExpressCapabilityHeader::DEVICE_CAPS => self.device_capabilities.into_bits(),
            PciExpressCapabilityHeader::DEVICE_CTL_STS => {
                let state = self.state.lock();
                let device_control = state.device_control.into_bits() as u32;
                let device_status = state.device_status.into_bits() as u32;
                device_control | (device_status << 16)
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
            pub flr_handler: u16,
        }
    }

    impl SaveRestore for PciExpressCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Err(SaveError::NotSupported)
        }

        fn restore(&mut self, _: Self::SavedState) -> Result<(), RestoreError> {
            Err(RestoreError::SavedStateNotSupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_pci_express_capability_read_u32() {
        let flr_handler = TestFlrHandler::new();
        let cap = PciExpressCapability::new(Some(flr_handler));

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

        // Test unhandled offset - should return 0 and not panic
        let unhandled_val = cap.read_u32(0x10);
        assert_eq!(unhandled_val, 0);
    }

    #[test]
    fn test_pci_express_capability_read_u32_no_flr() {
        let cap = PciExpressCapability::new(None);

        // Test Device Capabilities Register (offset 0x04) - FLR should not be set
        let device_caps_val = cap.read_u32(0x04);
        assert_eq!(device_caps_val & PCI_EXPRESS_DEVICE_CAPS_FLR_BIT_MASK, 0);
    }

    #[test]
    fn test_pci_express_capability_write_u32_readonly_registers() {
        let mut cap = PciExpressCapability::new(None);

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
        let mut cap = PciExpressCapability::new(Some(flr_handler.clone()));

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
        let mut cap = PciExpressCapability::new(None);

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
        let mut cap = PciExpressCapability::new(None);

        // Writing to unhandled offset should not panic
        cap.write_u32(0x10, 0xFFFFFFFF);
        // Should not crash and should not affect other registers
        assert_eq!(cap.read_u32(0x08), 0); // Device Control/Status should still be 0
    }

    #[test]
    fn test_pci_express_capability_reset() {
        let flr_handler = TestFlrHandler::new();
        let mut cap = PciExpressCapability::new(Some(flr_handler.clone()));

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
    fn test_pci_express_capability_length() {
        let cap = PciExpressCapability::new(None);
        assert_eq!(cap.len(), 0x0C); // Should be 12 bytes
    }

    #[test]
    fn test_pci_express_capability_label() {
        let cap = PciExpressCapability::new(None);
        assert_eq!(cap.label(), "pci-express");
    }
}
