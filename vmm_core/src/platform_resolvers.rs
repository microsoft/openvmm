// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::partition_unit::Halt;
use power_resources::PowerRequest;
use power_resources::PowerRequestClient;
use power_resources::PowerRequestHandleKind;
use std::convert::Infallible;
use std::sync::Arc;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vmm_core_defs::HaltReason;

/// Platform power request resolver over [`Halt`].
pub struct HaltResolver(pub Arc<Halt>);

impl ResolveResource<PowerRequestHandleKind, PlatformResource> for HaltResolver {
    type Output = PowerRequestClient;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: PlatformResource,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        let halt = self.0.clone();
        Ok((move |request: PowerRequest| match request {
            PowerRequest::PowerOff => halt.halt(HaltReason::PowerOff),
            PowerRequest::Reset => halt.halt(HaltReason::Reset),
            PowerRequest::Hibernate => halt.halt(HaltReason::Hibernate),
            PowerRequest::TripleFault { vp } => halt.halt(HaltReason::TripleFault {
                vp,
                registers: None,
            }),
        })
        .into())
    }
}
