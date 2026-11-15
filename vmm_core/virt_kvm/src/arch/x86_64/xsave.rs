// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Restores of a VP's XSAVE area.
//!
//! An area marks a component that is in its init state with a clear XSTATE_BV
//! bit, and the reset area marks every component so. But `KVM_SET_XSAVE` does
//! not always read a clear bit as init (`fpu_copy_uabi_to_guest_fpstate` and
//! `copy_uabi_to_xstate`, in the kernel's `arch/x86/kernel/fpu/`), and what it
//! keeps depends on the host:
//!
//! - PKRU: where the host has PKU, KVM keeps the guest's PKRU in its vCPU,
//!   apart from the guest's FPU state, and writes it only from an area whose
//!   XSTATE_BV bit 9, the PKRU component, is set, and leaves it as it was
//!   otherwise.
//! - MXCSR: KVM writes it only from an area whose XSTATE_BV has x87, SSE, or
//!   AVX, but a kernel with neither XSAVES nor XSAVEC runs the guest with the
//!   standard form of XRSTOR, which loads MXCSR whatever XSTATE_BV says, so an
//!   area with none of them leaves the guest's earlier MXCSR.
//! - x87 and SSE: a kernel without XSAVE copies the whole 512-byte legacy area
//!   whatever XSTATE_BV says, so the bytes of a component that an area marks
//!   init, which only a crafted snapshot holds, become live state.
//!
//! So a VM reset, or a restore onto a VP that has run, leaves the guest's
//! earlier PKRU or MXCSR, and a crafted snapshot loads registers that it marks
//! init. A restore onto a new VP, whose state is init, is not affected by the
//! first two. [`restore`] writes the buffers that [`Buffers`] builds: the
//! area's legacy components that its XSTATE_BV leaves out are init bytes, and,
//! where KVM would keep PKRU or MXCSR, a seed that makes KVM write their init
//! values comes first, so that the area, which is always written last, finds
//! them in place.
//!
//! What a restore leaves in the VP is the area's own state, so a save right
//! after it reads the area back. A save after a later `KVM_RUN` can differ in
//! the PKRU component, whatever the restore wrote: the `pkru` module gives the
//! VP's thread a nonzero PKRU, which XSAVE always marks in use, so the save
//! sets XSTATE_BV bit 9 and reports the guest's PKRU, which is 0 after a reset,
//! the component's init value: the same state.

use crate::KvmError;
use std::mem::offset_of;
use std::ops::Range;
use virt::x86::X86PartitionCapabilities;
use virt::x86::XsaveCapabilities;
use virt::x86::vp::Xsave;
use virt::x86::vp::XsaveError;
use x86defs::xsave::Fxsave;
use x86defs::xsave::X86X_XSAVE_LEGACY_FEATURES;
use x86defs::xsave::XFEATURE_SSE;
use x86defs::xsave::XFEATURE_X87;
use x86defs::xsave::XFEATURE_YMM;
use x86defs::xsave::XSAVE_LEGACY_LEN;

/// The index of XSTATE_BV, the first word of the XSAVE header, among the 64-bit
/// words of an area. The header follows the legacy area.
const XSTATE_BV_WORD: usize = XSAVE_LEGACY_LEN / size_of::<u64>();

/// PKRU's bit of XSTATE_BV and of the XCR0 mask.
const XFEATURE_PKRU: u64 = 1 << 9;

/// The bytes of XSTATE_BV in a standard-format buffer.
const XSTATE_BV_BYTES: Range<usize> = XSAVE_LEGACY_LEN..XSAVE_LEGACY_LEN + size_of::<u64>();

/// The x87 state between FCW and MXCSR in the legacy area: FSW, FTW, FOP, FIP,
/// and FDP.
const X87_STATE_BYTES: Range<usize> = offset_of!(Fxsave, fsw)..offset_of!(Fxsave, mxcsr);

/// ST0-ST7 in the legacy area.
const X87_REGISTER_BYTES: Range<usize> = offset_of!(Fxsave, st)..offset_of!(Fxsave, xmm);

/// XMM0-XMM15 in the legacy area.
const XMM_BYTES: Range<usize> = offset_of!(Fxsave, xmm)..offset_of!(Fxsave, reserved2);

/// Restores `value` as the XSAVE state of `vp`, a VP of a partition with
/// `caps`.
///
/// Refuses an area that the VP cannot take, before it writes anything: one
/// whose XSTATE_BV the VP cannot keep, as `check_xstate_bv` describes, and one
/// that [`Xsave::write_standard`] cannot write in the standard format that KVM
/// takes, since it has too few words for what its header selects. KVM refuses a
/// buffer before it changes the VP, and takes the area whenever it took the
/// seed, whose XSTATE_BV has all of the area's bits, so a refused restore
/// leaves the VP's XSAVE state as it was.
pub(super) fn restore(
    vp: &kvm::Processor<'_>,
    caps: &X86PartitionCapabilities,
    value: &Xsave,
) -> Result<(), KvmError> {
    check_xstate_bv(value, &caps.xsave)?;
    Buffers::new(value, caps)?.write(|buffer| vp.set_xsave(buffer))?;
    Ok(())
}

/// The standard-format buffers that restoring an area writes with
/// `KVM_SET_XSAVE`, which make KVM read the area's clear XSTATE_BV bits as init
/// state on every host.
struct Buffers {
    /// Written first where KVM would keep the VP's PKRU or MXCSR through the
    /// area: the area with the components marked in use that make KVM write
    /// their init values, as [`seed_components`] says. The area then drops
    /// those bits again, so that the VP's XSAVE state, which a save reads, is
    /// the area's own, as a reset's save must equal the reset area (debug
    /// builds check it).
    seed: Option<[u8; 4096]>,
    /// The area itself, which is written last.
    area: [u8; 4096],
}

impl Buffers {
    /// Builds the buffers that restore `value`, an area of a partition with
    /// `caps`, or fails as [`Xsave::write_standard`] does for an area that it
    /// cannot write, before anything is written to a VP.
    fn new(value: &Xsave, caps: &X86PartitionCapabilities) -> Result<Self, XsaveError> {
        let mut area = [0; 4096];
        value.write_standard(&mut area, caps)?;
        init_cleared_legacy(&mut area);
        let xstate_bv = read_xstate_bv(&area);
        let seeded = seed_components(xstate_bv, &caps.xsave);
        let seed = (seeded != 0).then(|| {
            let mut seed = area;
            write_xstate_bv(&mut seed, xstate_bv | seeded);
            seed
        });
        Ok(Self { seed, area })
    }

    /// Gives the buffers to `set_xsave`, which writes one, in order, and stops
    /// at its first error.
    fn write<E>(&self, mut set_xsave: impl FnMut(&[u8; 4096]) -> Result<(), E>) -> Result<(), E> {
        if let Some(seed) = &self.seed {
            set_xsave(seed)?;
        }
        set_xsave(&self.area)
    }
}

/// Returns XSTATE_BV of the standard-format buffer `data`.
fn read_xstate_bv(data: &[u8; 4096]) -> u64 {
    let mut bytes = [0; size_of::<u64>()];
    bytes.copy_from_slice(&data[XSTATE_BV_BYTES]);
    u64::from_le_bytes(bytes)
}

/// Sets XSTATE_BV of the standard-format buffer `data`.
fn write_xstate_bv(data: &mut [u8; 4096], xstate_bv: u64) {
    data[XSTATE_BV_BYTES].copy_from_slice(&xstate_bv.to_le_bytes());
}

/// Makes the legacy components that the standard-format buffer `data`'s
/// XSTATE_BV leaves out hold their init state: without x87 (bit 0), FSW, FTW,
/// FOP, FIP, FDP, and ST0-ST7 are zero, and without SSE (bit 1), XMM0-XMM15
/// are. FCW and MXCSR already hold their init values there, 0x37f and 0x1f80,
/// as [`Xsave::fxsave`] gives them.
///
/// A kernel without XSAVE takes the whole legacy area whatever XSTATE_BV says,
/// so bytes of a component that the area leaves out, which only a crafted
/// snapshot has, would load as live state. A kernel with XSAVE never reads
/// them.
fn init_cleared_legacy(data: &mut [u8; 4096]) {
    let xstate_bv = read_xstate_bv(data);
    if xstate_bv & XFEATURE_X87 == 0 {
        data[X87_STATE_BYTES].fill(0);
        data[X87_REGISTER_BYTES].fill(0);
    }
    if xstate_bv & XFEATURE_SSE == 0 {
        data[XMM_BYTES].fill(0);
    }
}

/// Returns the components that the seed marks in use besides those of the area
/// whose XSTATE_BV is `xstate_bv`, which make KVM write the init values that it
/// would otherwise keep through the area:
///
/// - SSE, where XSTATE_BV has none of x87, SSE, and AVX. KVM then takes MXCSR
///   from the seed, which has it at 0x1f80, where [`Xsave::fxsave`] puts it, and
///   the XMM bytes zero, which [`init_cleared_legacy`] makes them: SSE's state
///   is its init state, and the seed changes no other state. A kernel with
///   neither XSAVES nor XSAVEC runs the guest with the standard form of
///   XRSTOR, which loads MXCSR whatever XSTATE_BV says, but KVM writes MXCSR
///   only from a buffer whose XSTATE_BV has x87, SSE, or AVX. So the area
///   alone, such as the reset area, would leave the guest's earlier MXCSR,
///   while a save reports 0x1f80. The area, which comes last, does not have
///   the bit, so the compacted forms of XRSTOR load their init MXCSR, which is
///   what they load for any area without SSE.
/// - PKRU, where the caps have it and XSTATE_BV leaves it out. KVM keeps a VP's
///   PKRU in its vCPU, apart from the guest's FPU state, and writes it only
///   from a buffer whose XSTATE_BV bit 9, the PKRU component, is set, so the
///   area alone, such as the reset area, would leave the VP's earlier PKRU,
///   which the guest runs with, while a save reports the component in its init
///   state. The seed's PKRU bytes are zero, its init value, since
///   `write_standard` writes a component only where its bit is set. The area,
///   which comes last, does not have the bit, and leaves the PKRU that the seed
///   wrote.
fn seed_components(xstate_bv: u64, caps: &XsaveCapabilities) -> u64 {
    let mut components = 0;
    if xstate_bv & (XFEATURE_X87 | XFEATURE_SSE | XFEATURE_YMM) == 0 {
        components |= XFEATURE_SSE;
    }
    if caps.features & XFEATURE_PKRU != 0 && xstate_bv & XFEATURE_PKRU == 0 {
        components |= XFEATURE_PKRU;
    }
    components
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
    use super::Buffers;
    use super::XSTATE_BV_WORD;
    use super::check_xstate_bv;
    use super::init_cleared_legacy;
    use super::read_xstate_bv;
    use super::seed_components;
    use super::write_xstate_bv;
    use test_with_tracing::test;
    use virt::VpIndex;
    use virt::state::StateElement;
    use virt::x86::X86PartitionCapabilities;
    use virt::x86::XsaveCapabilities;
    use virt::x86::XsaveFeature;
    use virt::x86::vp::Xsave;
    use virt::x86::vp::XsaveError;
    use vm_topology::processor::ProcessorTopology;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use vm_topology::processor::x86::X86Topology;
    use x86defs::xsave::DEFAULT_MXCSR;
    use x86defs::xsave::INIT_FCW;
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

    /// 4096 non-zero bytes, so that no component is in its init state.
    fn garbage(seed: u8) -> [u8; 4096] {
        let mut data = [0; 4096];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(seed) | 1;
        }
        data
    }

    /// `data` with the XSTATE_BV `xstate_bv`, whose bytes follow the legacy area.
    fn with_xstate_bv(mut data: [u8; 4096], xstate_bv: u64) -> [u8; 4096] {
        data[512..520].copy_from_slice(&xstate_bv.to_le_bytes());
        data
    }

    #[test]
    fn init_cleared_legacy_zeroes_the_legacy_components_that_xstate_bv_leaves_out() {
        for xstate_bv in [0, X87, SSE, X87 | SSE, AVX, PKRU | 1 << 63, !0] {
            let before = with_xstate_bv(garbage(7), xstate_bv);
            let mut data = before;
            init_cleared_legacy(&mut data);
            for (i, (&got, &was)) in data.iter().zip(&before).enumerate() {
                // FSW through FDP and ST0-ST7, then XMM0-XMM15, in the legacy
                // area that FXSAVE lays out. FCW, MXCSR, and the rest stay.
                let x87 = (2..24).contains(&i) || (32..160).contains(&i);
                let xmm = (160..416).contains(&i);
                let zeroed = (x87 && xstate_bv & X87 == 0) || (xmm && xstate_bv & SSE == 0);
                assert_eq!(
                    got,
                    if zeroed { 0 } else { was },
                    "XSTATE_BV {xstate_bv:#x}, byte {i}"
                );
            }
        }
    }

    /// The caps of a CPU with AVX and PKRU, laid out as Intel CPUs do, which
    /// put PKRU at 2688 whatever else they have.
    fn pkru_caps() -> XsaveCapabilities {
        let mut caps = milan_caps();
        caps.features |= PKRU;
        caps.feature_info[9] = XsaveFeature {
            offset: 2688,
            len: 8,
            align: false,
        };
        caps.standard_len = 2696;
        caps.compact_len = 840;
        caps
    }

    #[test]
    fn seed_components_are_sse_without_x87_sse_and_avx_and_pkru_where_the_caps_have_it() {
        for low in 0..8 {
            for high in [0, AVX512_OPMASK, PKRU, 1 << 63] {
                let xstate_bv = low | high;
                // SSE, so that KVM writes MXCSR, where XSTATE_BV has none of
                // the components that have it.
                let sse = if low == 0 { SSE } else { 0 };
                // PKRU, so that KVM writes PKRU, where the caps have it and
                // XSTATE_BV does not.
                let pkru = if high & PKRU == 0 { PKRU } else { 0 };
                assert_eq!(
                    seed_components(xstate_bv, &milan_caps()),
                    sse,
                    "XSTATE_BV {xstate_bv:#x}"
                );
                assert_eq!(
                    seed_components(xstate_bv, &pkru_caps()),
                    sse | pkru,
                    "XSTATE_BV {xstate_bv:#x}, with PKRU in the caps"
                );
            }
        }
    }

    // The tests below restore into a model of a host's kernel, since the hosts
    // that keep state in `KVM_SET_XSAVE` are not at hand.

    /// A host's kernel, in what it does with the guest's FPU state and PKRU.
    #[derive(Clone, Copy, Debug)]
    struct Host {
        /// The kernel switches FPU states with XSAVE, XSAVEOPT, XSAVEC, or
        /// XSAVES, and not with FXSAVE.
        xsave: bool,
        /// The kernel uses XSAVEC or XSAVES, whose area is compacted.
        compacted: bool,
        /// The CPU has PKU, which the kernel enabled.
        pku: bool,
    }

    /// The hosts of every combination that the kernel can run with.
    const HOSTS: [Host; 5] = [
        Host {
            xsave: true,
            compacted: true,
            pku: false,
        },
        Host {
            xsave: true,
            compacted: true,
            pku: true,
        },
        // Intel before Skylake, AMD before Zen, an L1 host whose hypervisor
        // hides XSAVEC and XSAVES, or a kernel booted without them.
        Host {
            xsave: true,
            compacted: false,
            pku: false,
        },
        Host {
            xsave: true,
            compacted: false,
            pku: true,
        },
        // A host without XSAVE.
        Host {
            xsave: false,
            compacted: false,
            pku: false,
        },
    ];

    /// The valid bits of MXCSR on the modeled CPUs.
    const MXCSR_MASK: u32 = 0xffff;
    /// The standard-format offsets of AVX and PKRU on the modeled CPUs.
    const AVX_OFFSET: usize = 576;
    const PKRU_OFFSET: usize = 2688;

    impl Host {
        /// The user components that KVM supports on the host.
        fn xcr0(&self) -> u64 {
            match (self.xsave, self.pku) {
                (false, _) => X87 | SSE,
                (true, false) => X87 | SSE | AVX,
                (true, true) => X87 | SSE | AVX | PKRU,
            }
        }

        /// The capabilities of a partition on the host, whose CPUID enumerates
        /// what KVM supports.
        fn caps(&self) -> X86PartitionCapabilities {
            let xcr0 = self.xcr0();
            X86PartitionCapabilities::from_cpuid(&topology(), &mut |function, index| {
                match (function, index) {
                    (0, _) => [0xd, 0, 0, 0],
                    (1, _) => [0, 0, (1 << 21) | (u32::from(self.xsave) << 26), 0],
                    (0xd, 0) => [xcr0 as u32, 0, u32::MAX, (xcr0 >> 32) as u32],
                    (0xd, 2) => [256, AVX_OFFSET as u32, 0, 0],
                    (0xd, 9) => [8, PKRU_OFFSET as u32, 0, 0],
                    _ => [0; 4],
                }
            })
            .unwrap()
        }
    }

    fn topology() -> ProcessorTopology<X86Topology> {
        TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(1)
            .unwrap()
    }

    /// The legacy area that a CPU in its init state holds. XRSTOR and FXRSTOR
    /// ignore the MXCSR mask, which is zero here.
    fn init_legacy() -> [u8; 512] {
        let mut legacy = [0; 512];
        legacy[..2].copy_from_slice(&INIT_FCW.to_le_bytes());
        legacy[24..28].copy_from_slice(&DEFAULT_MXCSR.to_le_bytes());
        legacy
    }

    /// `KVM_SET_XSAVE` refused the buffer.
    #[derive(Debug)]
    struct Refused;

    /// A VP's guest state on a host, which `KVM_SET_XSAVE` and `KVM_GET_XSAVE`
    /// handle as Linux 5.16 and later do, in `fpu_copy_uabi_to_guest_fpstate`,
    /// `copy_uabi_to_xstate`, and `__copy_xstate_to_uabi_buf` of the kernel's
    /// `arch/x86/kernel/fpu/`.
    #[derive(Clone)]
    struct Vp {
        host: Host,
        /// KVM's `vcpu->arch.pkru`, the guest's PKRU on a host with PKU.
        pkru: u32,
        /// The legacy area of the guest's FPU state, and its XSTATE_BV.
        legacy: [u8; 512],
        xfeatures: u64,
        /// AVX's component of the guest's FPU state.
        avx: [u8; 256],
    }

    impl Vp {
        /// A new VP, whose guest state is init.
        fn new(host: Host) -> Self {
            Self {
                host,
                pkru: 0,
                legacy: init_legacy(),
                xfeatures: 0,
                avx: [0; 256],
            }
        }

        /// A VP whose guest has run, and left every component that the host
        /// holds in use with values that no init state has, such as an MXCSR
        /// that rounds toward zero.
        fn dirty(host: Host) -> Self {
            let mut buffer = garbage(5);
            buffer[512..576].fill(0);
            write_xstate_bv(&mut buffer, host.xcr0());
            buffer[24..28].copy_from_slice(&0x7f80u32.to_le_bytes());
            let mut vp = Self::new(host);
            vp.set_xsave(&buffer).unwrap();
            assert_ne!(vp.running(), (init_legacy(), 0));
            vp
        }

        /// `KVM_SET_XSAVE`.
        fn set_xsave(&mut self, buffer: &[u8; 4096]) -> Result<(), Refused> {
            let xstate_bv = read_xstate_bv(buffer);
            let mxcsr = u32::from_le_bytes(buffer[24..28].try_into().unwrap());
            if !self.host.xsave {
                // The FXSAVE image, whatever XSTATE_BV says about its
                // components.
                if xstate_bv & !(X87 | SSE) != 0 || mxcsr & !MXCSR_MASK != 0 {
                    return Err(Refused);
                }
                self.legacy.copy_from_slice(&buffer[..512]);
                return Ok(());
            }
            // The header is the user's: in the standard format, so that
            // XCOMP_BV and the reserved words are zero.
            if xstate_bv & !self.host.xcr0() != 0 || buffer[520..576].iter().any(|&b| b != 0) {
                return Err(Refused);
            }
            if xstate_bv & (X87 | SSE | AVX) != 0 && mxcsr & !MXCSR_MASK != 0 {
                return Err(Refused);
            }
            // The components that XSTATE_BV selects are copied, and MXCSR with
            // them: with x87, which covers it, or, for SSE and AVX, alone. The
            // fpstate keeps the others as they were.
            if xstate_bv & X87 != 0 {
                self.legacy[..160].copy_from_slice(&buffer[..160]);
            } else if xstate_bv & (SSE | AVX) != 0 {
                self.legacy[24..32].copy_from_slice(&buffer[24..32]);
            }
            if xstate_bv & SSE != 0 {
                self.legacy[160..416].copy_from_slice(&buffer[160..416]);
            }
            if xstate_bv & AVX != 0 {
                self.avx
                    .copy_from_slice(&buffer[AVX_OFFSET..AVX_OFFSET + 256]);
            }
            // KVM's odd ABI: PKRU is written only with its bit.
            if xstate_bv & PKRU != 0 {
                self.pkru =
                    u32::from_le_bytes(buffer[PKRU_OFFSET..PKRU_OFFSET + 4].try_into().unwrap());
            }
            self.xfeatures = xstate_bv;
            Ok(())
        }

        /// `KVM_GET_XSAVE`.
        fn get_xsave(&self) -> [u8; 4096] {
            let mut buffer = [0; 4096];
            if !self.host.xsave {
                buffer[..512].copy_from_slice(&self.legacy);
                write_xstate_bv(&mut buffer, X87 | SSE);
                return buffer;
            }
            // A component that is not in use is reported in its init state.
            let init = init_legacy();
            let xstate_bv = self.xfeatures;
            let from = |in_use: bool| if in_use { &self.legacy } else { &init };
            buffer[..24].copy_from_slice(&from(xstate_bv & X87 != 0)[..24]);
            buffer[24..28].copy_from_slice(&from(xstate_bv & (SSE | AVX) != 0)[24..28]);
            buffer[32..160].copy_from_slice(&from(xstate_bv & X87 != 0)[32..160]);
            buffer[160..416].copy_from_slice(&from(xstate_bv & SSE != 0)[160..416]);
            write_xstate_bv(&mut buffer, xstate_bv);
            if xstate_bv & AVX != 0 {
                buffer[AVX_OFFSET..AVX_OFFSET + 256].copy_from_slice(&self.avx);
            }
            if xstate_bv & PKRU != 0 {
                buffer[PKRU_OFFSET..PKRU_OFFSET + 4].copy_from_slice(&self.pkru.to_le_bytes());
            }
            buffer
        }

        /// The legacy area and the PKRU that the guest runs with once the VP
        /// enters it, which the kernel loads from the VP's state.
        fn running(&self) -> ([u8; 512], u32) {
            let mut image = init_legacy();
            // FXRSTOR loads the whole image, XRSTOR the components in use.
            let loads = |in_use: u64| !self.host.xsave || self.xfeatures & in_use != 0;
            if loads(X87) {
                image[..24].copy_from_slice(&self.legacy[..24]);
                image[32..160].copy_from_slice(&self.legacy[32..160]);
            }
            if loads(SSE) {
                image[160..416].copy_from_slice(&self.legacy[160..416]);
            }
            // The standard form of XRSTOR loads MXCSR whatever XSTATE_BV says,
            // while the compacted forms load it only with SSE in use.
            if !self.host.xsave || !self.host.compacted || loads(SSE) {
                image[24..28].copy_from_slice(&self.legacy[24..28]);
            }
            (image, self.pkru)
        }
    }

    /// A compact area of the partition with `caps` whose XSTATE_BV is
    /// `xstate_bv`, and whose every other word is non-zero, those of the
    /// components that XSTATE_BV leaves out as a crafted snapshot's, but for
    /// the bytes that a save leaves zero: MXCSR's mask, the legacy area's
    /// reserved bytes, the header's reserved words, and PKRU's padding. MXCSR
    /// rounds toward zero where SSE holds it, is the init value where only AVX
    /// covers it, which a save of a compacted host never has otherwise, and is
    /// invalid where no component of XSTATE_BV holds it.
    fn crafted_area(caps: &X86PartitionCapabilities, xstate_bv: u64) -> Xsave {
        let words = caps.xsave.compact_len as usize / 8;
        let mut data: Vec<u64> = (0..words as u64)
            .map(|word| 0x0101_0101_0101_0101 * (word % 251 + 1))
            .collect();
        // FXSAVE's reserved byte 5, and its reserved bytes 416 to 463.
        data[0] &= !(0xff << 40);
        data[52..58].fill(0);
        data[3] = if xstate_bv & SSE != 0 {
            0x7f80
        } else if xstate_bv & AVX != 0 {
            0x1f80
        } else {
            0xffff_ffff
        };
        data[XSTATE_BV_WORD] = xstate_bv;
        data[XSTATE_BV_WORD + 1] = XCOMP_COMPRESSED | caps.xsave.features;
        data[XSTATE_BV_WORD + 2..XSTATE_BV_WORD + 8].fill(0);
        // PKRU follows AVX in the compact format.
        if caps.xsave.features & PKRU != 0 {
            data[(AVX_OFFSET + 256) / 8] &= u64::from(u32::MAX);
        }
        Xsave { data }
    }

    /// What `area`, which `crafted_area` made, says that the guest runs with,
    /// by the architecture: a component whose XSTATE_BV bit is clear is in its
    /// init state, and MXCSR belongs to the SSE state, which AVX covers too.
    fn meaning(area: &Xsave) -> ([u8; 512], u32) {
        let xstate_bv = area.data[XSTATE_BV_WORD];
        let bytes = area.compact();
        let mut image = init_legacy();
        if xstate_bv & X87 != 0 {
            image[..24].copy_from_slice(&bytes[..24]);
            image[32..160].copy_from_slice(&bytes[32..160]);
        }
        if xstate_bv & SSE != 0 {
            image[160..416].copy_from_slice(&bytes[160..416]);
        }
        if xstate_bv & (SSE | AVX) != 0 {
            image[24..28].copy_from_slice(&bytes[24..28]);
        }
        // PKRU follows AVX in the compact format.
        let pkru_offset = AVX_OFFSET + 256;
        let pkru = if xstate_bv & PKRU != 0 {
            u32::from_le_bytes(bytes[pkru_offset..pkru_offset + 4].try_into().unwrap())
        } else {
            0
        };
        (image, pkru)
    }

    /// Restores `area` into `vp` as `restore` does.
    fn restore_into(vp: &mut Vp, caps: &X86PartitionCapabilities, area: &Xsave) {
        Buffers::new(area, caps)
            .unwrap()
            .write(|buffer| vp.set_xsave(buffer))
            .unwrap();
    }

    /// Restores `area` into `vp` as `restore` did before it made KVM read the
    /// area's clear bits as init state: `write_standard`'s buffer as it is.
    fn restore_as_is(vp: &mut Vp, caps: &X86PartitionCapabilities, area: &Xsave) {
        let mut buffer = [0; 4096];
        area.write_standard(&mut buffer, caps).unwrap();
        vp.set_xsave(&buffer).unwrap();
    }

    #[test]
    fn a_reset_gives_every_host_the_init_state() {
        for host in HOSTS {
            let caps = host.caps();
            let reset = Xsave::at_reset(&caps, &topology().vp_arch(VpIndex::BSP));

            let mut vp = Vp::dirty(host);
            restore_into(&mut vp, &caps, &reset);
            assert_eq!(vp.running(), (init_legacy(), 0), "{host:?}");
            // A save then reads back the reset area, which debug builds check
            // after a reset.
            assert!(
                Xsave::from_standard(&vp.get_xsave(), &caps) == reset,
                "{host:?}: a save differs from the reset area"
            );

            // Restoring the buffer as it is leaves the guest's PKRU where the
            // host has PKU, and its MXCSR where the kernel uses neither XSAVEC
            // nor XSAVES. The reset area holds no stale bytes for a host
            // without XSAVE to load.
            let mut vp = Vp::dirty(host);
            restore_as_is(&mut vp, &caps, &reset);
            assert_eq!(
                vp.running() != (init_legacy(), 0),
                host.pku || (host.xsave && !host.compacted),
                "{host:?}"
            );
        }
    }

    #[test]
    fn a_reset_restores_init_components_as_buffers_that_say_so() {
        let host = Host {
            xsave: true,
            compacted: false,
            pku: true,
        };
        let caps = host.caps();
        let vp_info = topology().vp_arch(VpIndex::BSP);
        let buffers = Buffers::new(&Xsave::at_reset(&caps, &vp_info), &caps).unwrap();

        // The area holds init state: FCW and MXCSR hold their init values, and
        // nothing is marked in use.
        let mut want = [0; 4096];
        want[..2].copy_from_slice(&INIT_FCW.to_le_bytes());
        want[24..28].copy_from_slice(&DEFAULT_MXCSR.to_le_bytes());
        assert!(buffers.area == want, "the area's buffer");

        // The seed before it marks SSE and PKRU in use with their init state,
        // so that KVM writes MXCSR and PKRU.
        want[512] = SSE as u8;
        want[513] = (PKRU >> 8) as u8;
        assert!(buffers.seed == Some(want), "the seed");

        // Where the host has no PKRU, the seed marks SSE only.
        let host = Host { pku: false, ..host };
        let caps = host.caps();
        want[513] = 0;
        assert!(
            Buffers::new(&Xsave::at_reset(&caps, &vp_info), &caps)
                .unwrap()
                .seed
                == Some(want),
            "the seed without PKRU"
        );

        // An area that has x87, SSE, and AVX leaves nothing for a seed to
        // write.
        let area = crafted_area(&caps, X87 | SSE | AVX);
        assert!(Buffers::new(&area, &caps).unwrap().seed.is_none());
    }

    #[test]
    fn a_short_or_malformed_area_gets_no_buffers() {
        for host in HOSTS {
            let caps = host.caps();
            // An area with too few words for its legacy area and header, from
            // none to one short of them, which a restore refuses before it
            // writes a buffer.
            for words in 0..72 {
                let area = Xsave {
                    data: vec![0; words],
                };
                assert_eq!(
                    Buffers::new(&area, &caps).err(),
                    Some(XsaveError::AreaTooShort { words }),
                    "{host:?}, {words} words"
                );
            }
            if host.xcr0() & AVX == 0 {
                continue;
            }
            // An area whose header selects AVX, which the header puts right
            // after itself, and that ends before AVX's 256 bytes do: the
            // legacy area and the header alone, and one a word short.
            let crafted = crafted_area(&caps, AVX);
            for words in [72, 103] {
                let area = Xsave {
                    data: crafted.data[..words].to_vec(),
                };
                assert_eq!(
                    Buffers::new(&area, &caps).err(),
                    Some(XsaveError::ComponentPastArea {
                        component: 2,
                        words
                    }),
                    "{host:?}, {words} words"
                );
            }
            // The area that ends where AVX does is written.
            let area = Xsave {
                data: crafted.data[..104].to_vec(),
            };
            Buffers::new(&area, &caps).unwrap();
        }
    }

    #[test]
    fn an_area_restores_as_the_state_that_it_means_on_every_host() {
        for host in HOSTS {
            let caps = host.caps();
            let mut kept_state = false;
            // Every combination of x87, SSE, AVX, and PKRU in XSTATE_BV that
            // the host holds.
            for bits in 0..16 {
                let xstate_bv = (bits & 7) | ((bits >> 3) << 9);
                if xstate_bv & !host.xcr0() != 0 {
                    continue;
                }
                let area = crafted_area(&caps, xstate_bv);
                let context = format!("{host:?}, XSTATE_BV {xstate_bv:#x}");

                let mut vp = Vp::dirty(host);
                restore_into(&mut vp, &caps, &area);
                assert_eq!(vp.running(), meaning(&area), "{context}");
                // A save reads back the area, as debug builds check after a
                // restore, which compare what `from_standard` makes of the
                // area's standard-format buffer: it holds no component that
                // XSTATE_BV leaves out.
                let mut as_is = [0; 4096];
                area.write_standard(&mut as_is, &caps).unwrap();
                assert!(
                    Xsave::from_standard(&vp.get_xsave(), &caps)
                        == Xsave::from_standard(&as_is, &caps),
                    "{context}: a save differs from the area"
                );

                let mut vp = Vp::dirty(host);
                restore_as_is(&mut vp, &caps, &area);
                kept_state |= vp.running() != meaning(&area);
            }
            // Without the conversions, a host with PKU, a kernel that uses
            // neither XSAVEC nor XSAVES, and a host without XSAVE each restore
            // some area wrongly, and the other host every area as it means.
            assert_eq!(kept_state, host.pku || !host.compacted, "{host:?}");
        }
    }
}

/// Tests against the host's KVM, which decides what a restored area keeps.
#[cfg(test)]
mod kvm_tests {
    use super::XFEATURE_PKRU;
    use super::XSTATE_BV_WORD;
    use super::restore;
    use crate::KvmError;
    use crate::arch::x86_64::test_guest::GuestRam;
    use kvm::Exit;
    use kvm::KVM_CPUID_FLAG_SIGNIFCANT_INDEX;
    use kvm::Kvm;
    use kvm::Partition;
    use kvm::Processor;
    use kvm::VmType;
    use kvm::VpRunner;
    use kvm::kvm_cpuid_entry2;
    use kvm::kvm_regs;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    use test_with_tracing::test;
    use virt::VpIndex;
    use virt::state::StateElement;
    use virt::x86::X86PartitionCapabilities;
    use virt::x86::vp::Xsave;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use x86defs::cpuid::CpuidFunction;
    use x86defs::xsave::DEFAULT_MXCSR;
    use x86defs::xsave::INIT_FCW;
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
                area.write_standard(&mut data, &caps).unwrap();
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

    const CODE_GPA: u64 = 0x1000;
    const OBSERVE_GPA: u64 = 0x2000;
    const DATA_GPA: u64 = 0x3000;
    const OUTPUT_GPA: u64 = 0x3100;
    const DONE_PORT: u16 = 0x80;
    /// An MXCSR that is not the init value: every exception masked, and rounding
    /// toward zero.
    const DIRTY_MXCSR: u32 = 0x7f80;
    /// An x87 control word that is not the init value: 53-bit precision.
    const DIRTY_FCW: u16 = 0x27f;
    /// A PKRU that is not the init value, 0: Linux's default for a new task.
    const DIRTY_PKRU: u32 = 0x5555_5554;

    /// Runs the guest until it writes to `DONE_PORT`, and completes the write.
    fn run_until_done(partition: &Partition, runner: &mut VpRunner<'_>) {
        let timed_out = &AtomicBool::new(false);
        std::thread::scope(|s| {
            let (done, wait_done) = mpsc::channel::<()>();
            s.spawn(move || {
                // Kick the VP out of KVM_RUN if the guest gets stuck.
                if let Err(mpsc::RecvTimeoutError::Timeout) =
                    wait_done.recv_timeout(Duration::from_secs(10))
                {
                    timed_out.store(true, Ordering::SeqCst);
                    partition.vp(0).force_exit();
                }
            });
            loop {
                match runner.run().unwrap() {
                    Exit::IoOut { port, .. } => {
                        assert_eq!(port, DONE_PORT);
                        break;
                    }
                    Exit::Interrupted => {
                        assert!(
                            !timed_out.load(Ordering::SeqCst),
                            "timed out waiting for the guest"
                        );
                    }
                    exit => panic!("unexpected exit: {exit:?}"),
                }
            }
            drop(done);
        });
        assert!(matches!(runner.complete_exit().unwrap(), Exit::Interrupted));
    }

    /// After a reset, a guest finds its x87 and SSE registers, and its PKRU
    /// where the host has PKU, in their init state. The guest sets MXCSR, the
    /// x87 control word, an x87 register, XMM0, and PKRU, and reads them back;
    /// the reset area is restored into the VP; and the guest reads them back
    /// again. A save reports the init state on every host, but a host whose
    /// kernel uses neither XSAVEC nor XSAVES runs the guest with its earlier
    /// MXCSR, and a host with PKU with its earlier PKRU, where a restore does
    /// not write them, so only a guest tells.
    ///
    /// The PKRU part runs only on a host with PKU.
    #[test]
    #[ignore = "requires /dev/kvm"]
    fn a_reset_gives_the_guest_the_init_state() {
        kvm::init();
        let kvm = Kvm::new().unwrap();
        // The partition drops KVM's own leaves.
        let cpuid: Vec<_> = kvm
            .supported_cpuid()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.function & 0xf000_0000 != 0x4000_0000)
            .collect();
        let caps = caps(&cpuid);
        let pku = caps.xsave.features & XFEATURE_PKRU != 0;
        let vp_info = TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(1)
            .unwrap()
            .vp_arch(VpIndex::BSP);

        // Real-mode code that sets what is to be reset, and code that reads it
        // back, each ending with a write to `DONE_PORT`.
        let mut set = vec![
            0x0f, 0xae, 0x16, 0x00, 0x30, // ldmxcsr [DATA_GPA]
            0xd9, 0x2e, 0x04, 0x30, // fldcw [DATA_GPA + 4]
            0x0f, 0x10, 0x06, 0x10, 0x30, // movups xmm0, [DATA_GPA + 0x10]
            0xd9, 0xe8, // fld1
        ];
        let mut observe = vec![
            0x0f, 0xae, 0x1e, 0x00, 0x31, // stmxcsr [OUTPUT_GPA]
            0xd9, 0x3e, 0x08, 0x31, // fnstcw [OUTPUT_GPA + 8]
            0xdd, 0x3e, 0x10, 0x31, // fnstsw [OUTPUT_GPA + 0x10]
            0x0f, 0x11, 0x06, 0x20, 0x31, // movups [OUTPUT_GPA + 0x20], xmm0
        ];
        if pku {
            set.extend([0x66, 0xb8]); // mov eax, DIRTY_PKRU
            set.extend(DIRTY_PKRU.to_le_bytes());
            set.extend([
                0x66, 0x31, 0xc9, // xor ecx, ecx
                0x66, 0x31, 0xd2, // xor edx, edx
                0x0f, 0x01, 0xef, // wrpkru
            ]);
            observe.extend([
                0x66, 0x31, 0xc9, // xor ecx, ecx
                0x0f, 0x01, 0xee, // rdpkru
                0x66, 0xa3, 0x30, 0x31, // mov [OUTPUT_GPA + 0x30], eax
            ]);
        }
        for code in [&mut set, &mut observe] {
            code.extend([
                0xe6,
                DONE_PORT as u8, // out DONE_PORT, al
                0xf4,            // hlt
            ]);
        }

        let mut partition = kvm.new_vm(VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.add_vp(0).unwrap();
        let ram = GuestRam::new(&partition, 0x10000);
        ram.write(CODE_GPA, &set);
        ram.write(OBSERVE_GPA, &observe);
        ram.write(DATA_GPA, &DIRTY_MXCSR.to_le_bytes());
        ram.write(DATA_GPA + 4, &DIRTY_FCW.to_le_bytes());
        ram.write(DATA_GPA + 0x10, &[0xa5; 16]);

        let vp = partition.vp(0);
        vp.set_cpuid(&cpuid).unwrap();
        // A real-mode guest with SSE, and PKRU where the host has it, enabled.
        let mut sregs = vp.get_sregs().unwrap();
        sregs.cs.selector = 0;
        sregs.cs.base = 0;
        sregs.cr4 |= 1 << 9;
        if pku {
            sregs.cr4 |= 1 << 22;
        }
        vp.set_sregs(&sregs).unwrap();
        let regs_at = |rip| kvm_regs {
            rip,
            rflags: 2,
            ..Default::default()
        };
        let mut runner = vp.runner();
        // MXCSR, FCW, FSW, XMM0, and PKRU, as the guest read them.
        let observed = |vp: &Processor<'_>, runner: &mut VpRunner<'_>| {
            vp.set_regs(&regs_at(OBSERVE_GPA)).unwrap();
            run_until_done(&partition, runner);
            [0, 8, 0x10, 0x20, 0x28, 0x30].map(|offset| ram.read_u64(OUTPUT_GPA + offset))
        };

        vp.set_regs(&regs_at(CODE_GPA)).unwrap();
        run_until_done(&partition, &mut runner);
        let sixteen_a5s = 0xa5a5_a5a5_a5a5_a5a5;
        assert_eq!(
            observed(&vp, &mut runner),
            [
                u64::from(DIRTY_MXCSR),
                u64::from(DIRTY_FCW),
                // x87's stack top is 7, after a push.
                0x3800,
                sixteen_a5s,
                sixteen_a5s,
                if pku { u64::from(DIRTY_PKRU) } else { 0 },
            ],
            "before the reset"
        );

        let reset = Xsave::at_reset(&caps, &vp_info);
        restore(&vp, &caps, &reset).unwrap();
        // A save reads back the reset area, which debug builds check after a
        // reset.
        assert!(
            Xsave::from_standard(&read(&vp), &caps) == reset,
            "a save differs from the reset area"
        );
        assert_eq!(
            observed(&vp, &mut runner),
            [u64::from(DEFAULT_MXCSR), u64::from(INIT_FCW), 0, 0, 0, 0],
            "after the reset"
        );
    }
}
