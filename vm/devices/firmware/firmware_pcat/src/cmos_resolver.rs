// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for PCAT default CMOS values.
//!
//! Resolves [`PcatDefaultCmosValuesHandle`] into a 256-byte CMOS image by
//! calling [`default_cmos_values_from_ram_size`](super::default_cmos_values_from_ram_size).

use chipset_resources::CmosRtcInitialValuesKind;
use chipset_resources::ResolvedCmosRtcInitialValues;
use chipset_resources::cmos_rtc_initial_values::PcatDefaultCmosValuesHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;

/// Resolver for [`PcatDefaultCmosValuesHandle`].
pub struct PcatDefaultCmosValuesResolver;

declare_static_resolver! {
    PcatDefaultCmosValuesResolver,
    (CmosRtcInitialValuesKind, PcatDefaultCmosValuesHandle),
}

impl ResolveResource<CmosRtcInitialValuesKind, PcatDefaultCmosValuesHandle>
    for PcatDefaultCmosValuesResolver
{
    type Output = ResolvedCmosRtcInitialValues;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        resource: PcatDefaultCmosValuesHandle,
        (): (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedCmosRtcInitialValues(
            super::default_cmos_values_from_ram_size(resource.first_ram_block_size),
        ))
    }
}
