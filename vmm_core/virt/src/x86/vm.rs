// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-VM state.

use super::X86PartitionCapabilities;
use crate::state::HvRegisterState;
use crate::state::StateElement;
use crate::state::state_trait;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateError;
    use crate::x86::tests::basic_caps;
    use test_with_tracing::test;
    use vm_topology::processor::VpIndex;
    use vm_topology::processor::VpInfo;

    #[derive(Debug, thiserror::Error)]
    #[error("fake access failed")]
    struct FakeError;

    /// A partition that stores its state in memory. `failing` names an
    /// operation ("get", "set", or "commit") that fails. When time isn't
    /// frozen, the reference time advances on every read.
    struct FakeVm {
        caps: X86PartitionCapabilities,
        hypercall: (u64, u64),
        reftime: u64,
        tsc_page: u64,
        commits: usize,
        failing: Option<&'static str>,
    }

    impl FakeVm {
        fn new(hv1: bool, reference_tsc_page: bool, can_freeze_time: bool) -> Self {
            let mut caps = basic_caps();
            caps.hv1 = hv1;
            caps.hv1_reference_tsc_page = reference_tsc_page;
            caps.can_freeze_time = can_freeze_time;
            Self {
                caps,
                hypercall: (0x8100_0000_0000_0001, 0x1_0001),
                reftime: 0x1234,
                tsc_page: 0x2_0001,
                commits: 0,
                failing: None,
            }
        }

        fn check(&self, op: &str) -> Result<(), FakeError> {
            if self.failing == Some(op) {
                Err(FakeError)
            } else {
                Ok(())
            }
        }
    }

    impl AccessVmState for FakeVm {
        type Error = FakeError;

        fn caps(&self) -> &X86PartitionCapabilities {
            &self.caps
        }

        fn commit(&mut self) -> Result<(), FakeError> {
            self.check("commit")?;
            self.commits += 1;
            Ok(())
        }

        fn hypercall(&mut self) -> Result<HypercallMsrs, FakeError> {
            self.check("get")?;
            Ok(HypercallMsrs {
                guest_os_id: self.hypercall.0,
                hypercall: self.hypercall.1,
            })
        }

        fn set_hypercall(&mut self, value: &HypercallMsrs) -> Result<(), FakeError> {
            self.check("set")?;
            self.hypercall = (value.guest_os_id, value.hypercall);
            Ok(())
        }

        fn reftime(&mut self) -> Result<ReferenceTime, FakeError> {
            self.check("get")?;
            let value = self.reftime;
            if !self.caps.can_freeze_time {
                self.reftime += 1;
            }
            Ok(ReferenceTime { value })
        }

        fn set_reftime(&mut self, value: &ReferenceTime) -> Result<(), FakeError> {
            self.check("set")?;
            self.reftime = value.value;
            Ok(())
        }

        fn reference_tsc_page(&mut self) -> Result<ReferenceTscPage, FakeError> {
            self.check("get")?;
            Ok(ReferenceTscPage {
                tsc_reference_page: self.tsc_page,
            })
        }

        fn set_reference_tsc_page(&mut self, value: &ReferenceTscPage) -> Result<(), FakeError> {
            self.check("set")?;
            self.tsc_page = value.tsc_reference_page;
            Ok(())
        }
    }

    fn bsp() -> X86VpInfo {
        X86VpInfo {
            base: VpInfo {
                vp_index: VpIndex::BSP,
                vnode: 0,
            },
            apic_id: 0,
        }
    }

    // Test: Save the state of partitions with and without the Hyper-V
    //       interface and the reference TSC page.
    // Expected: Saved state holds the current value of each element the
    //       partition has and nothing for the elements it lacks.
    #[test]
    fn save_all_includes_only_present_state() {
        let saved = FakeVm::new(true, false, true).save_all().unwrap();
        assert_eq!(
            saved,
            VmSavedState {
                hypercall: Some(HypercallMsrs {
                    guest_os_id: 0x8100_0000_0000_0001,
                    hypercall: 0x1_0001,
                }),
                reftime: Some(ReferenceTime { value: 0x1234 }),
                reference_tsc_page: None,
            }
        );

        let saved = FakeVm::new(true, true, true).save_all().unwrap();
        assert_eq!(
            saved.reference_tsc_page,
            Some(ReferenceTscPage {
                tsc_reference_page: 0x2_0001
            })
        );

        assert_eq!(
            FakeVm::new(false, false, true).save_all().unwrap(),
            VmSavedState::default()
        );
    }

    // Test: Restore saved state into a partition whose reference time keeps
    //       running (time can't be frozen), so it never reads back the value
    //       that was set.
    // Expected: Every saved element is set, the changes are committed once,
    //       and the running reference time isn't treated as a mismatch.
    #[test]
    fn restore_all_sets_state_and_commits_once() {
        let saved = FakeVm::new(true, true, true).save_all().unwrap();
        let mut vm = FakeVm::new(true, true, false);
        vm.hypercall = (0, 0);
        vm.reftime = 0;
        vm.tsc_page = 0;

        vm.restore_all(&saved).unwrap();

        assert_eq!(vm.hypercall, (0x8100_0000_0000_0001, 0x1_0001));
        assert_eq!(vm.reftime, 0x1234);
        assert_eq!(vm.tsc_page, 0x2_0001);
        assert_eq!(vm.commits, 1);
    }

    // Test: Reset a partition with the Hyper-V interface and reference TSC
    //       page after the guest has set all three elements.
    // Expected: All three elements return to zero, as after machine reset, and
    //       the built-in reset check passes.
    #[test]
    fn reset_all_clears_present_state() {
        let mut vm = FakeVm::new(true, true, true);

        vm.reset_all(&bsp()).unwrap();

        assert_eq!((vm.hypercall, vm.reftime, vm.tsc_page), ((0, 0), 0, 0));
        vm.check_reset_all(&bsp());
    }

    // Test: Save, restore, and reset when reading state, writing state, or
    //       committing fails.
    // Expected: Each failure is returned as an error naming the phase and the
    //       element (or the commit) that failed.
    #[test]
    fn state_errors_name_the_failing_phase() {
        let phase = |failing, op: fn(&mut FakeVm) -> Result<(), StateError<FakeError>>| {
            let mut vm = FakeVm::new(true, false, true);
            vm.failing = Some(failing);
            op(&mut vm).unwrap_err().phase
        };

        assert_eq!(
            phase("get", |vm| vm.save_all().map(drop)),
            "save \"hypercall\""
        );
        assert_eq!(
            phase("set", |vm| vm.restore_all(&VmSavedState {
                hypercall: Some(HypercallMsrs::default()),
                ..Default::default()
            })),
            "restore \"hypercall\""
        );
        assert_eq!(
            phase("set", |vm| vm.reset_all(&bsp())),
            "reset \"hypercall\""
        );
        assert_eq!(
            phase("commit", |vm| vm.restore_all(&VmSavedState::default())),
            "commit restore"
        );
    }

    // Test: Write a distinct value to each register of the hypercall MSRs,
    //       reference time, and reference TSC page through the register
    //       interface, then read them back.
    // Expected: Each register lands in the field its name refers to and reads
    //       back unchanged.
    #[test]
    fn vm_registers_round_trip_under_their_names() {
        let mut hypercall = HypercallMsrs::default();
        hypercall.set_values([0x11u64.into(), 0x22u64.into()].into_iter());
        assert_eq!(
            hypercall,
            HypercallMsrs {
                guest_os_id: 0x11,
                hypercall: 0x22
            }
        );
        assert_eq!(
            hypercall.names(),
            &[HvX64RegisterName::GuestOsId, HvX64RegisterName::Hypercall]
        );
        let mut values = [HvRegisterValue::from(0u64); 2];
        hypercall.get_values(values.iter_mut());
        assert_eq!(values.map(|v| v.as_u64()), [0x11, 0x22]);

        let mut time = ReferenceTime::default();
        time.set_values([0x33u64.into()].into_iter());
        assert_eq!(time.names(), &[HvX64RegisterName::TimeRefCount]);
        let mut value = [HvRegisterValue::from(0u64)];
        time.get_values(value.iter_mut());
        assert_eq!(value[0].as_u64(), 0x33);

        let mut page = ReferenceTscPage::default();
        page.set_values([0x44u64.into()].into_iter());
        assert_eq!(page.names(), &[HvX64RegisterName::ReferenceTsc]);
        page.get_values(value.iter_mut());
        assert_eq!(value[0].as_u64(), 0x44);
    }
}
