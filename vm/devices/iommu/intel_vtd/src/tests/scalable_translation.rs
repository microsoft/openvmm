// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use iommu_common::IommuTranslator;
use test_with_tracing::test;

#[path = "accessed_dirty.rs"]
mod accessed_dirty;

const ROOT: u64 = 0x1000;
const LOWER: u64 = 0x2000;
const UPPER: u64 = 0x3000;
const DIRECTORY: u64 = 0x10000;
const PASID_TABLE: u64 = 0x40000;
const SL_ROOT: u64 = 0x50000;
const GPA: u64 = 0x70000;
const UNMAPPED: u64 = 0x100000;
const IOVA: u64 = 0x1234_5678_9abc;

fn final_ecap() -> EcapReg {
    EcapReg::from(ECAP_VALUE)
        .with_smts(true)
        .with_ssts(true)
        .with_ssads(true)
        .with_smpwcs(true)
        .with_rps(true)
}

fn put(gm: &GuestMemory, address: u64, words: &[u64]) {
    for (index, word) in words.iter().enumerate() {
        gm.write_at(address + index as u64 * 8, &word.to_le_bytes())
            .unwrap();
    }
}

fn walk(gm: &GuestMemory, levels: u8, leaf: u8, iova: u64, gpa: u64) -> Vec<u64> {
    walk_at(gm, SL_ROOT, levels, leaf, iova, gpa)
}

fn walk_at(gm: &GuestMemory, root: u64, levels: u8, leaf: u8, iova: u64, gpa: u64) -> Vec<u64> {
    let mut entries = Vec::new();
    for level in (leaf..=levels).rev() {
        let table = root + u64::from(levels - level) * 4096;
        // Deliberately independent of SlPte's index/address helpers.
        let index = (iova >> (12 + 9 * (level - 1))) & 511;
        let address = table + index * 8;
        let value = if level == leaf {
            gpa | 3 | if leaf > 1 { 1 << 7 } else { 0 }
        } else {
            (table + 4096) | 3
        };
        put(gm, address, &[value]);
        entries.push(address);
    }
    entries
}

struct Fixture {
    dev: IntelVtdDevice,
    gm: GuestMemory,
    rid: u16,
    entries: [u64; 4],
}

impl Fixture {
    fn new(rid: u16, pasid: u32, pdts: u8) -> Self {
        let gm = GuestMemory::allocate(UNMAPPED as usize);
        let root = ROOT + u64::from(rid >> 8) * 16 + if rid & 128 == 0 { 0 } else { 8 };
        let table = if rid & 128 == 0 { LOWER } else { UPPER };
        let context = table + u64::from(rid & 127) * 32;
        let directory = DIRECTORY + u64::from(pasid >> 6) * 8;
        let pasid_entry = PASID_TABLE + u64::from(pasid & 63) * 64;
        put(&gm, root, &[table | 1]);
        put(
            &gm,
            context,
            &[
                DIRECTORY | 1 | (u64::from(pdts) << 9),
                u64::from(pasid),
                0,
                0,
            ],
        );
        put(&gm, directory, &[PASID_TABLE | 1]);
        put(
            &gm,
            pasid_entry,
            &[SL_ROOT | 1 | (2 << 2) | (2 << 6), 0xbeef, 0, 0, 0, 0, 0, 0],
        );
        walk(&gm, 4, 1, IOVA, GPA);
        let (mut dev, _) = IntelVtdDevice::new(
            gm.clone(),
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            Arc::new(TestSignalMsi),
        );
        write64(&mut dev, 0x020, ROOT | (1 << 10));
        write32(&mut dev, 0x018, (1 << 30) | (1 << 31));
        Self {
            dev,
            gm,
            rid,
            entries: [root, context, directory, pasid_entry],
        }
    }

    fn word(&self, address: u64) -> u64 {
        u64::from_le(self.gm.read_plain(address).unwrap())
    }

    fn fpd(&self, mask: u8) {
        for stage in 1..4 {
            let address = self.entries[stage];
            let value = (self.word(address) & !2) | (u64::from((mask >> (stage - 1)) & 1) << 1);
            put(&self.gm, address, &[value]);
        }
    }

    fn translation_context(&self, iova: u64) -> TranslationContext {
        TranslationContext {
            source_id: self.rid,
            iova,
            mode: TranslationTableMode::SCALABLE,
            fpd: false,
        }
    }

    fn resolve(&self) -> TranslationDescriptor {
        self.dev
            .shared
            .resolve_scalable(ROOT, self.translation_context(IOVA), final_ecap())
            .unwrap()
    }

    fn translate(
        &self,
        iova: u64,
        write: bool,
    ) -> Result<u64, iommu_common::TranslationFault<VtdFault>> {
        self.dev.shared.translator().translate_with_capabilities(
            self.rid,
            iova,
            write,
            final_ecap(),
            |gpa| {
                // Invalidation's write lock must also drain the DMA operation.
                assert!(self.dev.shared.state.try_write().is_none());
                gpa
            },
        )
    }

    fn assert_fault(&mut self, iova: u64, write: bool, reason: u8, suppressed: bool) {
        self.assert_fault_with_capabilities(iova, write, reason, suppressed, final_ecap());
    }

    fn translate_legacy(&self, iova: u64, write: bool) -> u64 {
        self.dev
            .shared
            .translator()
            .translate(self.rid, iova, write, |gpa| {
                assert!(self.dev.shared.state.try_write().is_none());
                gpa
            })
            .unwrap()
    }

    fn assert_legacy_fault(&mut self, iova: u64, write: bool, reason: u8, suppressed: bool) {
        self.assert_fault_with_capabilities(
            iova,
            write,
            reason,
            suppressed,
            EcapReg::from(ECAP_VALUE),
        );
    }

    fn assert_fault_with_capabilities(
        &mut self,
        iova: u64,
        write: bool,
        reason: u8,
        suppressed: bool,
        ecap: EcapReg,
    ) {
        write32(&mut self.dev, 0x12c, 1 << 31);
        write32(&mut self.dev, 0x034, u32::MAX);
        let previous = (read64(&mut self.dev, 0x120), read64(&mut self.dev, 0x128));
        let fault = self
            .dev
            .shared
            .translator()
            .translate_with_capabilities(self.rid, iova, write, ecap, |_| {
                panic!("DMA operation ran on a fault")
            })
            .unwrap_err();
        assert_eq!(fault.iova, iova);
        assert_eq!(fault.error.source_id(), self.rid);
        assert_eq!(fault.error.fault_address(), iova);
        assert_eq!(fault.error.fault_reason().0, reason, "{fault:?}");
        assert_eq!(fault.error.fpd(), suppressed, "{fault:?}");
        let raw = (read64(&mut self.dev, 0x120), read64(&mut self.dev, 0x128));
        if suppressed {
            assert_eq!(raw, previous);
            assert_eq!(read32(&mut self.dev, 0x034) & 3, 0);
        } else {
            // FI retains the original request at 4KB granularity, with bits
            // above the largest advertised AGAW reserved (§11.4.7.6).
            // PP/PASID/PRIV/EXE stay zero: RID_PASID is not an explicit tag.
            assert_eq!(raw.0, iova & 0x0000_ffff_ffff_f000);
            assert_eq!(
                raw.1,
                (1 << 63)
                    | (u64::from(!write) << 62)
                    | (u64::from(reason) << 32)
                    | u64::from(self.rid)
            );
            assert_eq!(read32(&mut self.dev, 0x034) & 3, 2);
        }
    }

    fn legacy(&mut self, levels: u8) {
        let context = LOWER + u64::from(self.rid & 255) * 16;
        put(
            &self.gm,
            ROOT + u64::from(self.rid >> 8) * 16,
            &[LOWER | 1, 0],
        );
        put(
            &self.gm,
            context,
            &[SL_ROOT | 1, u64::from(levels - 2) | (0xbeef << 8)],
        );
        write64(&mut self.dev, 0x020, ROOT);
        write32(&mut self.dev, 0x018, (1 << 30) | (1 << 31));
    }

    fn set_levels(&self, levels: u8) {
        let address = self.entries[3];
        put(
            &self.gm,
            address,
            &[(self.word(address) & !(7 << 2)) | (u64::from(levels - 2) << 2)],
        );
    }
}

#[test]
fn lookup_bus_devfn_and_rid_pasid_boundaries() {
    for bus in [0, 255] {
        for devfn in [0, 127, 128, 255] {
            for pasid in [0, 1, 63, 64, 8191, 8192, (1 << 20) - 1] {
                let f = Fixture::new((bus << 8) | devfn, pasid, 7);
                assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
                assert_eq!(f.translate(IOVA, true).unwrap(), GPA | 0xabc);
            }
        }
    }
}

#[test]
fn every_directory_size_last_valid_and_first_invalid_pasid() {
    for pdts in 0..=7 {
        let limit = 1 << (pdts + 13);
        let mut f = Fixture::new(0xffff, limit - 1, pdts);
        assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
        put(&f.gm, f.entries[1] + 8, &[u64::from(limit)]);
        // At 20 bits the next value is RID_PRIV, which is reserved in this profile.
        f.assert_fault(IOVA, false, if pdts == 7 { 0x42 } else { 0x43 }, false);
        f.fpd(1);
        f.assert_fault(IOVA, true, if pdts == 7 { 0x42 } else { 0x43 }, true);
    }
}

#[test]
fn only_selected_root_half_is_validated() {
    for devfn in [0, 127, 128, 255] {
        let f = Fixture::new(0xff00 | devfn, 64, 0);
        put(&f.gm, f.entries[0] ^ 8, &[u64::MAX]);
        assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
    }
}

#[test]
fn resolver_preserves_domain_ad_snoop_and_accumulated_fpd() {
    for levels in [3, 4] {
        for did in [0, 1, u16::MAX] {
            for ssade in [false, true] {
                for pwsnp in [false, true] {
                    for fpd in 0..8 {
                        let f = Fixture::new(0x7f80, 0xabcde, 7);
                        f.set_levels(levels);
                        f.fpd(fpd);
                        put(
                            &f.gm,
                            f.entries[3],
                            &[f.word(f.entries[3]) | (u64::from(ssade) << 9)],
                        );
                        put(
                            &f.gm,
                            f.entries[3] + 8,
                            &[u64::from(did) | (u64::from(pwsnp) << 23)],
                        );
                        let TranslationDescriptor::SecondStage(t) = f.resolve() else {
                            panic!("not second stage")
                        };
                        assert_eq!(t.root, SL_ROOT);
                        assert_eq!(t.levels, levels);
                        assert_eq!(t.domain_id, did);
                        assert_eq!(t.ssade, ssade);
                        assert_eq!(t.pwsnp, pwsnp);
                        assert_eq!(t.context.source_id, 0x7f80);
                        assert_eq!(t.context.iova, IOVA);
                        assert_eq!(t.context.mode, TranslationTableMode::SCALABLE);
                        assert_eq!(t.context.fpd, fpd != 0);
                    }
                }
            }
        }
    }
}

#[test]
fn pass_through_ignores_paging_configuration_but_preserves_domain() {
    for aw in 0..8 {
        for ssade in [0, 1] {
            for pwsnp in [0, 1] {
                let f = Fixture::new(0x0080, 63, 0);
                put(
                    &f.gm,
                    f.entries[3],
                    &[
                        UNMAPPED | 1 | (aw << 2) | (4 << 6) | (ssade << 9),
                        0xffff | (pwsnp << 23) | (1 << 24),
                    ],
                );
                let TranslationDescriptor::PassThrough { context, domain_id } = f.resolve() else {
                    panic!("not pass-through")
                };
                assert_eq!(domain_id, 0xffff);
                assert_eq!(context.mode, TranslationTableMode::SCALABLE);
                assert_eq!(f.translate(IOVA, false).unwrap(), IOVA);
            }
        }
    }
}

#[test]
fn absent_lookup_entries_ignore_other_fields_and_honor_local_fpd() {
    for stage in 0..4 {
        for fpd in 0..8 {
            for (ecap, pasid) in [
                (final_ecap(), 0xabcde),
                (
                    final_ecap()
                        .with_rps(false)
                        .with_ssts(false)
                        .with_ssads(false)
                        .with_smpwcs(false)
                        .with_sc(false),
                    0,
                ),
            ] {
                let mut f = Fixture::new(0xffff, pasid, 7);
                f.fpd(fpd);
                let address = f.entries[stage];
                let words = [1, 4, 1, 8][stage];
                for word in 0..words {
                    let value = if word == 0 {
                        !3 | (f.word(address) & 2)
                    } else {
                        u64::MAX
                    };
                    put(&f.gm, address + word * 8, &[value]);
                }
                let suppress = fpd & ((1 << stage) - 1) != 0;
                for write in [false, true] {
                    f.assert_fault_with_capabilities(
                        IOVA,
                        write,
                        [0x39, 0x41, 0x51, 0x59][stage],
                        suppress,
                        ecap,
                    );
                }
            }
        }
    }
}

#[test]
fn lookup_memory_errors_are_unqualified_even_after_parent_fpd() {
    for stage in 0..4 {
        for fpd in 0..8 {
            let mut f = Fixture::new(0xffff, 0xabcde, 7);
            f.fpd(fpd);
            if stage == 0 {
                write64(&mut f.dev, 0x020, UNMAPPED | (1 << 10));
                write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
            } else {
                let parent = f.entries[stage - 1];
                put(&f.gm, parent, &[UNMAPPED | (f.word(parent) & 0xfff)]);
            }
            for write in [false, true] {
                f.assert_fault(IOVA, write, [0x38, 0x40, 0x50, 0x58][stage], false);
            }
        }
    }
}

#[test]
fn every_lookup_reserved_and_unsupported_field_faults() {
    let masks = [
        (0, 0, 0xffff_0000_0000_0ffe),
        (1, 0, 0xffff_0000_0000_01fc),
        (1, 1, u64::MAX << 20),
        (1, 2, u64::MAX),
        (1, 3, u64::MAX),
        (2, 0, 0xffff_0000_0000_0ffc),
        (3, 0, 0xffff_0000_0000_0c20),
        (3, 1, 0xffff_ffff_fe7f_0000),
        (3, 2, !0x60),
        (3, 3, u64::MAX),
        (3, 4, u64::MAX),
        (3, 5, u64::MAX),
        (3, 6, u64::MAX),
        (3, 7, u64::MAX),
    ];
    for fpd in 0..8 {
        let mut f = Fixture::new(0xabff, 64, 0);
        f.fpd(fpd);
        for (stage, word, mask) in masks {
            let address = f.entries[stage] + word * 8;
            let original = f.word(address);
            for bit in 0..64 {
                if mask & (1u64 << bit) == 0 {
                    continue;
                }
                put(&f.gm, address, &[original | (1 << bit)]);
                f.assert_fault(
                    IOVA,
                    false,
                    [0x3a, 0x42, 0x52, 0x5a][stage],
                    fpd & ((1 << stage) - 1) != 0,
                );
                put(&f.gm, address, &[original]);
            }
        }
    }
}

#[test]
fn unsupported_pgtt_and_address_width_encodings() {
    for pgtt in 0..8 {
        for aw in 0..8 {
            let mut f = Fixture::new(0x120f, 0, 0);
            put(
                &f.gm,
                f.entries[3],
                &[SL_ROOT | 1 | (pgtt << 6) | (aw << 2)],
            );
            if pgtt == 4 {
                assert_eq!(f.translate(IOVA, false).unwrap(), IOVA);
            } else if pgtt == 2 && matches!(aw, 1 | 2) {
                assert!(matches!(f.resolve(), TranslationDescriptor::SecondStage(_)));
            } else {
                f.assert_fault(IOVA, true, 0x5b, false);
                f.fpd(4);
                f.assert_fault(IOVA, false, 0x5b, true);
            }
        }
    }
}

#[test]
fn capability_policy_respects_reserved_zero_fields_even_for_pass_through() {
    for pgtt in [2, 4] {
        for (ecap, stage, word, bit, reason) in [
            (final_ecap().with_rps(false), 1, 1, 0, 0x42),
            (final_ecap().with_ssads(false), 3, 0, 9, 0x5a),
            (final_ecap().with_smpwcs(false), 3, 1, 23, 0x5a),
            (final_ecap().with_sc(false), 3, 1, 24, 0x5a),
        ] {
            let mut f = Fixture::new(0x00ff, 0, 0);
            put(&f.gm, f.entries[3], &[SL_ROOT | 1 | (2 << 2) | (pgtt << 6)]);
            let address = f.entries[stage] + word * 8;
            put(&f.gm, address, &[f.word(address) | (1 << bit)]);
            f.assert_fault_with_capabilities(IOVA, false, reason, false, ecap);
        }
    }
    let mut f = Fixture::new(0, 0, 0);
    put(&f.gm, f.entries[3], &[1 | (2 << 6)]);
    f.assert_fault_with_capabilities(IOVA, false, 0x5b, false, final_ecap().with_ssts(false));
    put(&f.gm, f.entries[3], &[1 | (4 << 6)]);
    f.assert_fault_with_capabilities(IOVA, false, 0x5b, false, final_ecap().with_pt(false));
    put(&f.gm, f.entries[3], &[1 | (4 << 6) | (1 << 2)]);
    f.assert_fault_with_capabilities(IOVA, false, 0x5a, false, final_ecap().with_ssts(false));
}

#[test]
fn context_capability_validation_precedes_rid_pasid_range_check() {
    for pasid in [1, 8191, 8192, (1 << 20) - 1] {
        for fpd in 0..8 {
            let mut f = Fixture::new(0x12ff, pasid, 0);
            f.fpd(fpd);
            for present in [false, true] {
                let address = f.entries[1];
                put(
                    &f.gm,
                    address,
                    &[(f.word(address) & !1) | u64::from(present)],
                );
                for write in [false, true] {
                    f.assert_fault_with_capabilities(
                        IOVA,
                        write,
                        if present { 0x42 } else { 0x41 },
                        fpd & 1 != 0,
                        final_ecap().with_rps(false),
                    );
                    if !present || pasid >= 8192 {
                        f.assert_fault(
                            IOVA,
                            write,
                            if present { 0x43 } else { 0x41 },
                            fpd & 1 != 0,
                        );
                    } else {
                        assert_eq!(f.translate(IOVA, write).unwrap(), GPA | 0xabc);
                    }
                }
            }
        }
    }
}

#[test]
fn pasid_capability_validation_precedes_invalid_pgtt_and_aw() {
    for (ecap, lo, hi) in [
        (final_ecap().with_ssts(false), 7 << 2, 0),
        (final_ecap().with_ssts(false), SL_ROOT, 0),
        (final_ecap().with_ssts(false), 1 << 9, 0),
        (final_ecap().with_ssads(false), 1 << 9, 0),
        (final_ecap().with_smpwcs(false), 0, 1 << 23),
        (final_ecap().with_sc(false), 0, 1 << 24),
    ] {
        for (pgtt, aw) in [(2, 7), (0, 0), (1, 0), (3, 0), (7, 0)] {
            for fpd in 0..8 {
                let mut f = Fixture::new(0x12ff, 64, 0);
                for present in [false, true] {
                    put(
                        &f.gm,
                        f.entries[3],
                        &[
                            lo | (pgtt << 6) | (aw << 2) | u64::from(present),
                            0xbeef | hi,
                        ],
                    );
                    f.fpd(fpd);
                    for write in [false, true] {
                        f.assert_fault_with_capabilities(
                            IOVA,
                            write,
                            if present { 0x5a } else { 0x59 },
                            fpd != 0,
                            ecap,
                        );
                        f.assert_fault(IOVA, write, if present { 0x5b } else { 0x59 }, fpd != 0);
                    }
                }
            }
        }
    }
}

#[test]
fn zero_capability_reserved_fields_preserve_supported_translations() {
    for ecap in [
        final_ecap().with_rps(false),
        final_ecap().with_ssads(false),
        final_ecap().with_smpwcs(false),
        final_ecap().with_sc(false),
        final_ecap().with_ssts(false),
        final_ecap()
            .with_rps(false)
            .with_ssts(false)
            .with_ssads(false)
            .with_smpwcs(false)
            .with_sc(false),
    ] {
        for pgtt in [2, 4] {
            for fpd in 0..8 {
                let mut f = Fixture::new(0x12ff, 0, 0);
                let paging = if pgtt == 2 && ecap.ssts() {
                    SL_ROOT | (2 << 2)
                } else {
                    0
                };
                put(&f.gm, f.entries[3], &[paging | 1 | (pgtt << 6)]);
                f.fpd(fpd);
                for write in [false, true] {
                    if pgtt == 2 && !ecap.ssts() {
                        f.assert_fault_with_capabilities(IOVA, write, 0x5b, fpd != 0, ecap);
                    } else {
                        let gpa = f
                            .dev
                            .shared
                            .translator()
                            .translate_with_capabilities(f.rid, IOVA, write, ecap, |gpa| gpa)
                            .unwrap();
                        assert_eq!(gpa, if pgtt == 2 { GPA | 0xabc } else { IOVA });
                    }
                }
            }
        }
    }
}

#[test]
fn ad_enabled_dma_marks_validated_entries() {
    let f = Fixture::new(0, 0, 0);
    let addresses = walk(&f.gm, 4, 1, IOVA, GPA);
    let before: Vec<_> = addresses.iter().map(|address| f.word(*address)).collect();
    put(
        &f.gm,
        f.entries[3],
        &[f.word(f.entries[3]) | (1 << 9), 1 << 23],
    );
    assert_eq!(f.translate(IOVA, true).unwrap(), GPA | 0xabc);
    for (index, address) in addresses.iter().copied().enumerate() {
        assert_eq!(
            f.word(address),
            before[index] | if index == 3 { 0x300 } else { 0x100 }
        );
    }
}

#[test]
fn shared_walk_depths_page_sizes_offsets_and_high_output_addresses() {
    for legacy in [false, true] {
        for levels in [3, 4] {
            for leaf in [1, 2, 3] {
                let mut f = Fixture::new(0xff80, 64, 0);
                if legacy {
                    f.legacy(levels);
                } else {
                    f.set_levels(levels);
                }
                let size = 1u64 << (12 + 9 * (leaf - 1));
                let base = (1u64 << (12 + 9 * levels - 1)) & !(size - 1);
                let output = 1u64 << 47;
                for offset in [0, 0xabc, size - 1] {
                    let iova = base | offset;
                    let addresses = walk(&f.gm, levels, leaf, iova, output);
                    let before: Vec<_> = addresses.iter().map(|address| f.word(*address)).collect();
                    for write in [false, true] {
                        assert_eq!(f.translate(iova, write).unwrap(), output | offset);
                    }
                    assert_eq!(
                        before,
                        addresses
                            .iter()
                            .map(|address| f.word(*address))
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}

#[test]
fn walk_rejects_input_overflow_without_aliasing() {
    for legacy in [false, true] {
        for levels in [3, 4] {
            let mut f = Fixture::new(0xabcd, 64, 0);
            if legacy {
                f.legacy(levels);
            } else {
                f.set_levels(levels);
            }
            let limit = 1u64 << (12 + 9 * levels);
            walk(&f.gm, levels, 1, limit - 1, GPA);
            assert_eq!(f.translate(limit - 1, false).unwrap(), GPA | 0xfff);
            for iova in [limit, limit | 0xabc, 1 << 63, u64::MAX] {
                f.assert_fault(iova, false, if legacy { 4 } else { 0x84 }, false);
            }
        }
    }
}

#[test]
fn shared_walk_reserved_bits_and_misaligned_large_pages() {
    for legacy in [false, true] {
        for levels in [3, 4] {
            for leaf in [1, 2, 3] {
                let mut f = Fixture::new(0, 0, 0);
                if legacy {
                    f.legacy(levels);
                } else {
                    f.set_levels(levels);
                }
                let addresses = walk(&f.gm, levels, leaf, 0xabc, 1 << 30);
                for (index, address) in addresses.iter().copied().enumerate() {
                    let level = levels - index as u8;
                    let original = f.word(address);
                    let mut mask = (1 << 62) | (0xf << 48);
                    if level != leaf {
                        mask |= 1 << 11;
                    }
                    if level == 4 {
                        mask |= 1 << 7;
                    }
                    if level == leaf && leaf > 1 {
                        mask |= ((1 << (12 + 9 * (leaf - 1))) - 1) & !0xfff;
                    }
                    for bit in 0..64 {
                        if mask & (1u64 << bit) == 0 {
                            continue;
                        }
                        put(&f.gm, address, &[original | (1 << bit)]);
                        f.assert_fault(0xabc, false, if legacy { 0x0c } else { 0x7a }, false);
                        put(&f.gm, address, &[original]);
                    }
                }
            }
        }
    }
}

#[test]
fn shared_walk_ignored_bits_and_leaf_snoop_are_accepted() {
    for legacy in [false, true] {
        for levels in [3, 4] {
            for leaf in [1, 2, 3] {
                let mut f = Fixture::new(0, 0, 0);
                if legacy {
                    f.legacy(levels);
                } else {
                    f.set_levels(levels);
                }
                let addresses = walk(&f.gm, levels, leaf, 0xabc, 1 << 30);
                for (index, address) in addresses.iter().copied().enumerate() {
                    let level = levels - index as u8;
                    let ignored = 0xbff0_0000_0000_077c | if level == 1 { 1 << 7 } else { 0 };
                    put(
                        &f.gm,
                        address,
                        &[f.word(address) | ignored | if level == leaf { 1 << 11 } else { 0 }],
                    );
                }
                assert_eq!(f.translate(0xabc, true).unwrap(), (1 << 30) | 0xabc);
                f.assert_fault_with_capabilities(
                    0xabc,
                    false,
                    if legacy { 0x0c } else { 0x7a },
                    false,
                    final_ecap().with_sc(false),
                );
            }
        }
    }
}

#[test]
fn paging_memory_errors_distinguish_root_from_later_levels_and_honor_fpd() {
    for levels in [3, 4] {
        for failing_level in 1..=levels {
            for fpd in 0..8 {
                let mut f = Fixture::new(0x55aa, 63, 0);
                f.set_levels(levels);
                f.fpd(fpd);
                let addresses = walk(&f.gm, levels, 1, 0xabc, GPA);
                if failing_level == levels {
                    put(
                        &f.gm,
                        f.entries[3],
                        &[UNMAPPED | (f.word(f.entries[3]) & 0xfff)],
                    );
                } else {
                    let parent = addresses[usize::from(levels - failing_level - 1)];
                    put(&f.gm, parent, &[UNMAPPED | 3]);
                }
                f.assert_fault(
                    0xabc,
                    false,
                    if failing_level == levels { 0x7b } else { 0x78 },
                    fpd != 0,
                );
            }
        }
    }
}

#[test]
fn permissions_are_accumulated_and_not_present_is_distinct_in_scalable_mode() {
    for legacy in [false, true] {
        for leaf in [1, 2, 3] {
            let mut f = Fixture::new(0, 0, 0);
            if legacy {
                f.legacy(4);
            }
            for parent_permissions in 0..4 {
                for leaf_permissions in 0..4 {
                    let addresses = walk(&f.gm, 4, leaf, 0xabc, 1 << 30);
                    for (address, permissions) in [
                        (addresses[0], parent_permissions),
                        (*addresses.last().unwrap(), leaf_permissions),
                    ] {
                        put(&f.gm, address, &[(f.word(address) & !3) | permissions]);
                    }
                    let permissions = parent_permissions & leaf_permissions;
                    for write in [false, true] {
                        if permissions & if write { 2 } else { 1 } != 0 {
                            assert_eq!(f.translate(0xabc, write).unwrap(), (1 << 30) | 0xabc);
                        } else {
                            let reason = if legacy {
                                if write { 5 } else { 6 }
                            } else if permissions == 0 {
                                0x79
                            } else if write {
                                0x85
                            } else {
                                0x86
                            };
                            f.assert_fault(0xabc, write, reason, false);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn absent_paging_entries_ignore_reserved_bits() {
    for level in 1..=4 {
        let mut f = Fixture::new(0xffff, 0, 0);
        let addresses = walk(&f.gm, 4, 1, IOVA, GPA);
        put(&f.gm, addresses[4 - level], &[!3]);
        f.assert_fault(IOVA, false, 0x79, false);
        f.fpd(4);
        f.assert_fault(IOVA, true, 0x79, true);
    }
}

#[test]
fn invalid_walk_levels_are_rejected_before_index_arithmetic() {
    let f = Fixture::new(0, 0, 0);
    for levels in [0, 1, 2, 5, 6, 255] {
        let translation = SecondStageTranslation {
            context: f.translation_context(IOVA),
            root: SL_ROOT,
            levels,
            domain_id: 0,
            ssade: false,
            pwsnp: false,
        };
        assert_eq!(
            f.dev
                .shared
                .walk_sl_page_table(&translation, false, final_ecap())
                .unwrap_err()
                .fault_reason()
                .0,
            0x5b
        );
    }
}

#[test]
fn checked_lookup_arithmetic_cannot_wrap_or_exceed_host_width() {
    let mut f = Fixture::new(0xffff, 0xfffff, 7);
    for root in [1 << 48, !0xfff] {
        write64(&mut f.dev, 0x020, root | (1 << 10));
        write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
        f.assert_fault(IOVA, true, 0x38, false);
    }
    for (base, index) in [(u64::MAX - 3, 1), (0, u64::MAX), ((1 << 48) - 4, 0)] {
        let fault = f
            .dev
            .shared
            .read_translation_entry::<PasidDirectoryEntry>(
                base,
                index,
                f.translation_context(IOVA),
                FaultReason::PASID_DIRECTORY_ACCESS_ERROR,
            )
            .unwrap_err();
        assert_eq!(fault.fault_reason().0, 0x50);
        assert_eq!(fault.fault_address(), IOVA);
    }
    // The directory can span multiple pages; its final indexed entry still
    // must fit HAW, even when its base pointer itself is valid.
    let mut f = Fixture::new(0xffff, 0xfffff, 7);
    put(&f.gm, f.entries[1], &[((1 << 48) - 4096) | 1 | (7 << 9)]);
    f.assert_fault(IOVA, false, 0x50, false);
}

#[test]
fn pass_through_and_walk_outputs_cannot_enter_interrupt_address_range() {
    for pass_through in [false, true] {
        for leaf in [1, 2, 3] {
            let mut f = Fixture::new(0, 0, 0);
            if pass_through {
                put(&f.gm, f.entries[3], &[1 | (4 << 6)]);
            }
            let size = 1u64 << (12 + 9 * (leaf - 1));
            for gpa in [
                0xfee0_0000,
                0xfeef_ffff,
                0xfedf_ffff,
                0xfef0_0000,
                0x1_fee0_0000,
            ] {
                walk(&f.gm, 4, leaf, gpa, gpa & !(size - 1));
                if (0xfee0_0000..=0xfeef_ffff).contains(&gpa) {
                    f.assert_fault(gpa, false, 0x87, false);
                } else {
                    assert_eq!(f.translate(gpa, false).unwrap(), gpa);
                }
            }
        }
    }
}

#[test]
fn pass_through_host_width_is_checked_after_fpd_lookup() {
    for fpd in 0..8 {
        let mut f = Fixture::new(0x0180, 63, 0);
        put(&f.gm, f.entries[3], &[1 | (4 << 6)]);
        f.fpd(fpd);
        assert_eq!(f.translate((1 << 48) - 1, false).unwrap(), (1 << 48) - 1);
        for iova in [1 << 48, u64::MAX] {
            f.assert_fault(iova, true, 0x83, fpd != 0);
        }
    }
}

#[test]
fn raw_mode_and_root_do_not_take_effect_before_srtp() {
    let mut f = Fixture::new(0x0080, 64, 0);
    write64(&mut f.dev, 0x020, UNMAPPED | (2 << 10));
    assert_eq!(read64(&mut f.dev, 0x020), UNMAPPED | (2 << 10));
    assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
    // Re-enabling without SRTP does not accidentally latch the raw register.
    write32(&mut f.dev, 0x018, 0);
    assert_eq!(f.translate(u64::MAX, true).unwrap(), u64::MAX);
    write32(&mut f.dev, 0x018, 1 << 31);
    assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
    write32(&mut f.dev, 0x018, 0);
    write32(&mut f.dev, 0x018, 1 << 30);
    write32(&mut f.dev, 0x018, 1 << 31);
    f.assert_fault(IOVA, false, 0x30, false);
    // Both a new root and a new mode become active on the next SRTP.
    write32(&mut f.dev, 0x018, 0);
    f.legacy(4);
    assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
    write64(&mut f.dev, 0x020, UNMAPPED | (1 << 10));
    assert_eq!(
        f.dev
            .shared
            .translator()
            .translate(f.rid, IOVA, false, |gpa| gpa)
            .unwrap(),
        GPA | 0xabc,
    );
    write32(&mut f.dev, 0x018, 0);
    write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
    f.assert_fault_with_capabilities(IOVA, true, 0x30, false, EcapReg::from(ECAP_VALUE));
}

#[test]
fn production_capabilities_gate_scalable_and_reserved_modes_without_dma() {
    for mode in [1, 2, 3] {
        let mut f = Fixture::new(0xff80, 0, 0);
        f.fpd(7);
        write64(&mut f.dev, 0x020, ROOT | (mode << 10));
        write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
        f.assert_fault_with_capabilities(IOVA, false, 0x30, false, EcapReg::from(ECAP_VALUE));
        let fault = f
            .dev
            .shared
            .translator()
            .translate(f.rid, IOVA, true, |_| {
                panic!("production DMA in unsupported mode")
            })
            .unwrap_err();
        assert_eq!(fault.error.fault_reason().0, 0x30);
        if mode != 1 {
            f.assert_fault(IOVA, true, 0x30, false);
        }
        write32(&mut f.dev, 0x018, 0);
        assert_eq!(
            f.dev
                .shared
                .translator()
                .translate(f.rid, u64::MAX, true, |gpa| gpa)
                .unwrap(),
            u64::MAX
        );
        assert_eq!(read64(&mut f.dev, 0x010), 0x00f0_10db);
    }
}

#[test]
fn legacy_lookup_faults_preserve_original_rid_address_and_absent_fpd() {
    for stage in [0, 1] {
        for absent in [false, true] {
            let mut f = Fixture::new(0xffff, 0, 0);
            f.legacy(4);
            let root = ROOT + 255 * 16;
            let context = LOWER + 255 * 16;
            if absent {
                put(
                    &f.gm,
                    if stage == 0 { root } else { context },
                    &[!3, u64::MAX],
                );
            } else if stage == 0 {
                write64(&mut f.dev, 0x020, UNMAPPED);
                write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
            } else {
                put(&f.gm, root, &[UNMAPPED | 1]);
            }
            f.assert_fault(
                IOVA,
                false,
                match (stage, absent) {
                    (0, true) => 1,
                    (0, false) => 8,
                    (_, true) => 2,
                    (_, false) => 9,
                },
                false,
            );
            if stage == 1 && absent {
                put(&f.gm, context, &[!1]);
                f.assert_fault(IOVA, true, 2, true);
            }
        }
    }
}

#[test]
fn legacy_supported_aw_pass_through_ignored_pointer_and_context_bits() {
    for aw in 0..8 {
        let mut f = Fixture::new(0, 0, 0);
        f.legacy(4);
        put(&f.gm, LOWER + 8, &[aw | (0xf << 3)]);
        if matches!(aw, 1 | 2) {
            walk(&f.gm, aw as u8 + 2, 1, 0xabc, GPA);
            assert_eq!(f.translate(0xabc, false).unwrap(), GPA | 0xabc);
        } else {
            f.assert_fault(0xabc, false, 3, false);
        }
    }
    let mut f = Fixture::new(0, 0, 0);
    f.legacy(4);
    put(&f.gm, LOWER, &[!0xfff | 1 | (2 << 2), 2]);
    assert_eq!(f.translate(IOVA, false).unwrap(), IOVA);
    f.assert_fault(1 << 48, true, 4, false);
    f.assert_fault(0xfee0_0123, true, 0x0e, false);
}

#[test]
fn legacy_pass_through_supported_widths_are_identity_without_a_walk() {
    for aw in [1, 2] {
        for pointer in [0, SL_ROOT, UNMAPPED, 0xffff_ffff_ffff_f000] {
            for fpd in [0, 1] {
                let mut f = Fixture::new(0xabcd, 0, 0);
                f.legacy(4);
                assert_eq!((read64(&mut f.dev, 0x008) >> 8) & 0x1f, 0b00110);
                let context = LOWER + 0xcd * 16;
                // AW=1, lo=9 is deliberately tolerated, not the §9.3
                // requirement to program the largest supported AW (2).
                put(
                    &f.gm,
                    context,
                    &[pointer | 0x9 | (fpd << 1), aw | (0xbeef << 8)],
                );
                // If the ignored pointer is followed, this is not identity.
                walk(&f.gm, 4, 1, GPA, 0x90000);
                let mut before = vec![0; UNMAPPED as usize];
                f.gm.read_at(0, &mut before).unwrap();
                for offset in [0, 1, 0xabc, 0xfff] {
                    for write in [false, true] {
                        assert_eq!(f.translate_legacy(GPA | offset, write), GPA | offset);
                    }
                }
                let mut after = vec![0; UNMAPPED as usize];
                f.gm.read_at(0, &mut after).unwrap();
                assert!(before == after, "pass-through changed guest memory");
                assert_eq!(read32(&mut f.dev, 0x034) & 3, 0);
            }
        }
    }
}

#[test]
fn legacy_pass_through_dma_reads_and_writes_only_identity_memory() {
    for aw in [1, 2] {
        let mut f = Fixture::new(0xabcd, 0, 0);
        f.legacy(4);
        put(
            &f.gm,
            LOWER + 0xcd * 16,
            &[UNMAPPED | 0x9, aw | (0xbeef << 8)],
        );
        let offsets = [0, 1, 0xabc, 0xfff];
        for offset in offsets {
            f.gm.write_at(GPA | offset, &[0xa5]).unwrap();
        }
        let mut expected = vec![0; UNMAPPED as usize];
        f.gm.read_at(0, &mut expected).unwrap();
        let translator = f.dev.shared.translator();
        for offset in offsets {
            let iova = GPA | offset;
            let value = translator
                .translate(f.rid, iova, false, |gpa| {
                    assert_eq!(gpa, iova);
                    assert!(f.dev.shared.state.try_write().is_none());
                    f.gm.read_plain::<u8>(gpa).unwrap()
                })
                .unwrap();
            assert_eq!(value, 0xa5);
            translator
                .translate(f.rid, iova, true, |gpa| {
                    assert_eq!(gpa, iova);
                    assert!(f.dev.shared.state.try_write().is_none());
                    f.gm.write_at(gpa, &[0x5a]).unwrap();
                })
                .unwrap();
            expected[iova as usize] = 0x5a;
        }
        let mut actual = vec![0; UNMAPPED as usize];
        f.gm.read_at(0, &mut actual).unwrap();
        assert!(actual == expected, "DMA changed memory outside its payload");
        assert_eq!(read32(&mut f.dev, 0x034) & 3, 0);
    }
}

#[test]
fn legacy_pass_through_preserves_host_and_interrupt_address_bounds() {
    for aw in [1, 2] {
        for fpd in [0, 1] {
            let mut f = Fixture::new(0xffff, 0, 0);
            f.legacy(4);
            put(
                &f.gm,
                LOWER + 255 * 16,
                &[0x9 | (fpd << 1), aw | (0xbeef << 8) | 0x78],
            );
            for write in [false, true] {
                // §3.9 and Table 26 LGN.1.3 check HAW, not AW=39 paging bounds.
                for iova in [
                    0,
                    (1 << 39) - 1,
                    1 << 39,
                    (1 << 39) | 0xabc,
                    (1 << 48) - 1,
                    0xfedf_ffff,
                    0xfef0_0000,
                    0x1_fee0_0000,
                ] {
                    assert_eq!(f.translate_legacy(iova, write), iova);
                }
                for iova in [1 << 48, (1 << 48) | 0xabc, (1 << 63) | 0xabc, u64::MAX] {
                    f.assert_legacy_fault(iova, write, 0x04, fpd != 0);
                }
                for iova in [0xfee0_0000, 0xfee0_0123, 0xfeef_ffff] {
                    f.assert_legacy_fault(iova, write, 0x0e, fpd != 0);
                }
            }
        }
    }
}

#[test]
fn legacy_unsupported_widths_and_reserved_translation_type_fault() {
    for tt in [0, 2, 3] {
        for aw in 0..8 {
            if tt != 3 && matches!(aw, 1 | 2) {
                continue;
            }
            for fpd in [0, 1] {
                let mut f = Fixture::new(0x12ff, 0, 0);
                f.legacy(4);
                put(
                    &f.gm,
                    LOWER + 255 * 16,
                    &[SL_ROOT | 1 | (tt << 2) | (fpd << 1), aw | (0xbeef << 8)],
                );
                for write in [false, true] {
                    f.assert_legacy_fault(0x12_3456_7abc, write, 0x03, fpd != 0);
                }
            }
        }
    }
}

#[test]
fn legacy_present_root_reserved_fields_fault_before_context_fpd() {
    for fpd in [0, 1] {
        let mut f = Fixture::new(0xabcd, 0, 0);
        f.legacy(4);
        put(&f.gm, LOWER + 0xcd * 16, &[0x9 | (fpd << 1), 2]);
        let root = ROOT + 0xab * 16;
        // §9.1: low bits 11:1 and 63:HAW, and the entire upper word.
        for bit in (1..12).chain(48..128) {
            let words = if bit < 64 {
                [LOWER | 1 | (1 << bit), 0]
            } else {
                [LOWER | 1, 1 << (bit - 64)]
            };
            put(&f.gm, root, &words);
            for write in [false, true] {
                // Table 26 LRT.3 is unqualified; the context isn't consumed.
                f.assert_legacy_fault(IOVA, write, 0x0a, false);
            }
        }
    }
}

#[test]
fn legacy_present_context_reserved_fields_honor_fpd() {
    for tt in [0, 2] {
        for fpd in [0, 1] {
            let mut f = Fixture::new(0xabcd, 0, 0);
            f.legacy(4);
            let context = LOWER + 0xcd * 16;
            let lo = SL_ROOT | 1 | (tt << 2) | (fpd << 1);
            let hi = 2 | (0xbeef << 8) | 0x78;
            // §9.3: bits 11:4, 71 and 127:88. Bits 70:67 are ignored.
            for bit in (4..12).chain([71]).chain(88..128) {
                let words = if bit < 64 {
                    [lo | (1 << bit), hi]
                } else {
                    [lo, hi | (1 << (bit - 64))]
                };
                put(&f.gm, context, &words);
                for write in [false, true] {
                    // Table 26 LCT.3 is qualified, even for pass-through.
                    f.assert_legacy_fault(IOVA, write, 0x0b, fpd != 0);
                }
            }
        }
    }
}

#[test]
fn legacy_ssptptr_above_haw_is_reserved_not_an_access_error() {
    for aw in [1, 2] {
        for fpd in [0, 1] {
            let mut f = Fixture::new(0xabcd, 0, 0);
            f.legacy(4);
            for bit in 48..64 {
                put(
                    &f.gm,
                    LOWER + 0xcd * 16,
                    &[SL_ROOT | 1 | (fpd << 1) | (1 << bit), aw | (0xbeef << 8)],
                );
                for write in [false, true] {
                    // §9.3 reserves SSPTPTR[63:HAW] for translated contexts.
                    f.assert_legacy_fault(0x12_3456_7abc, write, 0x0b, fpd != 0);
                }
            }
        }
    }
}

#[test]
fn legacy_lookup_access_errors_cannot_use_unread_context_fpd() {
    for fpd in [0, 1] {
        for stage in [0, 1] {
            let mut f = Fixture::new(0xabcd, 0, 0);
            f.legacy(4);
            put(&f.gm, LOWER + 0xcd * 16, &[0x9 | (fpd << 1), 2]);
            for pointer in [UNMAPPED, (1 << 48) - 4096] {
                if stage == 0 {
                    write64(&mut f.dev, 0x020, pointer);
                    write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
                } else {
                    put(&f.gm, ROOT + 0xab * 16, &[pointer | 1, 0]);
                }
                for write in [false, true] {
                    // Table 26 LRT.1/LCT.1 are unqualified, unlike LCT.3.
                    f.assert_legacy_fault(IOVA, write, if stage == 0 { 8 } else { 9 }, false);
                }
            }
        }
    }
}

#[test]
fn legacy_sl_access_errors_distinguish_ssptptr_from_later_tables() {
    for levels in [3, 4] {
        for failing_level in 1..=levels {
            for fpd in [0, 1] {
                let mut f = Fixture::new(0xabcd, 0, 0);
                f.legacy(levels);
                let iova = 0x12_3456_7abc;
                let addresses = walk(&f.gm, levels, 1, iova, GPA);
                let context = LOWER + 0xcd * 16;
                put(&f.gm, context, &[SL_ROOT | 1 | (fpd << 1)]);
                if failing_level == levels {
                    put(&f.gm, context, &[UNMAPPED | 1 | (fpd << 1)]);
                } else {
                    let parent = addresses[usize::from(levels - failing_level - 1)];
                    put(&f.gm, parent, &[UNMAPPED | 3]);
                }
                // Table 26: LCT.4.3 (SSPTPTR) is 03h; LSS.1 (ADDR) is 07h.
                for write in [false, true] {
                    f.assert_legacy_fault(
                        iova,
                        write,
                        if failing_level == levels { 3 } else { 7 },
                        fpd != 0,
                    );
                }
            }
        }
    }
}

#[test]
fn legacy_walk_input_width_does_not_limit_host_output_width() {
    for (levels, width) in [(3, 39), (4, 48)] {
        for fpd in [0, 1] {
            let mut f = Fixture::new(0xabcd, 0, 0);
            f.legacy(levels);
            put(&f.gm, LOWER + 0xcd * 16, &[SL_ROOT | 1 | (fpd << 1)]);
            let limit = 1u64 << width;
            walk(&f.gm, levels, 1, limit - 1, (1 << 48) - 4096);
            for write in [false, true] {
                assert_eq!(f.translate_legacy(limit - 1, write), (1 << 48) - 1);
                for iova in [limit, limit | 0xabc, (1 << 63) | 0xabc, u64::MAX] {
                    f.assert_legacy_fault(iova, write, 0x04, fpd != 0);
                }
            }
        }
    }
}

#[test]
fn legacy_second_stage_addresses_above_haw_honor_fpd() {
    for levels in [3, 4] {
        for fpd in [0, 1] {
            let mut f = Fixture::new(0xabcd, 0, 0);
            f.legacy(levels);
            put(&f.gm, LOWER + 0xcd * 16, &[SL_ROOT | 1 | (fpd << 1)]);
            let iova = 0x12_3456_7abc;
            let addresses = walk(&f.gm, levels, 1, iova, GPA);
            for address in addresses {
                let original = f.word(address);
                // §9.8 reserves ADDR[51:HAW] at both table and leaf entries.
                for bit in 48..52 {
                    put(&f.gm, address, &[original | (1 << bit)]);
                    for write in [false, true] {
                        f.assert_legacy_fault(iova, write, 0x0c, fpd != 0);
                    }
                }
                put(&f.gm, address, &[original]);
            }
        }
    }
}

#[test]
fn independent_roots_devices_domains_and_pasid_entries_do_not_alias() {
    let mut f = Fixture::new(0x007f, 63, 0);
    let root2 = 0x8000;
    let context2 = 0x9000;
    let directory2 = 0x60000;
    let pasid2 = 0x80000;
    let sl2 = 0x90000;
    let rid2 = 0xff80;
    put(&f.gm, root2 + 255 * 16 + 8, &[context2 | 1]);
    put(&f.gm, context2, &[directory2 | 1, 64, 0, 0]);
    put(&f.gm, directory2 + 8, &[pasid2 | 1]);
    put(
        &f.gm,
        pasid2,
        &[sl2 | 1 | (1 << 2) | (2 << 6), 0x1234, 0, 0, 0, 0, 0, 0],
    );
    put(&f.gm, sl2, &[(1 << 30) | 0x83]);
    // A second devfn on the first bus uses an independent PASID hierarchy too.
    put(&f.gm, LOWER, &[directory2 | 1, 64, 0, 0]);
    for _ in 0..2 {
        let TranslationDescriptor::SecondStage(t) = f.resolve() else {
            panic!("not second stage")
        };
        assert_eq!(t.domain_id, 0xbeef);
        assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
        let original_rid = f.rid;
        f.rid = 0;
        assert_eq!(f.translate(0xabc, false).unwrap(), (1 << 30) | 0xabc);
        f.rid = rid2;
        write64(&mut f.dev, 0x020, root2 | (1 << 10));
        write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
        let TranslationDescriptor::SecondStage(t) = f
            .dev
            .shared
            .resolve_scalable(root2, f.translation_context(0xabc), final_ecap())
            .unwrap()
        else {
            panic!("not second stage")
        };
        assert_eq!(t.domain_id, 0x1234);
        assert_eq!(f.translate(0xabc, true).unwrap(), (1 << 30) | 0xabc);
        f.rid = original_rid;
        write64(&mut f.dev, 0x020, ROOT | (1 << 10));
        write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
    }
}

#[test]
fn pass_through_still_rejects_unsupported_optional_fields() {
    let mut f = Fixture::new(0xffff, 0xfffff, 7);
    put(&f.gm, f.entries[3], &[1 | (4 << 6)]);
    for (word, bits) in [
        (0, 0xffff_0000_0000_0c20u64),
        (1, 0xffff_ffff_fe7f_0000),
        (2, !0x60),
        (3, u64::MAX),
        (4, u64::MAX),
        (5, u64::MAX),
        (6, u64::MAX),
        (7, u64::MAX),
    ] {
        let address = f.entries[3] + word * 8;
        let original = f.word(address);
        for bit in 0..64 {
            if bits & (1 << bit) == 0 {
                continue;
            }
            put(&f.gm, address, &[original | (1 << bit)]);
            f.assert_fault(IOVA, true, 0x5a, false);
            put(&f.gm, address, &[original]);
        }
    }
    put(&f.gm, f.entries[3] + 16, &[0x60]);
    assert_eq!(f.translate(IOVA, false).unwrap(), IOVA);
}

#[test]
fn qualified_walk_faults_honor_each_parent_and_local_fpd_combination() {
    for fpd in 0..8 {
        for (pte, iova, write, reason) in [
            (GPA, IOVA, false, 0x79),
            (GPA | 1, IOVA, true, 0x85),
            (GPA | 2, IOVA, false, 0x86),
            (GPA | 3 | (1 << 62), IOVA, false, 0x7a),
            (GPA | 3, IOVA | (1 << 48), false, 0x84),
            (0xfee0_0003, IOVA, true, 0x87),
        ] {
            let mut f = Fixture::new(0xffff, 0xfffff, 7);
            f.fpd(fpd);
            let addresses = walk(&f.gm, 4, 1, IOVA, GPA);
            put(&f.gm, *addresses.last().unwrap(), &[pte]);
            f.assert_fault(iova, write, reason, fpd != 0);
        }
    }
}

#[test]
fn all_lookup_bytes_must_be_readable_before_using_entry_fields() {
    // Entries are naturally contained in pages. Use a bounded GuestMemory
    // region ending within each entry to exercise partial reads.
    for (stage, size) in [(0, 16), (1, 32), (2, 8), (3, 64)] {
        let f = Fixture::new(0, 0, 0);
        let end = f.entries[stage] + size - 1;
        let gm = f.gm.subrange(0, end, false).unwrap();
        let (mut dev, shared) = IntelVtdDevice::new(
            gm,
            IntelVtdConfig {
                mmio_base: TEST_MMIO_BASE,
            },
            Arc::new(TestSignalMsi),
        );
        write64(&mut dev, 0x020, ROOT | (1 << 10));
        write32(&mut dev, 0x018, (1 << 30) | (1 << 31));
        let fault = shared
            .translator()
            .translate_with_capabilities(0, IOVA, false, final_ecap(), |_| {
                panic!("DMA after a partial lookup read")
            })
            .unwrap_err();
        let reason = [0x38, 0x40, 0x50, 0x58][stage];
        assert_eq!(fault.error.fault_reason().0, reason);
        assert_eq!(read64(&mut dev, 0x120), IOVA & 0x0000_ffff_ffff_f000);
        assert_eq!(
            read64(&mut dev, 0x128),
            (3 << 62) | (u64::from(reason) << 32)
        );
    }
}
