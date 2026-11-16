// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Frontend-neutral USB controller configuration.
//!
//! A frontend describes each xHCI controller with a [`UsbControllerSpec`], and
//! [`controller_configs`] checks the rules that span controllers before
//! building the controller resources. Per-controller rules, such as connector
//! assignment, belong to the controller's resolver.

use std::collections::HashSet;
use usb_config_resources::UsbControllerConfig;
use usb_config_resources::UsbDeviceConfig;
use usb_config_resources::UsbDeviceConfigKind;
use usb_config_resources::WinUsbAllConfig;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// Typed configuration selecting a USB device backend.
#[derive(Clone, Debug)]
pub enum UsbBackend {
    /// Loopback mock.
    Loopback(usb_config_resources::MockLoopbackConfig),
    /// Composite HID mock.
    Hid(usb_config_resources::MockHidConfig),
    /// Mass-storage mock.
    Storage(usb_config_resources::MockStorageConfig),
    /// FX2 MUTT mock.
    Mutt(usb_config_resources::MockMuttConfig),
    /// FX3 SuperMUTT mock.
    SuperMutt(usb_config_resources::MockSuperMuttConfig),
    /// One Windows host device.
    WinUsb(usb_config_resources::WinUsbConfig),
}

impl UsbBackend {
    /// Erases the configuration type for resource resolution.
    pub fn into_resource(self) -> Resource<UsbDeviceConfigKind> {
        match self {
            Self::Loopback(config) => config.into_resource(),
            Self::Hid(config) => config.into_resource(),
            Self::Storage(config) => config.into_resource(),
            Self::Mutt(config) => config.into_resource(),
            Self::SuperMutt(config) => config.into_resource(),
            Self::WinUsb(config) => config.into_resource(),
        }
    }
}

/// An xHCI controller requested by a frontend.
#[derive(Debug)]
pub struct UsbControllerSpec {
    /// Name of the PCIe port the controller is attached to.
    pub port_name: String,
    /// Devices attached when the controller is created, each with an optional
    /// one-based connector.
    pub devices: Vec<(Option<u8>, UsbBackend)>,
    /// Whether to also attach eligible host WinUSB devices to the remaining
    /// free connectors when the controller is created.
    pub winusb_all: bool,
}

/// Checks the rules that span `controllers` and returns their configurations.
///
/// Each PCIe port may have only one controller, each WinUSB instance may be
/// selected only once, and only one controller may select WinUSB devices
/// automatically. Automatic selection skips every explicitly selected device.
pub fn controller_configs(
    controllers: Vec<UsbControllerSpec>,
) -> anyhow::Result<Vec<(String, UsbControllerConfig)>> {
    let mut port_names = HashSet::new();
    let mut instance_ids = HashSet::new();
    let mut winusb_all = false;
    for controller in &controllers {
        anyhow::ensure!(
            port_names.insert(controller.port_name.as_str()),
            "duplicate USB controller on PCIe port {:?}",
            controller.port_name
        );
        for (_, backend) in &controller.devices {
            if let UsbBackend::WinUsb(config) = backend {
                // Windows instance IDs are case-insensitive.
                anyhow::ensure!(
                    instance_ids.insert(config.instance_id.to_ascii_uppercase()),
                    "WinUSB instance selected more than once: {}",
                    config.instance_id
                );
            }
        }
        if controller.winusb_all {
            anyhow::ensure!(!winusb_all, "winusb-all may be selected only once per VM");
            winusb_all = true;
        }
    }
    // Automatic selection runs after every explicit selection is known, so it
    // skips devices selected by controllers listed after it.
    let mut excluded_instance_ids: Vec<_> = instance_ids.into_iter().collect();
    excluded_instance_ids.sort();
    Ok(controllers
        .into_iter()
        .map(|controller| {
            let mut devices: Vec<_> = controller
                .devices
                .into_iter()
                .map(|(connector, backend)| UsbDeviceConfig::Device {
                    connector,
                    device: backend.into_resource(),
                })
                .collect();
            if controller.winusb_all {
                devices.push(UsbDeviceConfig::Discover(
                    WinUsbAllConfig {
                        excluded_instance_ids: excluded_instance_ids.clone(),
                    }
                    .into_resource(),
                ));
            }
            (controller.port_name, UsbControllerConfig { devices })
        })
        .collect())
}
