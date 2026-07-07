// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use chipset::cmos_rtc::Rtc;
use local_clock::MockLocalClock;
use std::time::Duration;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

/// An action the fuzzer can take in each iteration of the main loop.
#[derive(Arbitrary, Debug)]
enum FuzzAction {
    /// Dispatch a chipset MMIO/PIO/PCI/poll event picked by `FuzzChipset`.
    ChipsetEvent,
    /// Advance vmtime and the mock local clock forward by the specified number of milliseconds.
    TickForward(u16),
    /// Advance vmtime forward but tick the mock local clock backward, to
    /// exercise the RTC's "clock went backwards" handling.
    TickBackward(u16),
}

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    let mut chipset = chipset_device_fuzz::FuzzChipset::default();

    let initial_cmos = u.arbitrary()?;

    // TODO: write a streamlined "fuzz driver" impl instead of using pal_async
    pal_async::DefaultPool::run_with(async |driver| {
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
            let (millis, go_backwards) = match u.arbitrary::<FuzzAction>()? {
                FuzzAction::ChipsetEvent => {
                    let action = chipset.get_arbitrary_action(u)?;
                    fuzz_eprintln!("{:x?}", action);
                    chipset.exec_action(action).unwrap();
                    continue;
                }
                FuzzAction::TickForward(amount) => (amount, false),
                FuzzAction::TickBackward(amount) => (amount, true),
            };

            let millis = Duration::from_millis(millis.into());
            fake_vmtime = fake_vmtime.wrapping_add(millis);
            vm_time_keeper.stop().await;
            vm_time_keeper
                .restore(vmcore::vmtime::SavedState::from_vmtime(fake_vmtime))
                .await;
            vm_time_keeper.start().await;

            if go_backwards {
                time_access.tick_backwards(millis);
            } else {
                time_access.tick(millis);
            }

            fuzz_eprintln!(
                "ticked vmtime by {:?}, mock clock {}",
                millis,
                if go_backwards {
                    "backwards"
                } else {
                    "forwards"
                }
            );
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
