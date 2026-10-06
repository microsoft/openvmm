// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use std::sync::Barrier;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use test_with_tracing::test;

const A: u64 = 1 << 8;
const D: u64 = 1 << 9;
const IGNORED: u64 = (1 << 63) | (1 << 52) | (1 << 10) | (1 << 6) | (1 << 2);
const ALTERNATE: u64 = 0x60000;
const QUEUE: u64 = 0x80000;
const STATUS: u64 = 0x90000;

fn enable_ad(f: &Fixture, ssade: bool, pwsnp: bool) {
    put(
        &f.gm,
        f.entries[3],
        &[
            (f.word(f.entries[3]) & !(1 << 9)) | (u64::from(ssade) << 9),
            (f.word(f.entries[3] + 8) & !(1 << 23)) | (u64::from(pwsnp) << 23),
        ],
    );
}

fn bytes(gm: &GuestMemory, base: u64, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    gm.read_at(base, &mut bytes).unwrap();
    bytes
}

fn expected_word(bytes: &mut [u8], address: u64, word: u64) {
    let offset = (address - SL_ROOT) as usize;
    bytes[offset..offset + 8].copy_from_slice(&word.to_le_bytes());
}

fn cas(gm: &GuestMemory, address: u64, current: u64, new: u64) -> Result<bool, GuestMemoryError> {
    gm.compare_exchange::<u64>(address, current.to_le(), new.to_le())
        .map(|result| result.is_ok())
}

fn injected<R>(
    shared: &Arc<VtdSharedState>,
    context: TranslationContext,
    write: bool,
    compare_exchange: impl FnMut(u64, u64, u64) -> Result<bool, GuestMemoryError>,
    op: impl FnOnce(u64) -> R,
) -> Result<R, iommu_common::TranslationFault<VtdFault>> {
    shared.translator().translate_with(
        context.iova,
        write,
        |state| {
            assert!(state.gsts.tes());
            assert_eq!(state.latched_rtaddr.ttm(), 1);
            let TranslationDescriptor::SecondStage(translation) = shared.resolve_scalable(
                state.latched_rtaddr.root_table_address(),
                context,
                final_ecap(),
            )?
            else {
                panic!("expected second-stage translation");
            };
            shared.walk_sl_page_table_with_cas(&translation, write, final_ecap(), compare_exchange)
        },
        op,
    )
}

fn injected_fault(
    f: &mut Fixture,
    write: bool,
    reason: u8,
    suppressed: bool,
    compare_exchange: impl FnMut(u64, u64, u64) -> Result<bool, GuestMemoryError>,
) -> VtdFault {
    write32(&mut f.dev, 0x12c, 1 << 31);
    write32(&mut f.dev, 0x034, u32::MAX);
    let before = (read64(&mut f.dev, 0x120), read64(&mut f.dev, 0x128));
    let fault = injected(
        &f.dev.shared,
        f.translation_context(IOVA),
        write,
        compare_exchange,
        |_| panic!("DMA closure ran after an A/D failure"),
    )
    .unwrap_err();
    assert_eq!(fault.iova, IOVA);
    assert_eq!(fault.error.source_id(), f.rid);
    assert_eq!(fault.error.fault_address(), IOVA);
    assert_eq!(fault.error.fault_reason().0, reason, "{fault:?}");
    assert_eq!(fault.error.fpd(), suppressed, "{fault:?}");
    let raw = (read64(&mut f.dev, 0x120), read64(&mut f.dev, 0x128));
    if suppressed {
        assert_eq!(raw, before);
        assert_eq!(read32(&mut f.dev, 0x034) & 3, 0);
    } else {
        assert_eq!(raw.0, IOVA & 0x0000_ffff_ffff_f000);
        assert_eq!(
            raw.1,
            (1 << 63) | (u64::from(!write) << 62) | (u64::from(reason) << 32) | u64::from(f.rid)
        );
        assert_eq!(read32(&mut f.dev, 0x034) & 3, 2);
    }
    fault.error
}

#[test]
fn matrix_96_cases_exact_words_neighbors_and_offsets() {
    let mut cases = 0;
    let mut walks = 0;
    for levels in [3, 4] {
        for leaf in [1, 2, 3] {
            for write in [false, true] {
                for ssade in [false, true] {
                    for leaf_ad in [0, A, D, A | D] {
                        cases += 1;
                        let f = Fixture::new(0xab80, 64, 0);
                        f.set_levels(levels);
                        enable_ad(&f, ssade, true);
                        let size = 1u64 << (12 + 9 * (leaf - 1));
                        let base = 0x12_3456_7000 & !(size - 1);
                        let output = 1 << 47;
                        // Each ancestor's A and ignored D vary independently,
                        // independently also of the leaf's initial A/D.
                        for nonleaf_ad in 0..(1 << (2 * (levels - leaf))) {
                            for offset in [0, 0xabc, size - 1] {
                                f.gm.write_at(SL_ROOT, &[0xa5; 4 * 4096]).unwrap();
                                let entries = walk(&f.gm, levels, leaf, base | offset, output);
                                for (index, &address) in entries.iter().enumerate() {
                                    let flags = if index == entries.len() - 1 {
                                        leaf_ad
                                    } else {
                                        ((nonleaf_ad >> (2 * index)) & 3) << 8
                                    };
                                    put(&f.gm, address, &[f.word(address) | flags | IGNORED]);
                                }
                                let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
                                for (index, &address) in entries.iter().enumerate() {
                                    let mark = if ssade {
                                        A | if write && index == entries.len() - 1 {
                                            D
                                        } else {
                                            0
                                        }
                                    } else {
                                        0
                                    };
                                    expected_word(&mut expected, address, f.word(address) | mark);
                                }
                                assert_eq!(
                                    f.translate(base | offset, write).unwrap(),
                                    output | offset
                                );
                                assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
                                walks += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cases, 96);
    assert_eq!(walks, 5040);
}

#[test]
fn legacy_disabled_and_pass_through_never_mark_entries() {
    for mode in 0..5 {
        for write in [false, true] {
            for initial in [0, A, D, A | D] {
                let mut f = Fixture::new(0x0180, 64, 0);
                enable_ad(&f, true, true);
                let entries = walk(&f.gm, 4, 1, IOVA, GPA);
                for &address in &entries {
                    put(&f.gm, address, &[f.word(address) | initial | IGNORED]);
                }
                let expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
                let output = match mode {
                    0..=2 => {
                        f.legacy(4);
                        let address = LOWER + u64::from(f.rid & 255) * 16;
                        put(&f.gm, address, &[f.word(address) | (mode << 2)]);
                        if mode == 2 { IOVA } else { GPA | 0xabc }
                    }
                    3 => {
                        write32(&mut f.dev, 0x018, 0);
                        IOVA
                    }
                    4 => {
                        put(
                            &f.gm,
                            f.entries[3],
                            &[(f.word(f.entries[3]) & !(7 << 6)) | (4 << 6)],
                        );
                        IOVA
                    }
                    _ => unreachable!(),
                };
                assert_eq!(f.translate(IOVA, write).unwrap(), output);
                assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
            }
        }
    }
}

#[test]
fn permission_checks_at_every_level_never_dirty_denied_writes() {
    for levels in [3, 4] {
        for leaf in [1, 2, 3] {
            for restricted in 0..=levels - leaf {
                for permissions in 0..4 {
                    for write in [false, true] {
                        for sticky in [0, D] {
                            let mut f = Fixture::new(0xab80, 64, 0);
                            f.set_levels(levels);
                            enable_ad(&f, true, true);
                            let entries = walk(&f.gm, levels, leaf, 0xabc, 1 << 30);
                            for &address in &entries {
                                put(&f.gm, address, &[f.word(address) | sticky | IGNORED]);
                            }
                            let address = entries[usize::from(restricted)];
                            put(&f.gm, address, &[(f.word(address) & !3) | permissions]);
                            let before: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
                            let permitted = permissions & if write { 2 } else { 1 } != 0;
                            if permitted {
                                assert_eq!(f.translate(0xabc, write).unwrap(), (1 << 30) | 0xabc);
                            } else {
                                let reason = if permissions == 0 {
                                    0x79
                                } else if write {
                                    0x85
                                } else {
                                    0x86
                                };
                                f.assert_fault(0xabc, write, reason, false);
                            }
                            for (index, &address) in entries.iter().enumerate() {
                                let used = if permissions == 0 {
                                    index < usize::from(restricted)
                                } else {
                                    permitted || index < entries.len() - 1
                                };
                                let mark = if used { A } else { 0 }
                                    | if permitted && write && index == entries.len() - 1 {
                                        D
                                    } else {
                                        0
                                    };
                                assert_eq!(f.word(address), before[index] | mark);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn malformed_walks_allow_only_prior_accessed_updates() {
    for levels in [3, 4] {
        for leaf in [1, 2, 3] {
            for failing in 0..=levels - leaf {
                let level = levels - failing;
                let mut invalid_bits = vec![1 << 62, 1 << 48, 1 << 51];
                if level != leaf {
                    invalid_bits.push(1 << 11);
                }
                if level == 4 {
                    invalid_bits.push(1 << 7);
                }
                if level == leaf && leaf > 1 {
                    for bit in 12..(12 + 9 * (leaf - 1)) {
                        invalid_bits.push(1 << bit);
                    }
                }
                for invalid in invalid_bits {
                    let mut f = Fixture::new(0x5580, 0, 0);
                    f.set_levels(levels);
                    enable_ad(&f, true, true);
                    let entries = walk(&f.gm, levels, leaf, 0xabc, 1 << 30);
                    let address = entries[usize::from(failing)];
                    put(&f.gm, address, &[f.word(address) | invalid]);
                    let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
                    for &address in entries.iter().take(usize::from(failing)) {
                        expected_word(&mut expected, address, f.word(address) | A);
                    }
                    f.assert_fault(0xabc, true, 0x7a, false);
                    assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
                }
            }
        }
    }
}

#[test]
fn input_output_geometry_and_memory_faults_do_not_set_dirty() {
    for levels in [3, 4] {
        for kind in 0..5 {
            let mut f = Fixture::new(0x7780, 0, 0);
            f.set_levels(levels);
            enable_ad(&f, true, true);
            let entries = walk(&f.gm, levels, 1, 0xabc, GPA);
            let mut iova = 0xabc;
            let (reason, marked) = match kind {
                0 => {
                    iova |= 1 << (12 + 9 * levels);
                    (0x84, 0)
                }
                1 => {
                    put(&f.gm, f.entries[3], &[f.word(f.entries[3]) & !(7 << 2)]);
                    (0x5b, 0)
                }
                2 => {
                    put(&f.gm, *entries.last().unwrap(), &[0xfee0_0003]);
                    (0x87, entries.len() - 1)
                }
                3 => {
                    put(
                        &f.gm,
                        f.entries[3],
                        &[(f.word(f.entries[3]) & 0xfff) | UNMAPPED],
                    );
                    (0x7b, 0)
                }
                4 => {
                    put(&f.gm, entries[0], &[UNMAPPED | 3]);
                    (0x78, 1)
                }
                _ => unreachable!(),
            };
            let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
            for &address in entries.iter().take(marked) {
                expected_word(&mut expected, address, f.word(address) | A);
            }
            f.assert_fault(iova, true, reason, false);
            assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
        }
    }
}

#[test]
fn snoop_policy_only_faults_when_an_update_is_required() {
    for pwsnp in [false, true] {
        for write in [false, true] {
            for missing in [0, A, D] {
                for target in 0..4 {
                    for fpd in 0..8 {
                        let mut f = Fixture::new(0xfe80, 64, 0);
                        enable_ad(&f, true, pwsnp);
                        f.fpd(fpd);
                        let entries = walk(&f.gm, 4, 1, IOVA, GPA);
                        for (index, &address) in entries.iter().enumerate() {
                            let flags = (A | D) & if index == target { !missing } else { u64::MAX };
                            put(&f.gm, address, &[f.word(address) | flags | IGNORED]);
                        }
                        let before = bytes(&f.gm, SL_ROOT, 4 * 4096);
                        let target_word = f.word(entries[target]);
                        let required = missing == A || (missing == D && target == 3 && write);
                        if !pwsnp && required {
                            f.assert_fault(IOVA, write, 0x7c, fpd != 0);
                            assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), before);
                        } else {
                            assert_eq!(f.translate(IOVA, write).unwrap(), GPA | 0xabc);
                            let mut expected = before;
                            if required {
                                let address = entries[target];
                                expected_word(&mut expected, address, target_word | missing);
                            }
                            assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn absent_coherency_capability_rejects_updates_not_already_set_bits() {
    let mut f = Fixture::new(0x5580, 0, 0);
    enable_ad(&f, true, false);
    let entries = walk(&f.gm, 4, 1, IOVA, GPA);
    let before = bytes(&f.gm, SL_ROOT, 4 * 4096);
    f.assert_fault_with_capabilities(IOVA, true, 0x7c, false, final_ecap().with_smpwcs(false));
    assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), before);
    for &address in &entries {
        put(&f.gm, address, &[f.word(address) | A | D]);
    }
    assert_eq!(
        f.dev
            .shared
            .translator()
            .translate_with_capabilities(
                f.rid,
                IOVA,
                true,
                final_ecap().with_smpwcs(false),
                |gpa| gpa
            )
            .unwrap(),
        GPA | 0xabc
    );
}

#[test]
fn memory_update_failures_are_typed_qualified_and_never_run_dma() {
    for target in 0..4 {
        for write in [false, true] {
            for fpd in 0..8 {
                let mut f = Fixture::new(0x5a80, 63, 0);
                enable_ad(&f, true, true);
                f.fpd(fpd);
                let entries = walk(&f.gm, 4, 1, IOVA, GPA);
                let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
                for &address in entries.iter().take(target) {
                    expected_word(&mut expected, address, f.word(address) | A);
                }
                let gm = f.gm.clone();
                let mut calls = 0;
                let error =
                    injected_fault(&mut f, write, 0x7d, fpd != 0, |address, current, new| {
                        calls += 1;
                        cas(
                            &gm,
                            if address == entries[target] {
                                UNMAPPED
                            } else {
                                address
                            },
                            current,
                            new,
                        )
                    });
                assert!(matches!(
                    error,
                    VtdFault::AdUpdate {
                        error: SecondStageAdError::Memory(_),
                        ..
                    }
                ));
                assert_eq!(calls, target + 1);
                assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
            }
        }
    }
}

#[test]
fn already_set_bits_skip_even_a_failing_atomic_memory_access() {
    for write in [false, true] {
        for leaf_dirty in [false, true] {
            if write && !leaf_dirty {
                continue;
            }
            let f = Fixture::new(0xff80, 64, 0);
            enable_ad(&f, true, true);
            let entries = walk(&f.gm, 4, 1, IOVA, GPA);
            for (index, &address) in entries.iter().enumerate() {
                put(
                    &f.gm,
                    address,
                    &[f.word(address) | A | if index == 3 && leaf_dirty { D } else { 0 }],
                );
            }
            let before = bytes(&f.gm, SL_ROOT, 4 * 4096);
            let mut calls = 0;
            // Readable paging memory with a failing CAS models read-only
            // tables without an unsafe GuestMemoryAccess test implementation.
            assert_eq!(
                injected(
                    &f.dev.shared,
                    f.translation_context(IOVA),
                    write,
                    |_, current, new| {
                        calls += 1;
                        cas(&f.gm, UNMAPPED, current, new)
                    },
                    |gpa| gpa
                )
                .unwrap(),
                GPA | 0xabc
            );
            assert_eq!(calls, 0);
            assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), before);
        }
    }
}

#[test]
fn ssade_clear_skips_cas_with_non_snooping_and_missing_flags() {
    for write in [false, true] {
        let f = Fixture::new(0xff80, 64, 0);
        enable_ad(&f, false, false);
        let before = bytes(&f.gm, SL_ROOT, 4 * 4096);
        let mut calls = 0;
        assert_eq!(
            injected(
                &f.dev.shared,
                f.translation_context(IOVA),
                write,
                |_, current, new| {
                    calls += 1;
                    cas(&f.gm, UNMAPPED, current, new)
                },
                |gpa| gpa
            )
            .unwrap(),
            GPA | 0xabc
        );
        assert_eq!(calls, 0);
        assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), before);
    }
}

#[test]
fn repeated_reads_and_writes_only_cas_missing_bits() {
    let f = Fixture::new(0x0180, 64, 0);
    enable_ad(&f, true, true);
    let entries = walk(&f.gm, 4, 1, IOVA, GPA);
    for (write, expected_calls) in [(false, 4), (false, 0), (true, 1), (true, 0), (false, 0)] {
        let mut calls = Vec::new();
        assert_eq!(
            injected(
                &f.dev.shared,
                f.translation_context(IOVA),
                write,
                |address, current, new| {
                    calls.push((address, new ^ current));
                    cas(&f.gm, address, current, new)
                },
                |gpa| gpa
            )
            .unwrap(),
            GPA | 0xabc
        );
        assert_eq!(calls.len(), expected_calls);
        if expected_calls == 4 {
            assert_eq!(
                calls,
                entries
                    .iter()
                    .map(|&address| (address, A))
                    .collect::<Vec<_>>()
            );
        } else if expected_calls == 1 {
            assert_eq!(calls, [(entries[3], D)]);
        }
    }
}

#[test]
fn retry_exhaustion_is_typed_bounded_and_qualified() {
    for write in [false, true] {
        for fpd in 0..8 {
            let mut f = Fixture::new(0xfa80, 63, 0);
            enable_ad(&f, true, true);
            f.fpd(fpd);
            let entries = walk(&f.gm, 4, 1, IOVA, GPA);
            let before = bytes(&f.gm, SL_ROOT, 4 * 4096);
            let gm = f.gm.clone();
            let mut attempts = 0;
            let error = injected_fault(&mut f, write, 0x7d, fpd != 0, |address, current, new| {
                assert_eq!(address, entries[0]);
                attempts += 1;
                put(&gm, address, &[current ^ (1 << 10)]);
                cas(&gm, address, current, new)
            });
            assert!(matches!(
                error,
                VtdFault::AdUpdate {
                    error: SecondStageAdError::RetryExhausted,
                    ..
                }
            ));
            assert_eq!(attempts, MAX_AD_WALK_ATTEMPTS);
            assert_eq!(bytes(&gm, SL_ROOT, 4 * 4096), before);
        }
    }
}

#[test]
fn retry_budget_is_shared_across_levels_and_complete_restarts() {
    let mut f = Fixture::new(0x7f80, 0, 0);
    enable_ad(&f, true, true);
    let entries = walk(&f.gm, 4, 1, IOVA, GPA);
    let gm = f.gm.clone();
    let mut conflicts = 0;
    let mut calls = 0;
    let error = injected_fault(&mut f, true, 0x7d, false, |address, current, new| {
        calls += 1;
        if address == entries[conflicts % 4] {
            conflicts += 1;
            // Software re-arms the ancestors on every conflict. A per-entry
            // budget, or one reset on progress, would never terminate here.
            for &entry in &entries {
                let word = u64::from_le(gm.read_plain::<u64>(entry).unwrap());
                put(&gm, entry, &[word & !A]);
            }
            put(&gm, address, &[current ^ (1 << 10)]);
        }
        cas(&gm, address, current, new)
    });
    assert!(matches!(
        error,
        VtdFault::AdUpdate {
            error: SecondStageAdError::RetryExhausted,
            ..
        }
    ));
    assert_eq!(conflicts, MAX_AD_WALK_ATTEMPTS);
    assert_eq!(calls, 40);
    for &address in &entries {
        assert_eq!(f.word(address) & (A | D), 0);
    }
}

#[test]
fn last_budgeted_walk_can_still_complete() {
    for target in [0, 3] {
        let f = Fixture::new(0x7f80, 0, 0);
        enable_ad(&f, true, true);
        let entries = walk(&f.gm, 4, 1, IOVA, GPA);
        let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
        for (index, &address) in entries.iter().enumerate() {
            let concurrent = if index == target && !(MAX_AD_WALK_ATTEMPTS - 1).is_multiple_of(2) {
                1 << 10
            } else {
                0
            };
            expected_word(
                &mut expected,
                address,
                f.word(address) | concurrent | A | if index == 3 { D } else { 0 },
            );
        }
        let mut attempts = 0;
        assert_eq!(
            injected(
                &f.dev.shared,
                f.translation_context(IOVA),
                true,
                |address, current, new| {
                    if address == entries[target] {
                        attempts += 1;
                        if attempts < MAX_AD_WALK_ATTEMPTS {
                            put(&f.gm, address, &[current ^ (1 << 10)]);
                        }
                    }
                    cas(&f.gm, address, current, new)
                },
                |gpa| gpa
            )
            .unwrap(),
            GPA | 0xabc
        );
        assert_eq!(attempts, MAX_AD_WALK_ATTEMPTS);
        assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
    }
}

#[test]
fn dirty_only_update_failure_and_non_snoop_fault_are_distinct() {
    for pwsnp in [false, true] {
        for fpd in 0..8 {
            let mut f = Fixture::new(0xaa80, 64, 0);
            enable_ad(&f, true, pwsnp);
            f.fpd(fpd);
            let entries = walk(&f.gm, 4, 1, IOVA, GPA);
            for &address in &entries {
                put(&f.gm, address, &[f.word(address) | A]);
            }
            let before = bytes(&f.gm, SL_ROOT, 4 * 4096);
            let gm = f.gm.clone();
            let mut calls = 0;
            let error = injected_fault(
                &mut f,
                true,
                if pwsnp { 0x7d } else { 0x7c },
                fpd != 0,
                |address, current, new| {
                    calls += 1;
                    assert_eq!(address, entries[3]);
                    assert_eq!(new ^ current, D);
                    cas(&gm, UNMAPPED, current, new)
                },
            );
            if pwsnp {
                assert!(matches!(
                    error,
                    VtdFault::AdUpdate {
                        error: SecondStageAdError::Memory(_),
                        ..
                    }
                ));
            } else {
                assert!(matches!(
                    error,
                    VtdFault::AdUpdate {
                        error: SecondStageAdError::NonSnooping,
                        ..
                    }
                ));
            }
            assert_eq!(calls, usize::from(pwsnp));
            assert_eq!(bytes(&gm, SL_ROOT, 4 * 4096), before);
            // A read needs no dirty update, even with non-snooping/read-only tables.
            assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
            assert_eq!(bytes(&gm, SL_ROOT, 4 * 4096), before);
        }
    }
}

#[test]
fn conflicts_preserve_concurrently_set_ad_and_changed_ignored_bits() {
    for target in 0..4 {
        for write in [false, true] {
            for change in [A, D, A | D, IGNORED] {
                let f = Fixture::new(0x0180, 64, 0);
                enable_ad(&f, true, true);
                let entries = walk(&f.gm, 4, 1, IOVA, GPA);
                // The ignored-bit conflict both clears and sets software bits.
                put(
                    &f.gm,
                    entries[target],
                    &[f.word(entries[target]) | (1 << 52)],
                );
                let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
                for (index, &address) in entries.iter().enumerate() {
                    let mut word = f.word(address);
                    if index == target {
                        word ^= change;
                    }
                    expected_word(
                        &mut expected,
                        address,
                        word | A | if write && index == 3 { D } else { 0 },
                    );
                }
                let mut changed = false;
                let mut conflicts = 0;
                assert_eq!(
                    injected(
                        &f.dev.shared,
                        f.translation_context(IOVA),
                        write,
                        |address, current, new| {
                            if address == entries[target] && !changed {
                                changed = true;
                                put(&f.gm, address, &[current ^ change]);
                            }
                            let success = cas(&f.gm, address, current, new)?;
                            conflicts += usize::from(!success);
                            Ok(success)
                        },
                        |gpa| gpa
                    )
                    .unwrap(),
                    GPA | 0xabc
                );
                assert!(changed);
                assert_eq!(conflicts, 1);
                assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
            }
        }
    }
}

#[test]
fn conflicts_revalidate_permissions_presence_and_reserved_bits_at_every_level() {
    for target in 0..4 {
        for write in [false, true] {
            for kind in 0..4 {
                let mut f = Fixture::new(0xbb80, 0, 0);
                enable_ad(&f, true, true);
                let entries = walk(&f.gm, 4, 1, IOVA, GPA);
                let original: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
                let (replacement, reason) = match kind {
                    0 => (
                        (original[target] & !3) | if write { 1 } else { 2 },
                        if write { 0x85 } else { 0x86 },
                    ),
                    1 => (original[target] & !3, 0x79),
                    2 => (original[target] | (1 << 62), 0x7a),
                    // Revalidation is mandatory even when the new word already
                    // has all required flags and will not itself need a CAS.
                    3 => ((original[target] & !3) | A | D, 0x79),
                    _ => unreachable!(),
                };
                let gm = f.gm.clone();
                let mut changed = false;
                injected_fault(&mut f, write, reason, false, |address, current, new| {
                    if address == entries[target] && !changed {
                        changed = true;
                        put(&gm, address, &[replacement]);
                    }
                    cas(&gm, address, current, new)
                });
                assert!(changed);
                for (index, &address) in entries.iter().enumerate() {
                    let value = if index == target {
                        replacement
                    } else {
                        original[index]
                    };
                    let accessed = index < target || (kind == 0 && index < 3);
                    assert_eq!(f.word(address), value | if accessed { A } else { 0 });
                }
            }
        }
    }
}

#[test]
fn leaf_conflict_restarts_ancestor_permissions_not_just_the_leaf() {
    let mut f = Fixture::new(0xbb80, 0, 0);
    enable_ad(&f, true, true);
    let entries = walk(&f.gm, 4, 1, IOVA, GPA);
    let original: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
    let gm = f.gm.clone();
    let mut changed = false;
    injected_fault(&mut f, true, 0x85, false, |address, current, new| {
        if address == entries[3] && !changed {
            changed = true;
            put(&gm, entries[0], &[(original[0] & !2) | A]);
            put(&gm, address, &[current | (1 << 10)]);
        }
        cas(&gm, address, current, new)
    });
    assert!(changed);
    assert_eq!(f.word(entries[0]), (original[0] & !2) | A);
    assert_eq!(f.word(entries[3]), original[3] | (1 << 10));
}

#[test]
fn conflicts_follow_changed_next_table_and_leave_abandoned_tables_untouched() {
    for target in 0..3 {
        let f = Fixture::new(0xdd80, 64, 0);
        enable_ad(&f, true, true);
        let entries = walk(&f.gm, 4, 1, IOVA, GPA);
        let alternate = walk_at(&f.gm, ALTERNATE, 3 - target as u8, 1, IOVA, 0xa0000);
        let replacement = ALTERNATE | 3 | IGNORED;
        let mut expected = bytes(&f.gm, SL_ROOT, 0x14000);
        for &address in entries.iter().take(target) {
            expected_word(&mut expected, address, f.word(address) | A);
        }
        expected_word(&mut expected, entries[target], replacement | A);
        for (index, &address) in alternate.iter().enumerate() {
            expected_word(
                &mut expected,
                address,
                f.word(address) | A | if index == alternate.len() - 1 { D } else { 0 },
            );
        }
        let mut changed = false;
        assert_eq!(
            injected(
                &f.dev.shared,
                f.translation_context(IOVA),
                true,
                |address, current, new| {
                    if address == entries[target] && !changed {
                        changed = true;
                        put(&f.gm, address, &[replacement]);
                    }
                    cas(&f.gm, address, current, new)
                },
                |gpa| gpa
            )
            .unwrap(),
            0xa0abc
        );
        assert!(changed);
        assert_eq!(bytes(&f.gm, SL_ROOT, 0x14000), expected);
    }
}

#[test]
fn conflicts_follow_changed_leaf_page_addresses() {
    for leaf in [1, 2, 3] {
        for ready in [0, A | D] {
            let f = Fixture::new(0xee80, 64, 0);
            enable_ad(&f, true, true);
            let entries = walk(&f.gm, 4, leaf, IOVA, 1 << 30);
            let address = *entries.last().unwrap();
            let replacement = (1 << 47) | 3 | IGNORED | ready | if leaf > 1 { 1 << 7 } else { 0 };
            let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
            for &entry in &entries[..entries.len() - 1] {
                expected_word(&mut expected, entry, f.word(entry) | A);
            }
            expected_word(&mut expected, address, replacement | A | D);
            let mut changed = false;
            assert_eq!(
                injected(
                    &f.dev.shared,
                    f.translation_context(IOVA),
                    true,
                    |entry, current, new| {
                        if entry == address && !changed {
                            changed = true;
                            put(&f.gm, entry, &[replacement]);
                        }
                        cas(&f.gm, entry, current, new)
                    },
                    |gpa| gpa
                )
                .unwrap(),
                (1 << 47) | (IOVA & ((1 << (12 + 9 * (leaf - 1))) - 1))
            );
            assert!(changed);
            assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
        }
    }
}

#[test]
fn conflicts_recompute_leaf_geometry_in_both_directions() {
    for leaf in [2, 3] {
        for make_leaf in [false, true] {
            let f = Fixture::new(0xee80, 64, 0);
            enable_ad(&f, true, true);
            let entries = walk(&f.gm, 4, if make_leaf { 1 } else { leaf }, IOVA, 1 << 30);
            let target = usize::from(4 - leaf);
            let alternate = walk_at(&f.gm, ALTERNATE, leaf - 1, 1, IOVA, 0xa0000);
            let replacement = if make_leaf {
                (1 << 30) | (1 << 7)
            } else {
                ALTERNATE
            } | 3
                | IGNORED;
            let output = if make_leaf {
                (1 << 30) | (IOVA & ((1 << (12 + 9 * (leaf - 1))) - 1))
            } else {
                0xa0abc
            };
            let mut expected = bytes(&f.gm, SL_ROOT, 0x14000);
            for &address in entries.iter().take(target) {
                expected_word(&mut expected, address, f.word(address) | A);
            }
            expected_word(
                &mut expected,
                entries[target],
                replacement | A | if make_leaf { D } else { 0 },
            );
            if !make_leaf {
                for (index, &address) in alternate.iter().enumerate() {
                    expected_word(
                        &mut expected,
                        address,
                        f.word(address) | A | if index == alternate.len() - 1 { D } else { 0 },
                    );
                }
            }
            let mut changed = false;
            assert_eq!(
                injected(
                    &f.dev.shared,
                    f.translation_context(IOVA),
                    true,
                    |address, current, new| {
                        if address == entries[target] && !changed {
                            changed = true;
                            put(&f.gm, address, &[replacement]);
                        }
                        cas(&f.gm, address, current, new)
                    },
                    |gpa| gpa
                )
                .unwrap(),
                output
            );
            assert!(changed);
            assert_eq!(bytes(&f.gm, SL_ROOT, 0x14000), expected);
        }
    }
}

#[test]
fn conflicts_revalidate_malformed_geometry_and_output_bounds() {
    for (leaf, target, replacement, reason) in [
        (1, 0, ALTERNATE | 0x83, 0x7a),   // PS at level 4
        (3, 1, (1 << 30) | 0x1083, 0x7a), // misaligned 1GB
        (2, 2, (1 << 30) | 0x1083, 0x7a), // misaligned 2MB
        (1, 3, (1 << 48) | 3, 0x7a),      // output above HAW
        (1, 3, 0xfee0_0003, 0x87),        // interrupt output
        (1, 0, (1 << 48) | 3, 0x7a),      // next table above HAW
        (1, 0, UNMAPPED | 3, 0x78),       // inaccessible new table
    ] {
        let mut f = Fixture::new(0x9a80, 64, 0);
        enable_ad(&f, true, true);
        let entries = walk(&f.gm, 4, leaf, IOVA, 1 << 30);
        let mut expected = bytes(&f.gm, SL_ROOT, 4 * 4096);
        for &address in entries.iter().take(target) {
            expected_word(&mut expected, address, f.word(address) | A);
        }
        expected_word(
            &mut expected,
            entries[target],
            replacement | if reason == 0x78 { A } else { 0 },
        );
        let gm = f.gm.clone();
        let mut changed = false;
        injected_fault(&mut f, true, reason, false, |address, current, new| {
            if address == entries[target] && !changed {
                changed = true;
                put(&gm, address, &[replacement]);
            }
            cas(&gm, address, current, new)
        });
        assert!(changed);
        assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), expected);
    }
}

#[test]
fn concurrent_translators_mark_shared_and_independent_guest_memory_tables() {
    for independent in [false, true] {
        let f = Fixture::new(0, 0, 0);
        enable_ad(&f, true, true);
        let first = walk(&f.gm, 4, 1, IOVA, GPA);
        let second = if independent {
            walk_at(&f.gm, ALTERNATE, 4, 1, IOVA, 0xa0000)
        } else {
            first.clone()
        };
        put(&f.gm, LOWER + 32, &[DIRECTORY | 1, 1, 0, 0]);
        put(
            &f.gm,
            PASID_TABLE + 64,
            &[
                (if independent { ALTERNATE } else { SL_ROOT })
                    | 1
                    | (2 << 2)
                    | (2 << 6)
                    | (1 << 9),
                0xbeef | (1 << 23),
                0,
                0,
                0,
                0,
                0,
                0,
            ],
        );
        for entries in [&first, &second] {
            for &address in &entries[..3] {
                put(&f.gm, address, &[f.word(address) | A]);
            }
        }
        let mut expected = bytes(&f.gm, SL_ROOT, 0x14000);
        for entries in [&first, &second] {
            expected_word(&mut expected, entries[3], f.word(entries[3]) | A | D);
        }
        let barrier = Barrier::new(4);
        let conflicts = AtomicUsize::new(0);
        let leaves = [first[3], second[3]];
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|index| {
                    let f = &f;
                    let barrier = &barrier;
                    let conflicts = &conflicts;
                    scope.spawn(move || {
                        let rid = index / 2;
                        let write = index % 2 != 0;
                        let context = TranslationContext {
                            source_id: rid,
                            ..f.translation_context(IOVA)
                        };
                        let output = if independent && rid == 1 {
                            0xa0abc
                        } else {
                            GPA | 0xabc
                        };
                        let mut first_cas = true;
                        assert_eq!(
                            injected(
                                &f.dev.shared,
                                context,
                                write,
                                |address, current, new| {
                                    if first_cas {
                                        first_cas = false;
                                        // Every translator has read the old leaf before
                                        // any can CAS it, forcing real memory conflicts.
                                        barrier.wait();
                                    }
                                    let success = cas(&f.gm, address, current, new)?;
                                    conflicts.fetch_add(usize::from(!success), Ordering::SeqCst);
                                    Ok(success)
                                },
                                |gpa| gpa
                            )
                            .unwrap(),
                            output
                        );
                        barrier.wait();
                        // Re-arm between rounds and contend through the production
                        // GuestMemory adapter too, not just the scheduled callable.
                        for _ in 0..16 {
                            if index == 0 {
                                for address in leaves {
                                    put(&f.gm, address, &[f.word(address) & !(A | D)]);
                                }
                            }
                            barrier.wait();
                            assert_eq!(
                                f.dev
                                    .shared
                                    .translator()
                                    .translate(rid, IOVA, write, |gpa| gpa)
                                    .unwrap(),
                                output
                            );
                            barrier.wait();
                        }
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert!(conflicts.load(Ordering::SeqCst) >= if independent { 2 } else { 3 });
        assert_eq!(bytes(&f.gm, SL_ROOT, 0x14000), expected);
    }
}

fn prepare_invalidation(f: &mut Fixture, data: u32) {
    // Global context/PASID/IOTLB caches with both drains, then a fenced
    // status-write wait. ATS is off, so no device-TLB invalidation is needed.
    for (index, words) in [
        [0x11, 0, 0, 0],
        [0x37, 0, 0, 0],
        [0xd2, 0, 0, 0],
        [(u64::from(data) << 32) | 0x65, STATUS, 0, 0],
    ]
    .iter()
    .enumerate()
    {
        put(&f.gm, QUEUE + index as u64 * 32, words);
    }
    f.gm.write_plain(STATUS, &0u32).unwrap();
    // Quiesce translation before disabling/reprogramming the queue.
    let qie = read32(&mut f.dev, 0x01c) & (1 << 26);
    write32(&mut f.dev, 0x018, qie);
    write32(&mut f.dev, 0x018, 0);
    write64(&mut f.dev, 0x088, 0);
    write64(&mut f.dev, 0x090, QUEUE | (1 << 11));
    write32(&mut f.dev, 0x018, (1 << 31) | (1 << 26));
}

fn complete_invalidation(dev: &mut IntelVtdDevice) {
    write64(dev, 0x088, 128);
}

fn invalidate_and_wait(f: &mut Fixture, data: u32) {
    prepare_invalidation(f, data);
    complete_invalidation(&mut f.dev);
    assert_eq!(read64(&mut f.dev, 0x080), 128);
    assert_eq!(read32(&mut f.dev, 0x034) & (1 << 4), 0);
    assert_eq!(u32::from_le(f.gm.read_plain::<u32>(STATUS).unwrap()), data);
}

#[test]
fn production_dma_isolates_lower_upper_devices_domains_and_ad_policy() {
    let mut f = Fixture::new(0x007f, 63, 0);
    enable_ad(&f, true, true);
    let first = walk(&f.gm, 4, 1, IOVA, GPA);
    let second_gpa = 0xa0000;
    let second_iova = IOVA & ((1 << 39) - 1);
    let second = walk_at(&f.gm, ALTERNATE, 3, 1, second_iova, second_gpa);
    put(&f.gm, ROOT + 8, &[UPPER | 1]);
    put(&f.gm, UPPER, &[DIRECTORY | 1, 65, 0, 0]);
    put(&f.gm, DIRECTORY + 8, &[PASID_TABLE | 1]);
    put(
        &f.gm,
        PASID_TABLE + 64,
        &[
            ALTERNATE | 1 | (1 << 2) | (2 << 6),
            0x1234 | (1 << 23),
            0,
            0,
            0,
            0,
            0,
            0,
        ],
    );
    let original_first: Vec<_> = first.iter().map(|&a| f.word(a)).collect();
    let original_second: Vec<_> = second.iter().map(|&a| f.word(a)).collect();
    let translator = f.dev.shared.translator();
    for (rid, iova, output, payload) in [
        (f.rid, IOVA, GPA | 0xabc, [0x5a, 0xa5]),
        (0x0080, second_iova, second_gpa | 0xabc, [0x12, 0x34]),
    ] {
        translator
            .translate(rid, iova, true, |gpa| {
                assert_eq!(gpa, output);
                f.gm.write_at(gpa, &payload).unwrap();
            })
            .unwrap();
    }
    for (index, &address) in first.iter().enumerate() {
        assert_eq!(
            f.word(address),
            original_first[index] | A | if index == 3 { D } else { 0 }
        );
    }
    for (index, &address) in second.iter().enumerate() {
        assert_eq!(f.word(address), original_second[index]);
    }
    assert_eq!(
        f.gm.read_plain::<[u8; 2]>(GPA | 0xabc).unwrap(),
        [0x5a, 0xa5]
    );
    assert_eq!(
        translator
            .translate(0x0080, second_iova, false, |gpa| f
                .gm
                .read_plain::<[u8; 2]>(gpa)
                .unwrap())
            .unwrap(),
        [0x12, 0x34]
    );

    // Changing one domain's tracking policy must not affect the other.
    put(
        &f.gm,
        PASID_TABLE + 64,
        &[f.word(PASID_TABLE + 64) | (1 << 9)],
    );
    invalidate_and_wait(&mut f, 1);
    translator
        .translate(0x0080, second_iova, false, |_| ())
        .unwrap();
    for (index, &address) in second.iter().enumerate() {
        assert_eq!(f.word(address), original_second[index] | A);
    }
    translator
        .translate(0x0080, second_iova, true, |_| ())
        .unwrap();
    assert_eq!(f.word(second[2]), original_second[2] | A | D);
    for (index, &address) in first.iter().enumerate() {
        assert_eq!(
            f.word(address),
            original_first[index] | A | if index == 3 { D } else { 0 }
        );
    }
    assert_eq!(read32(&mut f.dev, 0x034), 0);
}

#[test]
fn clear_rearm_after_invalidation_wait_and_ssade_toggle() {
    for leaf in [1, 2, 3] {
        let f = Fixture::new(0x0180, 64, 0);
        enable_ad(&f, true, true);
        let mut f = f;
        let entries = walk(&f.gm, 4, leaf, IOVA, 1 << 30);
        let original: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
        let output = (1 << 30) | (IOVA & ((1 << (12 + 9 * (leaf - 1))) - 1));
        assert_eq!(f.translate(IOVA, true).unwrap(), output);
        for (step, write) in [false, true].into_iter().enumerate() {
            for (index, &address) in entries.iter().enumerate() {
                put(&f.gm, address, &[original[index]]);
            }
            invalidate_and_wait(&mut f, step as u32 + 1);
            assert_eq!(f.translate(IOVA, write).unwrap(), output);
            for (index, &address) in entries.iter().enumerate() {
                assert_eq!(
                    f.word(address),
                    original[index]
                        | A
                        | if write && index == entries.len() - 1 {
                            D
                        } else {
                            0
                        }
                );
            }
        }
        enable_ad(&f, false, true);
        for (index, &address) in entries.iter().enumerate() {
            put(&f.gm, address, &[original[index]]);
        }
        invalidate_and_wait(&mut f, 3);
        assert_eq!(f.translate(IOVA, true).unwrap(), output);
        for (index, &address) in entries.iter().enumerate() {
            assert_eq!(f.word(address), original[index]);
        }
        enable_ad(&f, true, true);
        invalidate_and_wait(&mut f, 4);
        assert_eq!(f.translate(IOVA, false).unwrap(), output);
        assert_eq!(f.word(*entries.last().unwrap()) & (A | D), A);
        assert_eq!(f.translate(IOVA, true).unwrap(), output);
        assert_eq!(f.word(*entries.last().unwrap()) & (A | D), A | D);
        let address = *entries.last().unwrap();
        put(&f.gm, address, &[f.word(address) & !D]);
        invalidate_and_wait(&mut f, 5);
        assert_eq!(f.translate(IOVA, false).unwrap(), output);
        assert_eq!(f.word(address) & D, 0);
        assert_eq!(f.translate(IOVA, true).unwrap(), output);
        assert_eq!(f.word(address) & (A | D), A | D);
    }
}

#[test]
fn uncached_walks_can_rearm_without_invalidation_as_an_implementation_property() {
    let f = Fixture::new(0x0180, 64, 0);
    enable_ad(&f, true, true);
    let entries = walk(&f.gm, 4, 1, IOVA, GPA);
    let original: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
    for write in [true, false, true] {
        for (index, &address) in entries.iter().enumerate() {
            put(&f.gm, address, &[original[index]]);
        }
        // No concurrent DMA here. This tests the uncached implementation, not
        // permission for portable software to omit §6.5.3.3 invalidations.
        assert_eq!(f.translate(IOVA, write).unwrap(), GPA | 0xabc);
        for (index, &address) in entries.iter().enumerate() {
            assert_eq!(
                f.word(address),
                original[index] | A | if write && index == 3 { D } else { 0 }
            );
        }
    }
}

#[test]
fn reset_translation_disable_and_mode_changes_preserve_guest_ad_state() {
    let mut f = Fixture::new(0, 0, 0);
    enable_ad(&f, true, true);
    let entries = walk(&f.gm, 4, 1, IOVA, GPA);
    let root = bytes(&f.gm, f.entries[0], 16);
    let context = bytes(&f.gm, f.entries[1], 32);
    let original: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
    assert_eq!(f.translate(IOVA, true).unwrap(), GPA | 0xabc);
    let marked = bytes(&f.gm, SL_ROOT, 4 * 4096);
    {
        use std::future::Future;
        use std::task::Context;
        use std::task::Poll;
        use std::task::Waker;
        let mut reset = std::pin::pin!(vmcore::device_state::ChangeDeviceState::reset(&mut f.dev));
        assert_eq!(
            reset.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
    }
    assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), marked);
    assert_eq!(read32(&mut f.dev, 0x01c), 0);
    assert_eq!(f.translate(IOVA, true).unwrap(), IOVA);
    for (index, &address) in entries.iter().enumerate() {
        put(&f.gm, address, &[original[index]]);
    }
    let cleared = bytes(&f.gm, SL_ROOT, 4 * 4096);
    assert_eq!(f.translate(IOVA, true).unwrap(), IOVA);
    assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), cleared);
    write64(&mut f.dev, 0x020, ROOT | (1 << 10));
    write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
    assert_eq!(f.translate(IOVA, false).unwrap(), GPA | 0xabc);
    let accessed = bytes(&f.gm, SL_ROOT, 4 * 4096);
    write32(&mut f.dev, 0x018, 0);
    f.legacy(4);
    assert_eq!(f.translate(IOVA, true).unwrap(), GPA | 0xabc);
    assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), accessed);
    write32(&mut f.dev, 0x018, 0);
    f.gm.write_at(f.entries[0], &root).unwrap();
    f.gm.write_at(f.entries[1], &context).unwrap();
    write64(&mut f.dev, 0x020, ROOT | (1 << 10));
    write32(&mut f.dev, 0x018, (1 << 30) | (1 << 31));
    assert_eq!(f.translate(IOVA, true).unwrap(), GPA | 0xabc);
    assert_eq!(bytes(&f.gm, SL_ROOT, 4 * 4096), marked);
}

#[test]
fn marking_precedes_dma_even_when_the_dma_closure_fails() {
    for write in [false, true] {
        for fail in [false, true] {
            let f = Fixture::new(0x0180, 64, 0);
            enable_ad(&f, true, true);
            let entries = walk(&f.gm, 4, 1, IOVA, GPA);
            let original: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
            let result = f
                .dev
                .shared
                .translator()
                .translate(f.rid, IOVA, write, |gpa| {
                    assert_eq!(gpa, GPA | 0xabc);
                    assert!(f.dev.shared.state.try_write().is_none());
                    for (index, &address) in entries.iter().enumerate() {
                        assert_eq!(
                            f.word(address),
                            original[index] | A | if write && index == 3 { D } else { 0 }
                        );
                    }
                    if fail {
                        Err("downstream DMA failed")
                    } else {
                        Ok(())
                    }
                })
                .unwrap();
            assert_eq!(
                result,
                if fail {
                    Err("downstream DMA failed")
                } else {
                    Ok(())
                }
            );
            for (index, &address) in entries.iter().enumerate() {
                assert_eq!(
                    f.word(address),
                    original[index] | A | if write && index == 3 { D } else { 0 }
                );
            }
            assert!(!f.dev.shared.state.read().frcd_hi.f());
        }
    }
}

#[test]
fn tracked_dma_and_atomic_marks_drain_before_invalidation_wait() {
    for write in [false, true] {
        let mut f = Fixture::new(0x0180, 64, 0);
        enable_ad(&f, true, true);
        let entries = walk(&f.gm, 4, 1, IOVA, GPA);
        let original: Vec<_> = entries.iter().map(|&a| f.word(a)).collect();
        prepare_invalidation(&mut f, 0xfeed);
        let shared = f.dev.shared.clone();
        let gm = f.gm.clone();
        let rid = f.rid;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let shared = &shared;
            let gm = &gm;
            let entries = &entries;
            let original = &original;
            let dma = scope.spawn(move || {
                shared
                    .translator()
                    .translate(rid, IOVA, write, |gpa| {
                        assert_eq!(gpa, GPA | 0xabc);
                        for (index, &address) in entries.iter().enumerate() {
                            assert_eq!(
                                u64::from_le(gm.read_plain::<u64>(address).unwrap()),
                                original[index] | A | if write && index == 3 { D } else { 0 }
                            );
                        }
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        if write {
                            gm.write_at(gpa, &[0xcd, 0xab]).unwrap();
                        } else {
                            assert_eq!(bytes(gm, gpa, 2), [0, 0]);
                        }
                    })
                    .unwrap();
            });
            entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(shared.state.try_write().is_none());
            let invalidation = scope.spawn(|| {
                complete_invalidation(&mut f.dev);
                done_tx.send(()).unwrap();
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while shared.state.try_read().is_some() {
                assert!(
                    Instant::now() < deadline,
                    "invalidation writer did not queue"
                );
                std::thread::yield_now();
            }
            assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
            release_tx.send(()).unwrap();
            dma.join().unwrap();
            invalidation.join().unwrap();
            done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        assert_eq!(read64(&mut f.dev, 0x080), 128);
        assert_eq!(read32(&mut f.dev, 0x034) & (1 << 4), 0);
        assert_eq!(u32::from_le(gm.read_plain::<u32>(STATUS).unwrap()), 0xfeed);
        if write {
            assert_eq!(bytes(&gm, GPA | 0xabc, 2), [0xcd, 0xab]);
        }
        for (index, &address) in entries.iter().enumerate() {
            assert_eq!(
                f.word(address),
                original[index] | A | if write && index == 3 { D } else { 0 }
            );
        }
    }
}

#[test]
fn invalidation_wait_drains_a_paused_atomic_update_and_then_the_dma() {
    let mut f = Fixture::new(0x0180, 64, 0);
    enable_ad(&f, true, true);
    let entries = walk(&f.gm, 4, 1, IOVA, GPA);
    prepare_invalidation(&mut f, 0xcafe);
    let shared = f.dev.shared.clone();
    let gm = f.gm.clone();
    let context = f.translation_context(IOVA);
    let (update_tx, update_rx) = mpsc::channel();
    let (mark_tx, mark_rx) = mpsc::channel();
    let (dma_tx, dma_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let shared = &shared;
        let gm = &gm;
        let leaf = entries[3];
        let dma = scope.spawn(move || {
            injected(
                shared,
                context,
                true,
                |address, current, new| {
                    assert!(shared.state.try_write().is_none());
                    if address == leaf {
                        update_tx.send(()).unwrap();
                        mark_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                    cas(gm, address, current, new)
                },
                |gpa| {
                    assert_eq!(gpa, GPA | 0xabc);
                    assert_eq!(
                        u64::from_le(gm.read_plain::<u64>(leaf).unwrap()) & (A | D),
                        A | D
                    );
                    dma_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    gm.write_at(gpa, &[0x42]).unwrap();
                },
            )
            .unwrap();
        });
        update_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            u64::from_le(gm.read_plain::<u64>(leaf).unwrap()) & (A | D),
            0
        );
        let invalidation = scope.spawn(|| {
            complete_invalidation(&mut f.dev);
            done_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while shared.state.try_read().is_some() {
            assert!(
                Instant::now() < deadline,
                "invalidation writer did not queue"
            );
            std::thread::yield_now();
        }
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
        mark_tx.send(()).unwrap();
        dma_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(gm.read_plain::<u32>(STATUS).unwrap(), 0);
        assert_eq!(bytes(gm, GPA | 0xabc, 1), [0]);
        release_tx.send(()).unwrap();
        dma.join().unwrap();
        invalidation.join().unwrap();
        done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    assert_eq!(bytes(&gm, GPA | 0xabc, 1), [0x42]);
    assert_eq!(u32::from_le(gm.read_plain::<u32>(STATUS).unwrap()), 0xcafe);
    assert_eq!(read64(&mut f.dev, 0x080), 128);
    assert_eq!(read32(&mut f.dev, 0x034) & (1 << 4), 0);
}
