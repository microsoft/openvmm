// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the CMOS RTC time source.

use super::local_clock::ArcMutexUnderhillLocalClock;
use chipset_resources::CmosRtcTimeSourceHandleKind;
use chipset_resources::ResolvedCmosRtcTimeSource;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;

pub struct UnderhillCmosRtcTimeSourceResolver {
    pub time_source: ArcMutexUnderhillLocalClock,
}

impl ResolveResource<CmosRtcTimeSourceHandleKind, PlatformResource>
    for UnderhillCmosRtcTimeSourceResolver
{
    type Output = ResolvedCmosRtcTimeSource;
    type Error = std::convert::Infallible;

    fn resolve(&self, _resource: PlatformResource, (): ()) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedCmosRtcTimeSource(Box::new(
            self.time_source.new_linked_clock(),
        )))
    }
}
