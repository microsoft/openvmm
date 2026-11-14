// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Restores of a VP's XSAVE area.

use crate::KvmError;
use virt::x86::X86PartitionCapabilities;
use virt::x86::XsaveCapabilities;
use virt::x86::vp::Xsave;
use x86defs::xsave::X86X_XSAVE_LEGACY_FEATURES;
use x86defs::xsave::XSAVE_LEGACY_LEN;

/// The index of XSTATE_BV, the first word of the XSAVE header, among the 64-bit
/// words of an area. The header follows the legacy area.
const XSTATE_BV_WORD: usize = XSAVE_LEGACY_LEN / size_of::<u64>();

/// Restores `value` as the XSAVE state of `vp`, a VP of a partition with
/// `caps`.
///
/// Refuses an area whose XSTATE_BV the VP cannot keep, as `check_xstate_bv`
/// describes, before it writes anything, so that a refused restore leaves the
/// VP's XSAVE state as it was.
pub(super) fn restore(
    vp: &kvm::Processor<'_>,
    caps: &X86PartitionCapabilities,
    value: &Xsave,
) -> Result<(), KvmError> {
    check_xstate_bv(value, &caps.xsave)?;
    let mut data = [0; 4096];
    value.write_standard(&mut data, caps);
    vp.set_xsave(&data)?;
    Ok(())
}

/// Refuses an XSAVE area to restore whose XSTATE_BV selects a component that
/// the partition does not have: one outside the caps' features, the XCR0 mask
/// of the guest's CPUID leaf 0xD, other than x87 and SSE, which every VP has.
///
/// `KVM_SET_XSAVE` takes any user component that KVM supports, whether or not
/// the guest's CPUID enumerates it, but `KVM_GET_XSAVE` returns only the
/// components that it enumerates. A restored component outside the caps would
/// be loaded and marked in use, and the next save would drop it, so the VP
/// would not get the state that was saved. A save never holds one, since its
/// XSTATE_BV is within the guest's supported XCR0, which the caps' features
/// bound, and the reset area's XSTATE_BV is 0. Only a crafted snapshot, or one
/// saved with other caps, is refused.
///
/// Supervisor components are outside the caps' features too: KVM's standard
/// format holds user state only, and KVM refuses them.
///
/// An area too short to hold XSTATE_BV selects no component. Whether the area
/// is otherwise well formed is not checked here.
fn check_xstate_bv(value: &Xsave, caps: &XsaveCapabilities) -> Result<(), KvmError> {
    let xstate_bv = value.data.get(XSTATE_BV_WORD).copied().unwrap_or(0);
    if xstate_bv & !(caps.features | X86X_XSAVE_LEGACY_FEATURES) != 0 {
        return Err(KvmError::InvalidState(
            "XSTATE_BV selects an XSAVE component that the partition does not have",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::XSTATE_BV_WORD;
    use super::check_xstate_bv;
    use test_with_tracing::test;
    use virt::x86::XsaveCapabilities;
    use virt::x86::XsaveFeature;
    use virt::x86::vp::Xsave;
    use x86defs::xsave::XCOMP_COMPRESSED;

    const X87: u64 = 1 << 0;
    const SSE: u64 = 1 << 1;
    const AVX: u64 = 1 << 2;
    const AVX512_OPMASK: u64 = 1 << 5;
    const PKRU: u64 = 1 << 9;
    const CET_USER: u64 = 1 << 11;

    /// The caps of the pinned profile `amd.milan.v1`, whose XCR0 mask is x87,
    /// SSE, and AVX, without AVX-512, and whose XSS mask is empty.
    fn milan_caps() -> XsaveCapabilities {
        let mut feature_info = [XsaveFeature::default(); 63];
        feature_info[2] = XsaveFeature {
            offset: 576,
            len: 256,
            align: false,
        };
        XsaveCapabilities {
            features: X87 | SSE | AVX,
            supervisor_features: 0,
            standard_len: 832,
            compact_len: 832,
            feature_info,
        }
    }

    /// A compact XSAVE area of the Milan caps with XSTATE_BV `xstate_bv`.
    fn area(xstate_bv: u64) -> Xsave {
        let mut data = vec![0; 832 / 8];
        data[XSTATE_BV_WORD] = xstate_bv;
        data[XSTATE_BV_WORD + 1] = XCOMP_COMPRESSED | X87 | SSE | AVX;
        Xsave { data }
    }

    #[test]
    fn an_xstate_bv_within_the_caps_is_accepted() {
        let caps = milan_caps();
        for xstate_bv in [0, X87, SSE, X87 | SSE, AVX, X87 | SSE | AVX] {
            check_xstate_bv(&area(xstate_bv), &caps).unwrap();
        }
    }

    #[test]
    fn a_partition_without_xsave_still_takes_x87_and_sse() {
        // Its CPUID has no leaf 0xD, which leaves no features.
        let caps = XsaveCapabilities {
            features: 0,
            supervisor_features: 0,
            standard_len: 576,
            compact_len: 576,
            feature_info: [XsaveFeature::default(); 63],
        };
        for xstate_bv in [X87, SSE, X87 | SSE] {
            check_xstate_bv(&area(xstate_bv), &caps).unwrap();
        }
        assert!(check_xstate_bv(&area(AVX), &caps).is_err());
    }

    #[test]
    fn an_xstate_bv_selecting_a_component_the_caps_lack_is_refused() {
        let caps = milan_caps();
        for xstate_bv in [
            // The first AVX-512 component, alone and with the supported ones.
            AVX512_OPMASK,
            X87 | SSE | AVX | AVX512_OPMASK,
            // All the AVX-512 components.
            0xe0,
            PKRU,
            // A supervisor component, which KVM_SET_XSAVE does not take.
            CET_USER,
            // Bits that no component has.
            1 << 62,
            1 << 63,
        ] {
            assert!(
                check_xstate_bv(&area(xstate_bv), &caps).is_err(),
                "XSTATE_BV {xstate_bv:#x}"
            );
        }
    }

    #[test]
    fn a_component_is_accepted_only_where_the_caps_have_it() {
        let mut caps = milan_caps();
        let opmask = area(AVX512_OPMASK);
        assert!(check_xstate_bv(&opmask, &caps).is_err());
        caps.features |= AVX512_OPMASK;
        check_xstate_bv(&opmask, &caps).unwrap();
        // The supervisor features are outside the area that KVM takes.
        caps.supervisor_features = CET_USER;
        assert!(check_xstate_bv(&area(CET_USER), &caps).is_err());
    }

    #[test]
    fn an_area_too_short_to_hold_xstate_bv_selects_no_component() {
        let caps = milan_caps();
        for words in [0, 1, XSTATE_BV_WORD] {
            check_xstate_bv(
                &Xsave {
                    data: vec![u64::MAX; words],
                },
                &caps,
            )
            .unwrap();
        }
    }
}

/// Tests against the host's KVM, which decides what a restored area keeps.
#[cfg(test)]
mod kvm_tests {
    use super::XSTATE_BV_WORD;
    use super::restore;
    use crate::KvmError;
    use kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
    use kvm::Kvm;
    use kvm::Processor;
    use kvm::VmType;
    use kvm::kvm_cpuid_entry2;
    use test_with_tracing::test;
    use virt::x86::X86PartitionCapabilities;
    use virt::x86::vp::Xsave;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use x86defs::cpuid::CpuidFunction;
    use x86defs::xsave::XCOMP_COMPRESSED;

    /// What KVM does with an area of one component, when it restores it alone.
    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        /// KVM refuses the area.
        Refused,
        /// KVM takes the component, and the next save has it.
        Kept,
        /// KVM takes the component, and the next save drops it.
        Dropped,
    }

    fn is_xsave_leaf(entry: &kvm_cpuid_entry2) -> bool {
        entry.function == CpuidFunction::ExtendedStateEnumeration.0 && entry.index == 0
    }

    /// The partition capabilities of a VP with CPUID `cpuid`.
    fn caps(cpuid: &[kvm_cpuid_entry2]) -> X86PartitionCapabilities {
        let topology = TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(1)
            .unwrap();
        X86PartitionCapabilities::from_cpuid(&topology, &mut |function, index| {
            cpuid
                .iter()
                .find(|entry| {
                    entry.function == function
                        && (entry.flags & KVM_CPUID_FLAG_SIGNIFCANT_INDEX == 0
                            || entry.index == index)
                })
                .map_or([0; 4], |entry| [entry.eax, entry.ebx, entry.ecx, entry.edx])
        })
        .unwrap()
    }

    /// A compact area whose XSTATE_BV selects only `component`.
    fn one_component(caps: &X86PartitionCapabilities, component: u32) -> Xsave {
        let mut data = vec![0; caps.xsave.compact_len as usize / 8];
        data[XSTATE_BV_WORD] = 1 << component;
        data[XSTATE_BV_WORD + 1] =
            caps.xsave.features | caps.xsave.supervisor_features | XCOMP_COMPRESSED;
        Xsave { data }
    }

    fn read(vp: &Processor<'_>) -> [u8; 4096] {
        let mut state = [0; 4096];
        vp.get_xsave(&mut state).unwrap();
        state
    }

    /// Whether the VP's next save selects `component`.
    fn selects(vp: &Processor<'_>, caps: &X86PartitionCapabilities, component: u32) -> bool {
        Xsave::from_standard(&read(vp), caps).data[XSTATE_BV_WORD] & (1 << component) != 0
    }

    /// A restore that succeeds gives the VP the component of its area, and one
    /// that fails writes nothing: the restore succeeds for a component that
    /// KVM keeps, fails with the check's error for one that KVM takes and
    /// drops from the next save, and fails for one that KVM refuses itself.
    /// The guest's CPUID decides which components KVM keeps.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_restore_gives_the_vp_its_component_or_writes_nothing() {
        let kvm = Kvm::new().unwrap();
        // The partition drops KVM's own leaves.
        let supported: Vec<_> = kvm
            .supported_cpuid()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.function & 0xf000_0000 != 0x4000_0000)
            .collect();
        let leaf = supported.iter().find(|entry| is_xsave_leaf(entry)).unwrap();
        let host_xcr0 = u64::from(leaf.eax) | u64::from(leaf.edx) << 32;
        assert_eq!(host_xcr0 & 3, 3);

        // x87 state that no restored area has, which tells a write from none.
        let mut marker = [0; 4096];
        marker[..2].copy_from_slice(&0x27f_u16.to_le_bytes());
        marker[XSTATE_BV_WORD * 8] = 1;

        for xcr0 in [host_xcr0, host_xcr0 & 0x7, 3] {
            let mut cpuid = supported.clone();
            let leaf = cpuid.iter_mut().find(|entry| is_xsave_leaf(entry)).unwrap();
            leaf.eax = xcr0 as u32;
            leaf.edx = (xcr0 >> 32) as u32;
            let caps = caps(&cpuid);
            assert_eq!(caps.xsave.features, xcr0);

            let mut vm = kvm.new_vm(VmType::Default).unwrap();
            vm.add_vp(0).unwrap();
            let vp = vm.vp(0);
            vp.set_cpuid(&cpuid).unwrap();

            let mut outcomes = Vec::new();
            // The legacy components are left out: reading them normalizes
            // their state, and the restore never refuses them.
            for component in 2..64 {
                let area = one_component(&caps, component);

                // What KVM does with the area alone.
                let mut data = [0; 4096];
                area.write_standard(&mut data, &caps);
                let outcome = match vp.set_xsave(&data) {
                    Err(_) => Outcome::Refused,
                    Ok(()) if selects(&vp, &caps, component) => Outcome::Kept,
                    Ok(()) => Outcome::Dropped,
                };

                vp.set_xsave(&marker).unwrap();
                let before = read(&vp);
                let restored = restore(&vp, &caps, &area);
                let context = format!("XCR0 mask {xcr0:#x}, component {component}: {outcome:?}");
                match outcome {
                    Outcome::Kept => {
                        restored.expect(&context);
                        assert!(selects(&vp, &caps, component), "{context}");
                    }
                    Outcome::Dropped => {
                        assert!(
                            matches!(restored, Err(KvmError::InvalidState(_))),
                            "{context}: {restored:?}"
                        );
                        assert!(read(&vp) == before, "{context}, written");
                    }
                    Outcome::Refused => {
                        assert!(restored.is_err(), "{context}");
                        assert!(read(&vp) == before, "{context}, written");
                    }
                }
                outcomes.push((component, outcome));
            }
            tracing::info!(xcr0, ?outcomes, "restored components");
        }
    }
}
