// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions related to vmbus.

use crate::channel::VmbusDevice;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::CanResolveTo;
use vmcore::vm_task::VmTaskDriverSource;

impl CanResolveTo<ResolvedVmbusDevice> for VmbusDeviceHandleKind {
    type Input<'a> = ResolveVmbusDeviceHandleParams<'a>;
}

/// Resolve resource input parameters for vmbus device handles.
pub struct ResolveVmbusDeviceHandleParams<'a> {
    /// The driver source to use for spawning tasks and IO.
    pub driver_source: &'a VmTaskDriverSource,
}

/// A resolved vmbus device.
pub struct ResolvedVmbusDevice(pub Box<dyn VmbusDevice>);

impl<T: 'static + VmbusDevice> From<T> for ResolvedVmbusDevice {
    fn from(value: T) -> Self {
        Self(Box::new(value))
    }
}
