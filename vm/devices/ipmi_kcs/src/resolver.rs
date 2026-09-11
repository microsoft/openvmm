// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the IPMI KCS chipset device.

use crate::IpmiKcsDevice;
use crate::sink::SelDeps;
use crate::sink::SystemClock;
use crate::sink::TracingSelSink;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use ipmi_kcs_resources::IpmiKcsHandle;
use std::convert::Infallible;
use std::sync::Arc;
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
        resource: IpmiKcsHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        input.configure.omit_saved_state();
        let deps = SelDeps {
            sink: if resource.forward_sel {
                Arc::new(TracingSelSink)
            } else {
                Arc::new(crate::sink::NullSelSink)
            },
            clock: Arc::new(SystemClock),
        };
        Ok(IpmiKcsDevice::with_deps(deps).into())
    }
}
