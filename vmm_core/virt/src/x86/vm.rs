// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-VM state.

use super::X86PartitionCapabilities;
use crate::state::state_trait;
use crate::state::HvRegisterState;
use crate::state::StateElement;
use hvdef::HvRegisterValue;
use hvdef::HvX64RegisterName;
use inspect::Inspect;
use mesh_protobuf::Protobuf;
use vm_topology::processor::x86::X86VpInfo;

#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct HypercallMsrs {
    #[mesh(1)]
    #[inspect(hex)]
    pub guest_os_id: u64,
    #[mesh(2)]
    #[inspect(hex)]
    pub hypercall: u64,
}

impl HvRegisterState<HvX64RegisterName, 2> for HypercallMsrs {
    fn names(&self) -> &'static [HvX64RegisterName; 2] {
        &[HvX64RegisterName::GuestOsId, HvX64RegisterName::Hypercall]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.guest_os_id, self.hypercall]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.guest_os_id, &mut self.hypercall]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for HypercallMsrs {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
    }
}

#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct ReferenceTscPage {
    #[mesh(1)]
    #[inspect(hex)]
    pub tsc_reference_page: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for ReferenceTscPage {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::ReferenceTsc]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.tsc_reference_page]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.tsc_reference_page]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for ReferenceTscPage {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1_reference_tsc_page
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self::default()
    }
}

#[repr(C)]
#[derive(Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct ReferenceTime {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for ReferenceTime {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::TimeRefCount]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.value]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.value]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for ReferenceTime {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.hv1
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { value: 0 }
    }

    fn can_compare(caps: &X86PartitionCapabilities) -> bool {
        caps.can_freeze_time
    }
}

state_trait!(
    "Access to per-VM state.",
    AccessVmState,
    X86PartitionCapabilities,
    X86VpInfo,
    VmSavedState,
    "virt.x86",
    (1, "hypercall", hypercall, set_hypercall, HypercallMsrs),
    (2, "reftime", reftime, set_reftime, ReferenceTime),
    (
        3,
        "reference_tsc_page",
        reference_tsc_page,
        set_reference_tsc_page,
        ReferenceTscPage,
    ),
);
