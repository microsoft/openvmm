// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource kind definitions that are used by multiple devices.
//!
//! This exists as a convenient place to define resource kinds without creating
//! a new crate or putting them in a crate that has a lot of unnecessary build
//! dependencies.
//!
//! Not all resource kinds need to be defined here. If you are adding a new kind
//! and there is a more natural resource crate to put the kind definition in,
//! put it there. For example, if you are defining a resource kind for a
//! resource kind specific to a single device, put it in the resource crate for
//! that device.

use crate::ResourceKind;

/// A resource kind for chipset device handles.
pub enum ChipsetDeviceHandleKind {}

impl ResourceKind for ChipsetDeviceHandleKind {
    const NAME: &'static str = "chipset_device_handle";
}

/// A resource kind for keyboard input source handles.
pub enum KeyboardInputHandleKind {}

impl ResourceKind for KeyboardInputHandleKind {
    const NAME: &'static str = "keyboard_input_handle";
}

/// A resource kind for mouse input source handles.
pub enum MouseInputHandleKind {}

impl ResourceKind for MouseInputHandleKind {
    const NAME: &'static str = "mouse_input_handle";
}

/// Resource kind for network endpoints.
pub enum NetEndpointHandleKind {}

impl ResourceKind for NetEndpointHandleKind {
    const NAME: &'static str = "net_endpoint_handle";
}

/// A resource kind for PCI device handles.
pub enum PciDeviceHandleKind {}

impl ResourceKind for PciDeviceHandleKind {
    const NAME: &'static str = "pci_device_handle";
}

/// A serial backend resource kind, where the underlying OS resources have
/// already been opened in a privileged context.
pub enum SerialBackendHandle {}

impl ResourceKind for SerialBackendHandle {
    const NAME: &'static str = "serial_handle";
}

/// A disk resource kind, where the underlying resources have already been
/// opened in a privileged context.
pub enum DiskHandleKind {}

impl ResourceKind for DiskHandleKind {
    const NAME: &'static str = "disk_handle";
}

/// A resource kind for SCSI devices.
pub enum ScsiDeviceHandleKind {}

impl ResourceKind for ScsiDeviceHandleKind {
    const NAME: &'static str = "scsi_device";
}

/// A resource kind for framebuffer memory that can be mapped into a VM.
pub enum FramebufferHandleKind {}

impl ResourceKind for FramebufferHandleKind {
    const NAME: &'static str = "framebuffer";
}

/// A resource kind for virtio device handles.
pub enum VirtioDeviceHandle {}

impl ResourceKind for VirtioDeviceHandle {
    const NAME: &'static str = "virtio";
}

/// Resource kind for vmbus device handles.
pub enum VmbusDeviceHandleKind {}

impl ResourceKind for VmbusDeviceHandleKind {
    const NAME: &'static str = "vmbus_device_handle";
}

/// Resource kind for non-volatile stores.
pub enum NonVolatileStoreKind {}

impl ResourceKind for NonVolatileStoreKind {
    const NAME: &'static str = "nvstore";
}
