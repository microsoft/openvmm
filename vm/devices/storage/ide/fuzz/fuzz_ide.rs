// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use chipset_arc_mutex_device::services::PortIoInterceptServices;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use ide::DriveMedia;
use ide::IdeDevice;
use pci_core::spec::cfg_space::HeaderType00;
use scsidisk::atapi_scsi::AtapiScsiDisk;
use scsidisk::scsidvd::SimpleScsiDvd;
use std::sync::Arc;
use vmcore::line_interrupt::LineInterrupt;
use xtask_fuzz::fuzz_eprintln;
use xtask_fuzz::fuzz_target;

#[derive(Arbitrary, Debug)]
pub enum FuzzDriveMedia {
    HardDrive,
    OpticalDrive,
}

impl FuzzDriveMedia {
    fn reify(self) -> DriveMedia {
        // we don't  care about drive contents for fuzzing
        match self {
            FuzzDriveMedia::HardDrive => {
                DriveMedia::hard_disk(disklayer_ram::ram_disk(0x100000 * 4, false).unwrap())
            }
            FuzzDriveMedia::OpticalDrive => {
                DriveMedia::optical_disk(Arc::new(AtapiScsiDisk::new(Arc::new(
                    SimpleScsiDvd::new(Some(disklayer_ram::ram_disk(0x100000 * 4, false).unwrap())),
                ))))
            }
        }
    }
}

#[derive(Arbitrary, Debug)]
pub enum FuzzChannelDrives {
    None,
    Single(FuzzDriveMedia),
    Dual(FuzzDriveMedia, FuzzDriveMedia),
}

impl FuzzChannelDrives {
    fn reify(self) -> [Option<DriveMedia>; 2] {
        match self {
            FuzzChannelDrives::None => [None, None],
            FuzzChannelDrives::Single(a) => [Some(a.reify()), None],
            FuzzChannelDrives::Dual(a, b) => [Some(a.reify()), Some(b.reify())],
        }
    }
}

#[derive(Arbitrary, Debug)]
struct StaticIdeFuzzConfig {
    memory: [u8; 256],
    primary_drives: FuzzChannelDrives,
    secondary_drives: FuzzChannelDrives,
}

// Any async operation should take no more than this many polls to complete.
// There is no backing asynchronous disk in this fuzzer, but the device emulator
// intentionally limits the amount of work it will do in a single poll.
// This is MAX_48BIT_SECTOR_COUNT / MAX_SECTORS_MULT_TRANSFER_DEFAULT
const MAX_POLL_COUNT: usize = 65536 / 128;

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    let static_config: StaticIdeFuzzConfig = u.arbitrary()?;

    fuzz_eprintln!("{:x?}", static_config);

    // increase at your own risk, the smaller this is the more effective mutating will be
    const GUEST_MEM_SIZE: usize = 4096;
    // Populate memory
    let guest_mem = GuestMemory::allocate(GUEST_MEM_SIZE);
    for addr in (0..GUEST_MEM_SIZE).step_by(static_config.memory.len()) {
        guest_mem
            .write_plain(addr.try_into().unwrap(), &static_config.memory)
            .unwrap()
    }

    let mut chipset = chipset_device_fuzz::FuzzChipset::new(MAX_POLL_COUNT);

    let ide_device = chipset
        .device_builder("ide")
        .try_add(|services| {
            IdeDevice::new(
                guest_mem,
                &mut services.register_pio(),
                static_config.primary_drives.reify(),
                static_config.secondary_drives.reify(),
                LineInterrupt::detached(),
                LineInterrupt::detached(),
            )
        })
        .unwrap();

    // set a bus master base to avoid wasted cycles
    (ide_device.lock())
        .pci_cfg_write(HeaderType00::BAR4.0, 0x1000)
        .unwrap();
    (ide_device.lock())
        .pci_cfg_write(HeaderType00::STATUS_COMMAND.0, 0x5)
        .unwrap();

    // remaining fuzzer input is used to drive device actions
    while !u.is_empty() {
        let action = chipset.get_arbitrary_action(u)?;

        // TODO: when hitting enlightened ports, make value point into guest memory

        fuzz_eprintln!("{:x?}", action);

        chipset.exec_action(action).unwrap();
    }

    Ok(())
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
