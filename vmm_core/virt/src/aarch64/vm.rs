// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Aarch64PartitionCapabilities;
use crate::state::state_trait;
use vm_topology::processor::aarch64::Aarch64VpInfo;

state_trait!(
    "Access to per-VM state.",
    AccessVmState,
    Aarch64PartitionCapabilities,
    Aarch64VpInfo,
    VmSavedState,
    "virt.aarch64",
);
