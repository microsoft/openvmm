// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the IPMI KCS chipset device.

use crate::IpmiKcsDevice;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use ipmi_kcs_resources::IpmiKcsHandle;
use std::convert::Infallible;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// The resource resolver for [`IpmiKcsDevice`].
pub struct IpmiKcsResolver;

declare_static_resolver!(IpmiKcsResolver, (ChipsetDeviceHandleKind, IpmiKcsHandle));

impl ResolveResource<ChipsetDeviceHandleKind, IpmiKcsHandle> for IpmiKcsResolver {
    type Output = ResolvedChipsetDevice;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: IpmiKcsHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        input.configure.omit_saved_state();
        Ok(IpmiKcsDevice::new().into())
    }
}
