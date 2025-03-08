// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

use arbitrary::Unstructured;
use chipset::cmos_rtc::Rtc;
use local_clock::MockLocalClock;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    let mut chipset = chipset_device_fuzz::FuzzChipset::default();

    let initial_cmos = u.arbitrary()?;

    // TODO: write a streamlined "fuzz driver" impl instead of using pal_async
    pal_async::DefaultPool::run_with(|driver| async move {
        let mut vm_time_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vm_time_source = vm_time_keeper.builder().build(&driver).await.unwrap();

        let time = MockLocalClock::new();
        let time_access = time.accessor();
        let enlightened_interrupts = u.arbitrary()?;

        chipset
            .device_builder("rtc")
            .add(|_| {
                Rtc::new(
                    Box::new(time),
                    LineInterrupt::detached(),
                    &vm_time_source,
                    0x32,
                    initial_cmos,
                    enlightened_interrupts,
                )
            })
            .unwrap();

        vm_time_keeper.start().await;
        let mut fake_vmtime = VmTime::from_100ns(0);

        while !u.is_empty() {
            let action = chipset.get_arbitrary_action(u)?;
            fuzz_eprintln!("{:x?}", action);
            chipset.exec_action(action).unwrap();

            // occasionally simulate time passing
            if u.ratio(1, 10)? {
                let millis = std::time::Duration::from_millis(u.int_in_range(500..=5000)?);

                fake_vmtime = fake_vmtime.wrapping_add(millis);
                vm_time_keeper.stop().await;
                vm_time_keeper
                    .restore(vmcore::vmtime::SavedState::from_vmtime(fake_vmtime))
                    .await;
                vm_time_keeper.start().await;

                fuzz_eprintln!("ticked vmtime by {:x?}", millis);

                // occasionally simulate RTC going backwards
                let go_backwards = u.ratio(1, 10)?;
                if go_backwards {
                    time_access.tick_backwards(millis)
                } else {
                    time_access.tick(millis)
                }

                fuzz_eprintln!(
                    "ticked mock local clock {} by {:?}",
                    if go_backwards {
                        "backwards"
                    } else {
                        "forwards"
                    },
                    millis
                );
            }
        }
        Ok(())
    })
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
