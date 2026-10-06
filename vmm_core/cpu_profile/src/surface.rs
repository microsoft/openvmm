// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! What a backend supports, and the check that it supports a profile.

use crate::cpuid;
use crate::cpuid::CpuidEntry;
use crate::cpuid::EXTENDED_LEAF_BASE;
use crate::error::ProfileError;
use crate::error::ProfileErrorCode;
use crate::fingerprint::BackendFingerprint;
use crate::fingerprint::IA32_ARCH_CAPABILITIES;
use crate::hv_banks;
use crate::profile::CpuProfile;
use crate::profile::describe;
use crate::unlisted::unlisted_cpuid_error;
use crate::unlisted::unlisted_cpuid_violations;

/// The register names, for messages.
const REGISTERS: [&str; 4] = ["EAX", "EBX", "ECX", "EDX"];

/// The CPU surface a backend supports on this host, which it reports through
/// `supported_cpu_surface()` and which the CPU fingerprint records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCpuSurface {
    /// Every CPUID leaf and subleaf the backend can expose, with every
    /// feature it can expose set, normalized with [`cpuid::normalize`]. KVM
    /// reports `KVM_GET_SUPPORTED_CPUID`. A CPU fingerprint of MSHV or WHP
    /// records a probe partition with every available processor and XSAVE
    /// feature enabled, and their `supported_cpu_surface()` approximates it
    /// from the host's CPUID and processor features.
    pub cpuid: Vec<CpuidEntry>,
    /// How the backend presents the CPUID entries outside a profile's
    /// tables, and what `cpuid` holds there.
    pub presentation: CpuidPresentation,
    /// The widest guest physical address the backend supports, in bits.
    ///
    /// Support checks use this rather than the CPUID's `0x80000008:EAX[7:0]`:
    /// a surface built from the host's own CPUID carries the host's width,
    /// and a hypervisor may give guests another one. WHP, for one, reports
    /// its partition property, 46 bits on a host whose CPUID reports 42.
    pub physical_address_width: u8,
    /// The MSR values the backend can present to a guest.
    pub msrs: Vec<SupportedMsr>,
}

/// How a backend presents the CPUID entries that a VM's effective CPUID does
/// not list, and what a [`HostCpuSurface`] holds there, which decides whether
/// [`verify_support`] checks them (`E_CPU_UNLISTED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuidPresentation {
    /// A table backend, KVM: `KVM_SET_CPUID2` installs the effective CPUID as
    /// the guest's whole CPUID table, and KVM answers every entry it does not
    /// list from the table itself: zero, or the architecture's out-of-range
    /// and topology results, never host data. The surface lists what the
    /// backend can support, not what a guest reads, so there is nothing to
    /// check.
    Table,
    /// A pass-through backend, MSHV or WHP, which programs the effective
    /// CPUID over the hypervisor's own guest view: every entry the effective
    /// CPUID does not list reads that view unless the backend registers a
    /// result there. The surface is what the hypervisor presents to a guest,
    /// as a probe partition enumerates it for `--cpu-fingerprint`, so every
    /// non-zero entry outside the profile's tables fails the check.
    PassThroughGuestView,
    /// A pass-through backend whose surface comes from the host's own CPUID,
    /// as the cheap `supported_cpu_surface()` of MSHV and WHP builds it.
    /// Outside the profile's tables, the host's view is not the guest's: the
    /// root partition sees entries that guests never do, such as Intel PT's
    /// subleaf `0x14.1` on Skylake-SP. [`verify_support`] skips the check,
    /// which [`check_unlisted_cpuid`](crate::check_unlisted_cpuid) makes on VP
    /// 0's view at the host's
    /// [`unlisted_cpuid_candidates`](crate::unlisted_cpuid_candidates)
    /// instead.
    PassThroughHostView,
}

/// The values of one MSR that a backend can present to a guest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SupportedMsr {
    /// The MSR index.
    pub index: u32,
    /// The bits the backend can present set.
    pub supported: u64,
    /// The bits the backend can present clear, even where `supported` has
    /// them. KVM controls every bit; MSHV and WHP control the bits that their
    /// processor feature banks derive.
    pub controllable: u64,
}

impl HostCpuSurface {
    /// Returns the surface that a CPU fingerprint recorded.
    ///
    /// `IA32_ARCH_CAPABILITIES` comes from the KVM feature MSR, or, for the
    /// Hyper-V backends, from the processor feature banks. A KVM fingerprint
    /// is a [`CpuidPresentation::Table`] surface; any other records a probe
    /// partition, a [`CpuidPresentation::PassThroughGuestView`].
    pub fn from_fingerprint(backend: &BackendFingerprint) -> Self {
        let width = cpuid::lookup(&backend.cpuid, EXTENDED_LEAF_BASE + 8, 0).unwrap_or_default()[0];
        let mut msrs = Vec::new();
        if let Some(value) = backend.msrs.arch_capabilities {
            msrs.push(SupportedMsr {
                index: IA32_ARCH_CAPABILITIES,
                supported: value.0,
                controllable: !0,
            });
        } else if let Some(banks) = hv_banks::fingerprint_banks(backend) {
            msrs.push(hv_banks::arch_capabilities_msr(banks));
        }
        Self {
            cpuid: backend.cpuid.clone(),
            presentation: match backend.name.as_str() {
                "kvm" => CpuidPresentation::Table,
                _ => CpuidPresentation::PassThroughGuestView,
            },
            physical_address_width: (width & 0xff) as u8,
            msrs,
        }
    }

    /// Returns the supported value of `IA32_ARCH_CAPABILITIES` or another
    /// MSR.
    pub fn msr(&self, index: u32) -> Option<&SupportedMsr> {
        self.msrs.iter().find(|msr| msr.index == index)
    }
}

/// The CPUID time bits that the time ABI sets by policy on every backend,
/// whatever the backend reports: the hypervisor bit, ARAT (a virtual LAPIC
/// timer never stops), and the invariant TSC (backed by host qualification
/// instead). Support checks skip them.
pub const TIME_POLICY_BITS: [(u32, u32, usize, u32); 3] = [
    (0x1, 0, 2, 1 << 31),
    (0x6, 0, 0, 1 << 2),
    (EXTENDED_LEAF_BASE + 7, 0, 3, 1 << 8),
];

/// How a support check compares one CPUID register, and how derivation
/// combines it across hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegisterClass {
    /// Feature flags: every pinned set bit must be supported.
    Features,
    /// Numeric limits in the given bit fields (shift, width): the profile's
    /// may not exceed the backend's.
    Limits(&'static [(u32, u32)]),
    /// Values that do not constrain the host, or that other checks cover:
    /// the vendor and signature (generation check), AMD's copy of the
    /// signature in `0x80000001` EAX, which Intel CPUs leave zero,
    /// descriptive cache, TLB, and brand information (AMD's cache topology
    /// leaf `0x8000001D` included), and the XSAVE leaf (layout check).
    Informational,
}

pub(crate) fn register_class(leaf: u32, subleaf: u32, register: usize) -> RegisterClass {
    const ALL: &[(u32, u32)] = &[(0, 32)];
    const ADDRESS_WIDTHS: &[(u32, u32)] = &[(0, 8), (8, 8)];
    match (leaf, subleaf, register) {
        (0x0, _, 0) | (0x8000_0000, _, 0) | (0x7, 0, 0) => RegisterClass::Limits(ALL),
        (0x8000_0008, _, 0) => RegisterClass::Limits(ADDRESS_WIDTHS),
        (0x0 | 0x2 | 0x3 | 0x4 | 0xd | 0x18 | 0x8000_001d, _, _)
        | (0x1 | 0x8000_0001, _, 0)
        | (0x1, _, 1)
        | (0x8000_0002..=0x8000_0006, _, _)
        | (0x8000_0008, _, 2 | 3) => RegisterClass::Informational,
        _ => RegisterClass::Features,
    }
}

/// Checks that the backend described by `surface` supports `profile`: every
/// feature bit the profile sets except the [`TIME_POLICY_BITS`], every
/// numeric limit, the exact XSAVE layout of every enabled component, the
/// guest physical address width, and every pinned MSR value. For a
/// [`CpuidPresentation::PassThroughGuestView`] surface, it also checks that
/// every entry outside the profile's tables reads zero, as
/// [`check_unlisted_cpuid`](crate::check_unlisted_cpuid) does.
///
/// Fails with `E_PROFILE_UNSUPPORTED`, naming every violation at once: each
/// missing leaf, subleaf, register, and bit, as [`support_violations`] lists
/// them, then each non-zero entry outside the profile's tables. A surface
/// whose only violations are such entries fails with
/// `E_CPU_UNLISTED`.
pub fn verify_support(profile: &CpuProfile, surface: &HostCpuSurface) -> Result<(), ProfileError> {
    let unlisted = match surface.presentation {
        CpuidPresentation::PassThroughGuestView => {
            unlisted_cpuid_violations(profile, &surface.cpuid)
        }
        CpuidPresentation::Table | CpuidPresentation::PassThroughHostView => Vec::new(),
    };
    support_result(profile, support_violations(profile, surface), &unlisted)
}

/// Checks, as [`verify_support`] does, that the backend described by
/// `surface` supports `profile`, but checks the entries outside the
/// profile's tables in `presented` instead of `surface`: what VP 0 of a
/// partition configured from `profile`, as a cold boot configures it, reads
/// at the host's
/// [`unlisted_cpuid_candidates`](crate::unlisted_cpuid_candidates).
///
/// `--cpu-fingerprint` uses it for a pass-through backend whose fingerprint
/// records a probe partition with every available feature: such a partition
/// also presents the XSAVE components of features that no profile enables,
/// such as CET's user and supervisor states, which a partition configured
/// from the profile does not.
pub fn verify_support_with_unlisted(
    profile: &CpuProfile,
    surface: &HostCpuSurface,
    presented: &[CpuidEntry],
) -> Result<(), ProfileError> {
    support_result(
        profile,
        support_violations(profile, surface),
        &unlisted_cpuid_violations(profile, presented),
    )
}

/// Returns the outcome of a support check with `violations` and the
/// `unlisted` entries: `E_PROFILE_UNSUPPORTED` naming both, or
/// `E_CPU_UNLISTED` when only entries outside the profile fail.
fn support_result(
    profile: &CpuProfile,
    violations: Vec<String>,
    unlisted: &[String],
) -> Result<(), ProfileError> {
    if violations.is_empty() {
        if unlisted.is_empty() {
            return Ok(());
        }
        return Err(unlisted_cpuid_error(profile, unlisted));
    }
    Err(ProfileError::new(
        ProfileErrorCode::ProfileUnsupported,
        format!(
            "the backend does not support CPU profile {}: {}",
            profile.id(),
            violations
                .iter()
                .chain(unlisted)
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("; ")
        ),
    ))
}

/// Returns every way in which `surface` falls short of `profile`, in the
/// order [`verify_support`] reports them.
pub fn support_violations(profile: &CpuProfile, surface: &HostCpuSurface) -> Vec<String> {
    let mut violations = Vec::new();
    for entry in profile.cpuid() {
        let leaf = entry.leaf.0;
        let subleaf = entry.subleaf.map_or(0, |subleaf| subleaf.0);
        let host = cpuid::lookup(&surface.cpuid, leaf, subleaf).unwrap_or_default();
        for (register, (value, mask)) in entry.values().iter().zip(entry.masks()).enumerate() {
            let pinned = value & mask;
            match register_class(leaf, subleaf, register) {
                RegisterClass::Informational => {}
                RegisterClass::Features => {
                    let exempt = TIME_POLICY_BITS
                        .iter()
                        .filter(|&&(l, s, r, _)| (l, s, r) == (leaf, subleaf, register))
                        .fold(0, |bits, &(.., bit)| bits | bit);
                    let missing = pinned & !exempt & !host[register];
                    if missing != 0 {
                        violations.push(format!(
                            "{} {} {} not supported",
                            describe(entry),
                            REGISTERS[register],
                            bits_phrase(missing.into())
                        ));
                    }
                }
                RegisterClass::Limits(fields) => {
                    for &(shift, width) in fields {
                        // The guest physical address width is checked against
                        // `surface.physical_address_width` below. The surface's
                        // CPUID may hold the host's width instead.
                        if (leaf, register, shift) == (EXTENDED_LEAF_BASE + 8, 0, 0) {
                            continue;
                        }
                        let field = |value: u32| field_value(value, shift, width);
                        if field(pinned) > field(host[register]) {
                            violations.push(format!(
                                "{} {}[{}:{shift}] is {:#x}, above the supported {:#x}",
                                describe(entry),
                                REGISTERS[register],
                                shift + width - 1,
                                field(pinned),
                                field(host[register])
                            ));
                        }
                    }
                }
            }
        }
    }

    let (host_xcr0, host_xss) = cpuid::xsave_supported(&surface.cpuid);
    let missing_xcr0 = profile.xcr0() & !host_xcr0;
    if missing_xcr0 != 0 {
        violations.push(format!("XCR0 bits {missing_xcr0:#x} are not supported"));
    }
    let missing_xss = profile.xss() & !host_xss;
    if missing_xss != 0 {
        violations.push(format!("IA32_XSS bits {missing_xss:#x} are not supported"));
    }
    let host_components = cpuid::xsave_components(&surface.cpuid);
    for component in profile.xsave_components() {
        match host_components
            .iter()
            .find(|host| host.index == component.index)
        {
            Some(host) if host == component => {}
            Some(host) => violations.push(format!(
                "XSAVE component {} is {} bytes at offset {} (flags supervisor={} align64={} xfd={}), but the backend's is {} bytes at offset {} (flags supervisor={} align64={} xfd={})",
                component.index,
                component.size,
                component.offset,
                component.supervisor,
                component.align64,
                component.xfd,
                host.size,
                host.offset,
                host.supervisor,
                host.align64,
                host.xfd
            )),
            None => violations.push(format!(
                "XSAVE component {} has no layout",
                component.index
            )),
        }
    }

    if profile.physical_address_width() > surface.physical_address_width {
        violations.push(format!(
            "the guest physical address width {} exceeds the supported {}",
            profile.physical_address_width(),
            surface.physical_address_width
        ));
    }

    for msr in profile.msrs() {
        let index = msr.index.0;
        let host = surface.msr(index).copied().unwrap_or(SupportedMsr {
            index,
            supported: 0,
            controllable: 0,
        });
        let unsupported = msr.value.0 & !host.supported;
        if unsupported != 0 {
            violations.push(format!(
                "MSR {index:#x} {} not supported",
                bits_phrase(unsupported)
            ));
        }
        let uncontrollable = !msr.value.0 & msr.mask.0 & host.supported & !host.controllable;
        if uncontrollable != 0 {
            violations.push(format!(
                "MSR {index:#x} {} pinned clear but cannot be cleared",
                bits_phrase(uncontrollable)
            ));
        }
    }
    violations
}

/// Returns the `width`-bit field at `shift` of `value`.
pub(crate) fn field_value(value: u32, shift: u32, width: u32) -> u32 {
    (value >> shift) & u32::MAX.checked_shr(32 - width).unwrap_or(0)
}

/// Returns `bit 16 is` or `bits 3, 7 are`.
fn bits_phrase(bits: u64) -> String {
    let list = (0..64)
        .filter(|bit| bits & (1 << bit) != 0)
        .map(|bit| bit.to_string())
        .collect::<Vec<_>>();
    if list.len() == 1 {
        format!("bit {} is", list[0])
    } else {
        format!("bits {} are", list.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fingerprint;
    use crate::test_support::fingerprint_with;
    use crate::test_support::profile;
    use crate::test_support::profile_entries;
    use test_with_tracing::test;

    const ICELAKE: &str = "intel.icelake-sp.v1";

    /// Returns the surface of a KVM host that supports exactly `profile`.
    fn surface(profile: &CpuProfile) -> HostCpuSurface {
        HostCpuSurface::from_fingerprint(&fingerprint(profile, "kvm").backend)
    }

    /// Changes the register of `leaf` and `subleaf` in `surface`.
    fn edit(
        surface: &mut HostCpuSurface,
        leaf: u32,
        subleaf: u32,
        register: usize,
        f: impl FnOnce(u32) -> u32,
    ) {
        let entry = surface
            .cpuid
            .iter_mut()
            .find(|entry| entry.leaf.0 == leaf && entry.subleaf.is_none_or(|s| s.0 == subleaf))
            .unwrap();
        let mut registers = entry.registers();
        registers[register] = f(registers[register]);
        *entry = CpuidEntry::new(leaf, entry.subleaf.map(|s| s.0), registers);
    }

    fn unsupported(profile: &CpuProfile, surface: &HostCpuSurface) -> String {
        let error = verify_support(profile, surface).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::ProfileUnsupported, "{error}");
        error.message
    }

    #[test]
    fn every_pinned_profile_supports_itself() {
        for profile in crate::pinned_profiles() {
            verify_support(profile, &surface(profile)).unwrap();
            for backend in ["mshv", "whp"] {
                let surface =
                    HostCpuSurface::from_fingerprint(&fingerprint(profile, backend).backend);
                assert_eq!(
                    surface.presentation,
                    CpuidPresentation::PassThroughGuestView
                );
                verify_support(profile, &surface).unwrap();
            }
        }
    }

    #[test]
    fn checks_unlisted_entries_of_a_pass_through_guest_view_only() {
        let profile = profile(ICELAKE);
        // Intel PT's subleaf 1, which the profile does not list, as the root
        // partition of a Skylake-SP host reads it.
        let mut entries = profile_entries(profile);
        entries.push(CpuidEntry::new(
            0x14,
            Some(1),
            [0x0249_0002, 0x003f_3fff, 0, 0],
        ));
        entries.sort_by_key(CpuidEntry::key);
        const PT: &str = "CPUID 0x14.1 is outside the profile and reads EAX 0x2490002, \
                          EBX 0x3f3fff, ECX 0x0, EDX 0x0";

        let whp = HostCpuSurface::from_fingerprint(
            &fingerprint_with(profile, "whp", entries.clone()).backend,
        );
        let error = verify_support(profile, &whp).unwrap_err();
        assert_eq!(error.code, ProfileErrorCode::CpuUnlisted);
        assert_eq!(
            error.message,
            format!(
                "the backend presents CPUID entries outside CPU profile intel.icelake-sp.v1: {PT}"
            )
        );
        assert!(support_violations(profile, &whp).is_empty());

        // KVM installs the effective CPUID as the guest's whole table.
        let kvm = HostCpuSurface::from_fingerprint(
            &fingerprint_with(profile, "kvm", entries.clone()).backend,
        );
        assert_eq!(kvm.presentation, CpuidPresentation::Table);
        verify_support(profile, &kvm).unwrap();

        // The host's own view does not show what a guest reads there.
        let host_view = HostCpuSurface {
            presentation: CpuidPresentation::PassThroughHostView,
            ..whp.clone()
        };
        verify_support(profile, &host_view).unwrap();

        // An unlisted entry that reads zero is what the profile requires.
        let mut zero = whp.clone();
        edit(&mut zero, 0x14, 1, 0, |_| 0);
        edit(&mut zero, 0x14, 1, 1, |_| 0);
        verify_support(profile, &zero).unwrap();

        // Every violation is named at once, the missing features first.
        let mut both = whp;
        edit(&mut both, 7, 0, 1, |ebx| ebx & !(1 << 16));
        assert_eq!(
            unsupported(profile, &both),
            format!(
                "the backend does not support CPU profile intel.icelake-sp.v1: \
                 CPUID 0x7.0 EBX bit 16 is not supported; {PT}"
            )
        );
    }

    #[test]
    fn the_time_policy_bits_are_exempt() {
        let profile = profile(ICELAKE);
        let mut surface = surface(profile);
        for (leaf, subleaf, register, bit) in TIME_POLICY_BITS {
            edit(&mut surface, leaf, subleaf, register, |value| value & !bit);
        }
        verify_support(profile, &surface).unwrap();
    }

    #[test]
    fn names_missing_features_and_limits() {
        let profile = profile(ICELAKE);
        let mut surface = surface(profile);
        // AVX512F and PSFD.
        edit(&mut surface, 7, 0, 1, |ebx| ebx & !(1 << 16));
        edit(&mut surface, 7, 2, 3, |edx| edx & !1);
        let message = unsupported(profile, &surface);
        assert_eq!(
            message,
            "the backend does not support CPU profile intel.icelake-sp.v1: \
             CPUID 0x7.0 EBX bit 16 is not supported; CPUID 0x7.2 EDX bit 0 is not supported"
        );

        let mut surface = self::surface(profile);
        edit(&mut surface, 0x8000_0008, 0, 0, |eax| (eax & !0xff) | 39);
        surface.physical_address_width = 39;
        let violations = support_violations(profile, &surface);
        assert_eq!(
            violations,
            ["the guest physical address width 46 exceeds the supported 39"]
        );

        // The surface's own field decides the physical address width, not its
        // CPUID: a cheap surface from the host's CPUID may report the host's
        // narrower width.
        let mut surface = self::surface(profile);
        edit(&mut surface, 0x8000_0008, 0, 0, |eax| (eax & !0xff) | 42);
        verify_support(profile, &surface).unwrap();

        // The linear address width is still a CPUID limit.
        let mut surface = self::surface(profile);
        edit(&mut surface, 0x8000_0008, 0, 0, |eax| {
            (eax & !0xff00) | 48 << 8
        });
        assert_eq!(
            support_violations(profile, &surface),
            ["CPUID 0x80000008 EAX[15:8] is 0x39, above the supported 0x30"]
        );

        let mut surface = self::surface(profile);
        edit(&mut surface, 7, 0, 0, |_| 0);
        assert!(
            unsupported(profile, &surface)
                .contains("CPUID 0x7.0 EAX[31:0] is 0x2, above the supported 0x0")
        );
    }

    #[test]
    fn requires_the_exact_xsave_layout() {
        let profile = profile(ICELAKE);
        let mut surface = surface(profile);
        edit(&mut surface, 0xd, 7, 1, |offset| offset + 64);
        assert!(
            unsupported(profile, &surface)
                .contains("XSAVE component 7 is 1024 bytes at offset 1664")
        );
        let mut surface = self::surface(profile);
        edit(&mut surface, 0xd, 0, 0, |eax| eax & !(1 << 7));
        let violations = support_violations(profile, &surface);
        assert!(
            violations.contains(&"XCR0 bits 0x80 are not supported".to_owned()),
            "{violations:?}"
        );
    }

    #[test]
    fn checks_that_pinned_msr_values_can_be_presented() {
        let profile = profile(ICELAKE);
        let (value, mask) = profile.msr(IA32_ARCH_CAPABILITIES).unwrap();
        assert_eq!((value, mask), (0x0800_0121, 0x4000_0000_0c12_e1bb));

        // KVM controls every bit, so it can present the value while
        // supporting more.
        let mut surface = surface(profile);
        surface.msrs[0].supported |= 1 << 62 | 1 << 26;
        verify_support(profile, &surface).unwrap();

        // A Hyper-V backend cannot clear what its banks do not derive.
        surface.msrs[0].controllable = hv_banks::ARCH_CAPABILITIES_BANK_MASK;
        assert!(
            unsupported(profile, &surface)
                .contains("MSR 0x10a bit 62 is pinned clear but cannot be cleared")
        );

        // A missing immunity bit.
        let mut surface = self::surface(profile);
        surface.msrs[0].supported = value & !(1 << 27);
        assert!(unsupported(profile, &surface).contains("MSR 0x10a bit 27 is not supported"));
        surface.msrs.clear();
        assert!(
            unsupported(profile, &surface).contains("MSR 0x10a bits 0, 5, 8, 27 are not supported")
        );
    }

    #[test]
    fn reads_the_hyper_v_arch_capabilities_from_the_banks() {
        let profile = profile(ICELAKE);
        let mut fingerprint = fingerprint(profile, "whp");
        fingerprint.backend.msrs.arch_capabilities = None;
        fingerprint.backend.set_feature_bank(
            "whp.capability.ProcessorFeaturesBanks.bank0",
            0x2e0a_8bff_e7f7_859f,
        );
        fingerprint.backend.set_feature_bank(
            "whp.capability.ProcessorFeaturesBanks.bank1",
            0x0001_000e_0000_00f1,
        );
        let surface = HostCpuSurface::from_fingerprint(&fingerprint.backend);
        assert_eq!(
            surface.msr(IA32_ARCH_CAPABILITIES).copied(),
            Some(SupportedMsr {
                index: IA32_ARCH_CAPABILITIES,
                supported: 0x0800_0121,
                controllable: hv_banks::ARCH_CAPABILITIES_BANK_MASK,
            })
        );
        verify_support(profile, &surface).unwrap();
    }
}
