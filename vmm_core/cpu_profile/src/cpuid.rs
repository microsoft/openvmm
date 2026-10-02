// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID tables: entries, subleaf enumeration, the XSAVE layout they
//! describe, and the normalization of fields that differ between processors
//! and virtual processor states rather than between CPU surfaces.

use crate::Hex32;
use serde::Deserialize;
use serde::Serialize;

/// The first leaf of the hypervisor range.
pub const HYPERVISOR_LEAF_BASE: u32 = 0x4000_0000;
/// The first leaf of the extended range.
pub const EXTENDED_LEAF_BASE: u32 = 0x8000_0000;

/// The most leaves enumerated in each of the basic, hypervisor, and extended
/// ranges, which bounds the enumeration of a hypervisor reporting garbage.
const MAX_LEAVES_PER_RANGE: u32 = 0x100;
/// The most subleaves enumerated for one leaf.
const MAX_SUBLEAVES: u32 = 64;

/// The size of the legacy region and the header of an XSAVE area.
const XSAVE_LEGACY_AND_HEADER_SIZE: u32 = 512 + 64;

/// `CPUID.1:EBX[31:24]`, the initial APIC ID of the executing processor.
const LEAF1_EBX_INITIAL_APIC_ID: u32 = 0xff00_0000;
/// `CPUID.1:ECX[27]`, OSXSAVE, which mirrors CR4.OSXSAVE.
const LEAF1_ECX_OSXSAVE: u32 = 1 << 27;
/// `CPUID.(7,0):ECX[4]`, OSPKE, which mirrors CR4.PKE.
const LEAF7_ECX_OSPKE: u32 = 1 << 4;

/// One CPUID leaf, or one subleaf of an indexed leaf, with its full register
/// values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuidEntry {
    /// The leaf, the input value of `EAX`.
    pub leaf: Hex32,
    /// The subleaf, the input value of `ECX`, or `None` for a leaf whose
    /// output does not depend on `ECX`.
    pub subleaf: Option<Hex32>,
    /// The output value of `EAX`.
    pub eax: Hex32,
    /// The output value of `EBX`.
    pub ebx: Hex32,
    /// The output value of `ECX`.
    pub ecx: Hex32,
    /// The output value of `EDX`.
    pub edx: Hex32,
}

impl CpuidEntry {
    /// Returns an entry for `leaf` and `subleaf` with the output registers
    /// `[eax, ebx, ecx, edx]`.
    pub fn new(leaf: u32, subleaf: Option<u32>, registers: [u32; 4]) -> Self {
        let [eax, ebx, ecx, edx] = registers;
        Self {
            leaf: Hex32(leaf),
            subleaf: subleaf.map(Hex32),
            eax: Hex32(eax),
            ebx: Hex32(ebx),
            ecx: Hex32(ecx),
            edx: Hex32(edx),
        }
    }

    /// Returns the output registers as `[eax, ebx, ecx, edx]`.
    pub fn registers(&self) -> [u32; 4] {
        [self.eax.0, self.ebx.0, self.ecx.0, self.edx.0]
    }

    /// Returns the sort key of the entry: the leaf, then the subleaf, with a
    /// subleaf-independent entry first.
    pub fn key(&self) -> (u32, Option<u32>) {
        (self.leaf.0, self.subleaf.map(|subleaf| subleaf.0))
    }
}

/// Returns whether the output of `leaf` depends on the subleaf in `ECX`.
///
/// These are the leaves for which KVM sets `KVM_CPUID_FLAG_SIGNIFCANT_INDEX`,
/// so tables enumerated here and tables from `KVM_GET_SUPPORTED_CPUID` index
/// the same leaves.
pub fn is_indexed_leaf(leaf: u32) -> bool {
    matches!(
        leaf,
        0x4 | 0x7
            | 0xb
            | 0xd
            | 0xf
            | 0x10
            | 0x12
            | 0x14
            | 0x17
            | 0x18
            | 0x1d
            | 0x1e
            | 0x1f
            | 0x24
            | 0x8000_001d
    )
}

/// Returns the output registers that `entries` define for `leaf` and
/// `subleaf`, if any. A subleaf-independent entry matches every subleaf.
pub fn lookup(entries: &[CpuidEntry], leaf: u32, subleaf: u32) -> Option<[u32; 4]> {
    entries
        .iter()
        .find(|entry| {
            entry.leaf.0 == leaf
                && entry
                    .subleaf
                    .is_none_or(|entry_subleaf| entry_subleaf.0 == subleaf)
        })
        .map(CpuidEntry::registers)
}

/// Returns whether bit `bit` of register `register` (0 for `EAX` through 3
/// for `EDX`) is set for `leaf` and `subleaf`.
pub fn has_bit(entries: &[CpuidEntry], leaf: u32, subleaf: u32, register: usize, bit: u32) -> bool {
    lookup(entries, leaf, subleaf).is_some_and(|registers| registers[register] & (1 << bit) != 0)
}

/// Enumerates the CPUID table reported by `query(leaf, subleaf)`.
///
/// This walks the basic, hypervisor, and extended ranges up to the maximum
/// leaf that each range reports, and the subleaves of every indexed leaf as
/// the architecture enumerates them: until an invalid level or cache type,
/// up to the maximum subleaf reported in subleaf 0, or, for XSAVE, one
/// subleaf per supported state component. The hypervisor range is walked
/// only when it reports a plausible maximum leaf; otherwise only its first
/// leaf is recorded. The result is sorted.
pub fn enumerate<E>(
    query: impl FnMut(u32, u32) -> Result<[u32; 4], E>,
) -> Result<Vec<CpuidEntry>, E> {
    enumerate_ranges(&[0, HYPERVISOR_LEAF_BASE, EXTENDED_LEAF_BASE], query)
}

/// Enumerates the basic and extended ranges of the CPUID table reported by
/// `query(leaf, subleaf)` as [`enumerate`] does, without querying the
/// hypervisor range `0x40000000..=0x4fffffff`.
///
/// A host's hypervisor leaves describe the hypervisor it runs on, not its
/// processor. Nothing that checks a profile against the host reads them:
/// profiles list no hypervisor leaves, and
/// [`unlisted_cpuid_candidates`](crate::unlisted_cpuid_candidates) exempts
/// the range. Every CPUID instruction in a Hyper-V root partition exits to the
/// hypervisor, so a backend that enumerates its host's CPUID at partition
/// creation saves the range's queries, about a fifth of the total on the
/// fleet's Hyper-V hosts.
pub fn enumerate_basic_and_extended<E>(
    query: impl FnMut(u32, u32) -> Result<[u32; 4], E>,
) -> Result<Vec<CpuidEntry>, E> {
    enumerate_ranges(&[0, EXTENDED_LEAF_BASE], query)
}

/// Enumerates the ranges that start at `bases`, as [`enumerate`] describes.
fn enumerate_ranges<E>(
    bases: &[u32],
    mut query: impl FnMut(u32, u32) -> Result<[u32; 4], E>,
) -> Result<Vec<CpuidEntry>, E> {
    let mut entries = Vec::new();
    for &base in bases {
        let max = query(base, 0)?[0];
        let last = if (base..base + MAX_LEAVES_PER_RANGE).contains(&max) {
            max
        } else {
            base
        };
        for leaf in base..=last {
            enumerate_leaf(&mut query, leaf, &mut entries)?;
        }
    }
    entries.sort_by_key(CpuidEntry::key);
    Ok(entries)
}

fn enumerate_leaf<E>(
    query: &mut impl FnMut(u32, u32) -> Result<[u32; 4], E>,
    leaf: u32,
    entries: &mut Vec<CpuidEntry>,
) -> Result<(), E> {
    if !is_indexed_leaf(leaf) {
        entries.push(CpuidEntry::new(leaf, None, query(leaf, 0)?));
        return Ok(());
    }

    let mut subleaf_query = |subleaf: u32| -> Result<[u32; 4], E> {
        let registers = query(leaf, subleaf)?;
        entries.push(CpuidEntry::new(leaf, Some(subleaf), registers));
        Ok(registers)
    };
    let first = subleaf_query(0)?;
    match leaf {
        // Deterministic cache parameters, up to and including the first
        // subleaf with a null cache type.
        0x4 | 0x8000_001d => {
            let mut registers = first;
            for subleaf in 1..MAX_SUBLEAVES {
                if registers[0] & 0x1f == 0 {
                    break;
                }
                registers = subleaf_query(subleaf)?;
            }
        }
        // Extended topology, up to and including the first subleaf with an
        // invalid level type.
        0xb | 0x1f => {
            let mut registers = first;
            for subleaf in 1..MAX_SUBLEAVES {
                if (registers[2] >> 8) & 0xff == 0 {
                    break;
                }
                registers = subleaf_query(subleaf)?;
            }
        }
        // Subleaf 0 reports the maximum subleaf in EAX.
        0x7 | 0x14 | 0x17 | 0x18 | 0x1d | 0x24 => {
            for subleaf in 1..=first[0].min(MAX_SUBLEAVES - 1) {
                subleaf_query(subleaf)?;
            }
        }
        // XSAVE: the two fixed subleaves, then one per supported state
        // component.
        0xd => {
            let second = subleaf_query(1)?;
            let components = xsave_mask(first[0], first[3]) | xsave_mask(second[2], second[3]);
            for subleaf in 2..63 {
                if components & (1 << subleaf) != 0 {
                    subleaf_query(subleaf)?;
                }
            }
        }
        // Resource director technology monitoring and allocation.
        0xf => {
            subleaf_query(1)?;
        }
        0x10 => {
            for subleaf in 1..=3 {
                subleaf_query(subleaf)?;
            }
        }
        // SGX: the two fixed subleaves, then the EPC sections up to and
        // including the first invalid one.
        0x12 => {
            subleaf_query(1)?;
            for subleaf in 2..MAX_SUBLEAVES {
                if subleaf_query(subleaf)?[0] & 0xf == 0 {
                    break;
                }
            }
        }
        // TMUL information has a single subleaf.
        _ => {}
    }
    Ok(())
}

/// Normalizes a CPUID table so that tables from different runs, hosts, and
/// backends compare equal when they offer guests the same CPU.
///
/// This sorts the entries, drops duplicate keys (keeping the first), and
/// rewrites the fields that describe the executing processor or the current
/// virtual processor state rather than the CPU surface:
///
/// - The initial APIC ID, `CPUID.1:EBX[31:24]`, and the x2APIC IDs of the
///   extended topology leaves 0xb and 0x1f in EDX, are zeroed. A backend
///   reports the ID of whichever processor executed the query, and the VMM
///   sets each virtual processor's own ID.
/// - The AMD extended APIC ID, CPUID.0x8000001e:EAX, is zeroed for the same
///   reason.
/// - OSXSAVE, `CPUID.1:ECX[27]`, and OSPKE, `CPUID.(7,0):ECX[4]`, are cleared.
///   They mirror control register bits, which are clear at reset.
/// - The XSAVE area sizes of the enabled features, CPUID.(0xd,0):EBX and
///   CPUID.(0xd,1):EBX, are set to the standard size of every supported user
///   state component and to the compacted size of every supported user and
///   supervisor state component. They otherwise depend on the current XCR0
///   and IA32_XSS. These are the values that `KVM_GET_SUPPORTED_CPUID`
///   reports.
pub fn normalize(entries: &mut Vec<CpuidEntry>) {
    entries.sort_by_key(CpuidEntry::key);
    entries.dedup_by_key(|entry| entry.key());
    let (xcr0, xss) = xsave_supported(entries);
    let components = xsave_components(entries);
    let standard_size = xsave_standard_size(&components, xcr0);
    let compacted_size = xsave_compacted_size(&components, xcr0 | xss);
    for entry in entries.iter_mut() {
        match entry.key() {
            (0x1, _) => {
                entry.ebx.0 &= !LEAF1_EBX_INITIAL_APIC_ID;
                entry.ecx.0 &= !LEAF1_ECX_OSXSAVE;
            }
            (0x7, Some(0)) => entry.ecx.0 &= !LEAF7_ECX_OSPKE,
            (0xb | 0x1f, _) => entry.edx.0 = 0,
            (0xd, Some(0)) => entry.ebx.0 = standard_size,
            (0xd, Some(1)) => entry.ebx.0 = compacted_size,
            (0x8000_001e, _) => entry.eax.0 = 0,
            _ => {}
        }
    }
}

fn xsave_mask(low: u32, high: u32) -> u64 {
    u64::from(low) | (u64::from(high) << 32)
}

/// One XSAVE state component, from CPUID.(0xd,n) for component `n >= 2`.
///
/// The fields are declared in the byte order of their keys, so profiles can
/// serialize it directly in canonical form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XsaveComponent {
    /// The component is 64-byte aligned in the compacted format (`ECX[1]`).
    pub align64: bool,
    /// The component number, which is its bit in XCR0 or IA32_XSS.
    pub index: u32,
    /// The offset of the component in the standard format, from EBX. It is
    /// zero for supervisor components, which only the compacted format holds.
    pub offset: u32,
    /// The size of the component in bytes, from EAX.
    pub size: u32,
    /// The component is a supervisor state, enabled in IA32_XSS (`ECX[0]`).
    pub supervisor: bool,
    /// The component supports extended feature disable (`ECX[2]`).
    pub xfd: bool,
}

/// Returns the supported XCR0 and IA32_XSS bits, from CPUID.(0xd,0):EDX:EAX
/// and CPUID.(0xd,1):EDX:ECX.
pub fn xsave_supported(entries: &[CpuidEntry]) -> (u64, u64) {
    let first = lookup(entries, 0xd, 0).unwrap_or_default();
    let second = lookup(entries, 0xd, 1).unwrap_or_default();
    (
        xsave_mask(first[0], first[3]),
        xsave_mask(second[2], second[3]),
    )
}

/// Returns the supported XSAVE state components beyond x87 and SSE, ordered
/// by component number.
pub fn xsave_components(entries: &[CpuidEntry]) -> Vec<XsaveComponent> {
    let (xcr0, xss) = xsave_supported(entries);
    (2..63)
        .filter(|index| (xcr0 | xss) & (1 << index) != 0)
        .filter_map(|index| {
            let [size, offset, flags, _] = lookup(entries, 0xd, index)?;
            Some(XsaveComponent {
                index,
                size,
                offset,
                supervisor: flags & 1 != 0,
                align64: flags & 2 != 0,
                xfd: flags & 4 != 0,
            })
        })
        .collect()
}

/// Returns the size of a standard-format XSAVE area holding the user state
/// components in `xcr0`.
pub fn xsave_standard_size(components: &[XsaveComponent], xcr0: u64) -> u32 {
    components
        .iter()
        .filter(|component| xcr0 & (1 << component.index) != 0 && !component.supervisor)
        .map(|component| component.offset.saturating_add(component.size))
        .fold(XSAVE_LEGACY_AND_HEADER_SIZE, u32::max)
}

/// Returns the size of a compacted-format XSAVE area holding the state
/// components in `mask`.
pub fn xsave_compacted_size(components: &[XsaveComponent], mask: u64) -> u32 {
    components
        .iter()
        .filter(|component| mask & (1 << component.index) != 0)
        .fold(XSAVE_LEGACY_AND_HEADER_SIZE, |size, component| {
            let start = if component.align64 {
                size.checked_next_multiple_of(64).unwrap_or(u32::MAX)
            } else {
                size
            };
            start.saturating_add(component.size)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use test_with_tracing::test;

    /// A small Skylake-like CPUID table, keyed by leaf and subleaf.
    fn table() -> BTreeMap<(u32, u32), [u32; 4]> {
        BTreeMap::from([
            ((0x0, 0), [0x16, 0x756e_6547, 0x6c65_746e, 0x4965_6e69]),
            (
                (0x1, 0),
                [0x0005_0654, 0x2a20_0800, 0xfffa_3203, 0x0f8b_fbff],
            ),
            ((0x4, 0), [0x0c00_4121, 0x01c0_003f, 0x3f, 0]),
            ((0x4, 1), [0x0c00_4122, 0x00c0_003f, 0x3f, 0]),
            ((0x4, 2), [0x0c00_4143, 0x03c0_003f, 0x3ff, 0]),
            (
                (0x7, 0),
                [0, 0xd19f_27eb, 0x8 | LEAF7_ECX_OSPKE, 0xbc00_0400],
            ),
            ((0xb, 0), [1, 2, 0x100, 0x2a]),
            ((0xb, 1), [5, 0x14, 0x201, 0x2a]),
            // XCR0: x87, SSE, AVX (2), opmask/ZMM (5-7). XSS: PT (8).
            ((0xd, 0), [0xe7, 0x340, 0xa88, 0]),
            ((0xd, 1), [0xf, 0x3c0, 0x100, 0]),
            ((0xd, 2), [0x100, 0x240, 0, 0]),
            ((0xd, 5), [0x40, 0x440, 0, 0]),
            ((0xd, 6), [0x200, 0x480, 0, 0]),
            ((0xd, 7), [0x400, 0x680, 0, 0]),
            ((0xd, 8), [0x80, 0, 1, 0]),
            ((0x40000000, 0), [0, 0, 0, 0]),
            ((0x8000_0000, 0), [0x8000_0001, 0, 0, 0]),
            ((0x8000_0001, 0), [0, 0, 0x121, 0x2c10_0800]),
        ])
    }

    fn query(
        table: &BTreeMap<(u32, u32), [u32; 4]>,
    ) -> impl FnMut(u32, u32) -> Result<[u32; 4], ()> {
        |leaf, subleaf| Ok(table.get(&(leaf, subleaf)).copied().unwrap_or_default())
    }

    #[test]
    fn enumerates_ranges_and_subleaves() {
        let table = table();
        let entries = enumerate(query(&table)).unwrap();
        let keys = entries.iter().map(CpuidEntry::key).collect::<Vec<_>>();

        // Basic leaves 0 through 0x16, with cache subleaves up to and
        // including the null entry (subleaf 3).
        assert!(keys.contains(&(0x16, None)));
        assert!(!keys.contains(&(0x17, Some(0))));
        for subleaf in 0..=3 {
            assert!(keys.contains(&(0x4, Some(subleaf))), "subleaf {subleaf}");
        }
        assert!(!keys.contains(&(0x4, Some(4))));
        // Leaf 7 reports no subleaves beyond 0.
        assert!(keys.contains(&(0x7, Some(0))));
        assert!(!keys.contains(&(0x7, Some(1))));
        // Topology up to and including the invalid level (subleaf 2).
        assert!(keys.contains(&(0xb, Some(2))));
        assert!(!keys.contains(&(0xb, Some(3))));
        // XSAVE: subleaves 0, 1, and one per supported component.
        let xsave = keys
            .iter()
            .filter(|(leaf, _)| *leaf == 0xd)
            .map(|(_, subleaf)| subleaf.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(xsave, [0, 1, 2, 5, 6, 7, 8]);
        // An implausible hypervisor maximum leaf records only the first leaf.
        assert!(keys.contains(&(0x4000_0000, None)));
        assert!(!keys.contains(&(0x4000_0001, None)));
        // Extended leaves.
        assert_eq!(keys.last(), Some(&(0x8000_0001, None)));
        // Sorted.
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn basic_and_extended_enumeration_skips_only_the_hypervisor_range() {
        // A root partition's plausible hypervisor range, 0x40000000 to
        // 0x4000000c as on the fleet's Hyper-V hosts, and the fixture's
        // implausible one.
        let mut hyperv = table();
        hyperv.insert(
            (0x4000_0000, 0),
            [0x4000_000c, 0x7263_694d, 0x666f_736f, 0x7648_2074],
        );
        for leaf in 0x4000_0001..=0x4000_000c {
            hyperv.insert((leaf, 0), [leaf, 1, 2, 3]);
        }
        let hypervisor = HYPERVISOR_LEAF_BASE..EXTENDED_LEAF_BASE;
        for host in [hyperv, table()] {
            let all = enumerate(query(&host)).unwrap();
            assert!(all.iter().any(|entry| hypervisor.contains(&entry.leaf.0)));

            let mut queried = Vec::new();
            let mut lookup = query(&host);
            let entries = enumerate_basic_and_extended(|leaf, subleaf| {
                queried.push(leaf);
                lookup(leaf, subleaf)
            })
            .unwrap();
            assert!(queried.iter().all(|leaf| !hypervisor.contains(leaf)));
            let expected = all
                .into_iter()
                .filter(|entry| !hypervisor.contains(&entry.leaf.0))
                .collect::<Vec<_>>();
            assert_eq!(entries, expected);
        }
    }

    #[test]
    fn enumeration_is_bounded() {
        // A hypervisor that reports endless valid subleaves and leaves.
        let entries = enumerate(|leaf, _| -> Result<_, ()> {
            Ok(match leaf {
                0 => [0xff, 0, 0, 0],
                0x4000_0000 => [0x4000_00ff, 0, 0, 0],
                0x8000_0000 => [0x8000_00ff, 0, 0, 0],
                _ => [!0, !0, !0, !0],
            })
        })
        .unwrap();
        assert!(entries.len() < 3 * 0x100 + 16 * 64 * 2);
    }

    #[test]
    fn enumeration_propagates_errors() {
        let result = enumerate(|leaf, _| {
            if leaf == 2 {
                Err("boom")
            } else {
                Ok([2, 0, 0, 0])
            }
        });
        assert_eq!(result, Err("boom"));
    }

    #[test]
    fn normalizes_processor_and_state_fields() {
        let table = table();
        let mut entries = enumerate(query(&table)).unwrap();
        let mut duplicated = entries.clone();
        duplicated.reverse();
        duplicated.push(CpuidEntry::new(0x1, None, [0; 4]));
        normalize(&mut entries);
        normalize(&mut duplicated);

        let [_, ebx, ecx, _] = lookup(&entries, 1, 0).unwrap();
        assert_eq!(ebx, 0x0020_0800);
        assert_eq!(ecx & LEAF1_ECX_OSXSAVE, 0);
        assert_eq!(lookup(&entries, 7, 0).unwrap()[2], 0x8);
        assert_eq!(lookup(&entries, 0xb, 0).unwrap()[3], 0);
        assert_eq!(lookup(&entries, 0xb, 1).unwrap()[3], 0);
        // Standard size: ZMM_Hi256 (7) ends at 0x680 + 0x400.
        assert_eq!(lookup(&entries, 0xd, 0).unwrap()[1], 0xa80);
        // Compacted: 576 + AVX 0x100 + 0x40 + 0x200 + 0x400 + PT 0x80.
        assert_eq!(
            lookup(&entries, 0xd, 1).unwrap()[1],
            576 + 0x100 + 0x40 + 0x200 + 0x400 + 0x80
        );
        // Normalization is independent of the input order and idempotent.
        assert_eq!(entries, duplicated);
        let again = {
            let mut again = entries.clone();
            normalize(&mut again);
            again
        };
        assert_eq!(entries, again);
    }

    #[test]
    fn computes_xsave_layout() {
        let table = table();
        let entries = enumerate(query(&table)).unwrap();
        assert_eq!(xsave_supported(&entries), (0xe7, 0x100));
        let components = xsave_components(&entries);
        assert_eq!(
            components.iter().map(|c| c.index).collect::<Vec<_>>(),
            [2, 5, 6, 7, 8]
        );
        assert!(components[4].supervisor);
        assert_eq!(xsave_standard_size(&components, 0xe7), 0xa80);
        // The supervisor component never counts towards the standard size.
        assert_eq!(xsave_standard_size(&components, 0x1e7), 0xa80);
        assert_eq!(xsave_standard_size(&components, 0x3), 576);

        let aligned = [
            XsaveComponent {
                index: 2,
                size: 10,
                offset: 576,
                supervisor: false,
                align64: false,
                xfd: false,
            },
            XsaveComponent {
                index: 3,
                size: 8,
                offset: 0,
                supervisor: false,
                align64: true,
                xfd: false,
            },
        ];
        assert_eq!(xsave_compacted_size(&aligned, 0xc), 640 + 8);
        assert_eq!(xsave_compacted_size(&aligned, 0x4), 586);
    }

    #[test]
    fn lookup_matches_subleaf_independent_entries() {
        let entries = [
            CpuidEntry::new(0x1, None, [1, 2, 3, 4]),
            CpuidEntry::new(0x7, Some(0), [5, 6, 7, 8]),
        ];
        assert_eq!(lookup(&entries, 0x1, 9), Some([1, 2, 3, 4]));
        assert_eq!(lookup(&entries, 0x7, 0), Some([5, 6, 7, 8]));
        assert_eq!(lookup(&entries, 0x7, 1), None);
        assert!(has_bit(&entries, 0x7, 0, 1, 1));
        assert!(!has_bit(&entries, 0x7, 0, 1, 0));
        assert!(!has_bit(&entries, 0x8, 0, 1, 1));
    }
}
