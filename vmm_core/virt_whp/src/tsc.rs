// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The CPUID leaves that expose the exact TSC frequency to partitions without
//! the time ABI.

use crate::Error;
use crate::VtlPartition;
use x86defs::cpuid::CpuidFunction;

/// Adds the CPUID leaves that expose the exact TSC frequency.
pub(crate) fn add_frequency_leaves(
    cpuid: Vec<virt::CpuidLeaf>,
    tsc_frequency_hz: u64,
    vtl0: &VtlPartition,
) -> Result<Vec<virt::CpuidLeaf>, Error> {
    let cpuid = virt::CpuidLeafSet::new(cpuid);
    let current_max_basic_leaf =
        cpuid.result(CpuidFunction::VendorAndMaxFunction.0, 0, &vtl0.cpuid(0, 0))[0];
    let mut cpuid = cpuid.into_leaves();
    cpuid.extend(virt::x86::tsc::tsc_frequency_cpuid_leaves(
        tsc_frequency_hz,
        current_max_basic_leaf,
    )?);
    Ok(cpuid)
}
