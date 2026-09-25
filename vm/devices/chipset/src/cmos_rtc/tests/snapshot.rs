// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the snapshot fields of the RTC saved state and for advancing
//! the guest-visible time.

use super::get_cmos_data;
use super::new_test_rtc;
use crate::cmos_rtc::spec::CmosReg;
use std::time::Duration;
use test_with_tracing::test;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveRestore;

#[test]
fn restore_rejects_inconsistent_status_c_before_mutating_state() {
    use vmcore::save_restore::{ProtobufSaveRestore, SavedStateBlob};

    for status_c in [0x10, 0x80] {
        let (_pool, _keeper, _, mut rtc) = new_test_rtc();
        let before = SaveRestore::save(&mut rtc).unwrap();
        let mut invalid = SaveRestore::save(&mut rtc).unwrap();
        invalid.cmos[CmosReg::STATUS_C.0 as usize] = status_c;

        let error =
            ProtobufSaveRestore::restore(&mut rtc, SavedStateBlob::new(invalid)).unwrap_err();
        match error {
            vmcore::save_restore::RestoreError::InvalidSavedState(error) => {
                assert_eq!(
                    error.to_string(),
                    "inconsistent RTC status C interrupt flags"
                )
            }
            error => panic!("unexpected restore error: {error}"),
        }

        let after = SaveRestore::save(&mut rtc).unwrap();
        assert_eq!(after.addr, before.addr);
        assert_eq!(after.cmos, before.cmos);
        assert_eq!(after.clock_time_millis, before.clock_time_millis);
        assert_eq!(after.transaction_read_mask, before.transaction_read_mask);
        assert_eq!(after.time_valid, before.time_valid);
    }
}

#[test]
fn restore_preserves_guest_epoch_and_advances_downtime() {
    let (mut pool, _vm_time_keeper, clock, mut rtc) = new_test_rtc();
    clock.tick(Duration::from_secs(10));
    let captured_time = clock.get_time();
    let saved = rtc.save().unwrap();

    clock.tick(Duration::from_secs(60));
    rtc.restore(saved).unwrap();
    assert_eq!(clock.get_time(), captured_time);

    pool.run_until(rtc.advance_time(Duration::from_secs(3)))
        .unwrap();
    assert_eq!(
        clock.get_time() - captured_time,
        Duration::from_secs(3).into()
    );
}

#[test]
fn rtc_rejects_retired_coherent_transaction_state() {
    let (_, _, _, mut rtc) = new_test_rtc();
    let mut saved = rtc.save().unwrap();
    saved.transaction_read_mask = Some(1 << 15);

    assert!(matches!(
        rtc.restore(saved),
        Err(vmcore::save_restore::RestoreError::InvalidSavedState(_))
    ));
}

#[test]
fn rtc_restores_legacy_valid_time_semantics() {
    let (_, _, _, mut rtc) = new_test_rtc();
    let mut saved = rtc.save().unwrap();
    saved.time_valid = None;
    saved.transaction_read_mask = None;

    rtc.restore(saved).unwrap();
    assert_eq!(get_cmos_data(&mut rtc, CmosReg::STATUS_D), 0x80);
}
