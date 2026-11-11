// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Data-only configuration resources for USB controllers and device selections.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::ResourceKind;
use vm_resource::kind::PciDeviceHandleKind;

/// Configuration for an emulated xHCI controller.
#[derive(Debug, Default, MeshPayload)]
pub struct UsbControllerConfig {
    /// Requested attachments, in configuration order.
    pub devices: Vec<UsbDeviceConfig>,
}

impl ResourceId<PciDeviceHandleKind> for UsbControllerConfig {
    const ID: &'static str = "usb_xhci_config";
}

/// A configured device or a request to discover devices for free connectors.
#[derive(Debug, MeshPayload)]
pub enum UsbDeviceConfig {
    /// Attach one configured device to a connector.
    Device {
        /// One-based physical connector number. `None` selects the lowest
        /// free connector after every explicit connector is reserved.
        connector: Option<u8>,
        /// Configuration interpreted by a device-specific resolver.
        device: Resource<UsbDeviceConfigKind>,
    },
    /// Discover initial attachments for the remaining free connectors.
    Discover(Resource<UsbDeviceDiscoveryConfigKind>),
}

/// Configuration of a single USB device, independent of its implementation.
pub enum UsbDeviceConfigKind {}

impl ResourceKind for UsbDeviceConfigKind {
    const NAME: &'static str = "usb_device_config";
}

/// Configuration of a USB device discovery request.
pub enum UsbDeviceDiscoveryConfigKind {}

impl ResourceKind for UsbDeviceDiscoveryConfigKind {
    const NAME: &'static str = "usb_device_discovery_config";
}

/// User-supplied identity overrides; omitted values are chosen by the resolver.
#[derive(Clone, Debug, Default, PartialEq, Eq, MeshPayload)]
pub struct UsbIdentityConfig {
    /// USB vendor ID override.
    pub vendor_id: Option<u16>,
    /// USB product ID override.
    pub product_id: Option<u16>,
    /// Serial-number override, including an explicitly empty serial number.
    pub serial_number: Option<String>,
}

/// Configuration of a loopback test device.
#[derive(Clone, Debug, Default, PartialEq, Eq, MeshPayload)]
pub struct MockLoopbackConfig {
    /// Optional identity overrides.
    pub identity: UsbIdentityConfig,
}

impl ResourceId<UsbDeviceConfigKind> for MockLoopbackConfig {
    const ID: &'static str = "mock_loopback_config";
}

/// Configuration of a keyboard-and-mouse test device.
#[derive(Clone, Debug, Default, PartialEq, Eq, MeshPayload)]
pub struct MockHidConfig {
    /// Optional identity overrides.
    pub identity: UsbIdentityConfig,
}

impl ResourceId<UsbDeviceConfigKind> for MockHidConfig {
    const ID: &'static str = "mock_hid_config";
}

/// Configuration of a USB storage test device.
#[derive(Clone, Debug, Default, PartialEq, Eq, MeshPayload)]
pub struct MockStorageConfig {
    /// Optional identity overrides.
    pub identity: UsbIdentityConfig,
    /// Optional capacity in 512-byte logical blocks.
    pub block_count: Option<u64>,
}

impl ResourceId<UsbDeviceConfigKind> for MockStorageConfig {
    const ID: &'static str = "mock_storage_config";
}

/// Configuration of a MUTT test device.
#[derive(Clone, Debug, Default, PartialEq, Eq, MeshPayload)]
pub struct MockMuttConfig {
    /// Optional identity overrides.
    pub identity: UsbIdentityConfig,
}

impl ResourceId<UsbDeviceConfigKind> for MockMuttConfig {
    const ID: &'static str = "mock_mutt_config";
}

/// Configuration of a SuperMUTT test device.
#[derive(Clone, Debug, Default, PartialEq, Eq, MeshPayload)]
pub struct MockSuperMuttConfig;

impl ResourceId<UsbDeviceConfigKind> for MockSuperMuttConfig {
    const ID: &'static str = "mock_supermutt_config";
}

/// Configuration selecting a Windows device by PnP instance ID.
#[derive(Clone, Debug, PartialEq, Eq, MeshPayload)]
pub struct WinUsbConfig {
    /// Instance ID of the host device.
    pub instance_id: String,
}

impl ResourceId<UsbDeviceConfigKind> for WinUsbConfig {
    const ID: &'static str = "winusb_config";
}

/// Configuration selecting eligible WinUSB devices at startup.
#[derive(Clone, Debug, Default, PartialEq, Eq, MeshPayload)]
pub struct WinUsbAllConfig {
    /// Instance IDs already selected explicitly by the surrounding configuration.
    pub excluded_instance_ids: Vec<String>,
}

impl ResourceId<UsbDeviceDiscoveryConfigKind> for WinUsbAllConfig {
    const ID: &'static str = "winusb_all_config";
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use vm_resource::IntoResource;

    /// Round-trips both omitted and explicit overrides without filling in backend defaults.
    #[test]
    fn overrides_round_trip_without_applying_defaults() {
        for config in [
            MockStorageConfig::default(),
            MockStorageConfig {
                identity: UsbIdentityConfig {
                    vendor_id: Some(0x1234),
                    product_id: Some(0x5678),
                    serial_number: Some(String::new()),
                },
                block_count: Some(4096),
            },
        ] {
            let serialized = mesh::OwnedMessage::new(config.clone()).serialize();
            let decoded: MockStorageConfig =
                mesh::OwnedMessage::serialized(serialized).parse().unwrap();
            // In particular, an explicitly empty serial must remain distinct from None.
            assert_eq!(decoded, config);
        }
        // Defaults belong to the resolver, not the public configuration payload.
        assert_eq!(MockStorageConfig::default().block_count, None);
        assert_eq!(UsbIdentityConfig::default().serial_number, None);
    }

    /// Preserves attachment order, connector numbers, and resource kinds across serialization.
    #[test]
    fn controller_configuration_round_trip() {
        let config = UsbControllerConfig {
            devices: vec![
                UsbDeviceConfig::Device {
                    connector: Some(15),
                    device: MockHidConfig::default().into_resource(),
                },
                UsbDeviceConfig::Device {
                    connector: None,
                    device: MockStorageConfig::default().into_resource(),
                },
                UsbDeviceConfig::Discover(WinUsbAllConfig::default().into_resource()),
            ],
        };
        let serialized = mesh::OwnedMessage::new(config).serialize();
        let decoded: UsbControllerConfig =
            mesh::OwnedMessage::serialized(serialized).parse().unwrap();
        let UsbDeviceConfig::Device { connector, device } = &decoded.devices[0] else {
            panic!("expected a fixed attachment");
        };
        // Serialization must not convert the public one-based connector to an internal index.
        assert_eq!(*connector, Some(15));
        assert_eq!(device.id(), MockHidConfig::ID);
        // An omitted connector must stay omitted so the resolver can choose one.
        let UsbDeviceConfig::Device { connector, device } = &decoded.devices[1] else {
            panic!("expected an attachment without a connector");
        };
        assert_eq!(*connector, None);
        assert_eq!(device.id(), MockStorageConfig::ID);
        let UsbDeviceConfig::Discover(discovery) = &decoded.devices[2] else {
            panic!("expected a discovery request");
        };
        assert_eq!(discovery.id(), WinUsbAllConfig::ID);
        // The default configuration requests no implicit device attachments.
        assert!(UsbControllerConfig::default().devices.is_empty());
        // The controller identifier is part of the serialized resource contract.
        assert_eq!(UsbControllerConfig::ID, "usb_xhci_config");
    }
}
