// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

#[derive(Debug, Arbitrary)]
struct StaticDeviceConfig {
    num_queues: usize,
}

fn fuzz_device(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    // Step 1: generate a device's fixed-at-construction-time configuration
    let static_config: StaticDeviceConfig = u.arbitrary()?;

    // Step 2: init the device, and wire-it-up to the fuzz chipset
    let mut chipset = chipset_device_fuzz::FuzzChipset::default();
    let _device = chipset
        .device_builder("fuzz_device")
        .add(|services| {
            // < device init code here >
            //
            // my_dev::MyDevice::new(
            //     static_device_config.num_queues,
            //     &mut services.register_mmio(), // e.g: pci devices with remappable BARs
            // )
            let _ = (static_config.num_queues, services);

            missing_dev::MissingDev::default() // REMOVE ME
        })
        .unwrap();

    // Step 3: use the remaining fuzzing input to slam the device with chipset events
    while !u.is_empty() {
        let action = chipset.get_arbitrary_action(u)?;
        fuzz_eprintln!("chipset action: {:x?}", action);
        chipset.exec_action(action).unwrap();

        // (optional) simulate "external" device events
        if u.ratio(1, 5)? {
            // e.g: device.inject_irq();
        }
    }

    Ok(())
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if fuzz_device(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
