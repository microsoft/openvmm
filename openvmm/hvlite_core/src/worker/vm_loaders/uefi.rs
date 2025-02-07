// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use guid::Guid;
use hvdef::HV_PAGE_SIZE;
use hvlite_defs::config::UefiConsoleMode;
use loader::importer::Register;
use loader::uefi::config;
use loader::uefi::IMAGE_SIZE;
use std::io::Read;
use std::io::Seek;
use thiserror::Error;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read uefi firmware file")]
    Firmware(#[source] std::io::Error),
    #[error("uefi loader error")]
    Loader(#[source] loader::uefi::Error),
}

pub struct UefiLoadSettings {
    pub debugging: bool,
    pub battery: bool,
    pub memory_protections: bool,
    pub frontpage: bool,
    pub tpm: bool,
    pub guest_watchdog: bool,
    pub vpci_boot: bool,
    pub serial: bool,
    pub uefi_console_mode: Option<UefiConsoleMode>,
}

/// Loads the UEFI firmware.
///
/// If `firmware` is `None`, load the embedded firmware.
pub fn load_uefi(
    mut firmware: &std::fs::File,
    gm: &GuestMemory,
    processor_topology: &ProcessorTopology,
    mem_layout: &MemoryLayout,
    load_settings: UefiLoadSettings,
    madt: &[u8],
    srat: &[u8],
    pptt: Option<&[u8]>,
) -> Result<Vec<Register>, Error> {
    assert!(mem_layout.mmio().len() >= 2, "UEFI expects 2 MMIO gaps");

    let mut loaded_image;
    let image = {
        loaded_image = Vec::new();
        firmware.rewind().map_err(Error::Firmware)?;
        firmware
            .read_to_end(&mut loaded_image)
            .map_err(Error::Firmware)?;
        loaded_image.as_slice()
    };

    let mut entropy = [0; 64];
    getrandom::getrandom(&mut entropy).expect("rng failure");

    let memory_map: Vec<_> = mem_layout
        .ram()
        .iter()
        .map(|range| config::MemoryRangeV5 {
            base_address: range.range.start(),
            length: range.range.len(),
            flags: 0,
            reserved: 0,
        })
        .collect();

    let low_mmio = mem_layout.mmio()[0];
    let high_mmio = mem_layout.mmio()[1];

    let flags = config::Flags::new()
        .with_hibernate_enabled(true)
        .with_serial_controllers_enabled(load_settings.serial)
        .with_vpci_boot_enabled(load_settings.vpci_boot)
        .with_debugger_enabled(load_settings.debugging)
        .with_virtual_battery_enabled(load_settings.battery)
        .with_disable_frontpage(!load_settings.frontpage)
        .with_tpm_enabled(load_settings.tpm)
        .with_measure_additional_pcrs(load_settings.tpm)
        .with_tpm_locality_regs_enabled(load_settings.tpm)
        .with_watchdog_enabled(load_settings.guest_watchdog)
        // OpenVMM pre-sets the MTRRs; tell the firmware
        .with_mtrrs_initialized_at_load(true)
        // TODO: plumb all 4 kinds of memory protection modes through
        .with_memory_protection(if load_settings.memory_protections {
            config::MemoryProtection::Default
        } else {
            config::MemoryProtection::Disabled
        })
        .with_console(
            match load_settings
                .uefi_console_mode
                .unwrap_or(UefiConsoleMode::Default)
            {
                UefiConsoleMode::Default => config::ConsolePort::Default,
                UefiConsoleMode::Com1 => config::ConsolePort::Com1,
                UefiConsoleMode::Com2 => config::ConsolePort::Com2,
                UefiConsoleMode::None => config::ConsolePort::None,
            },
        );

    let mut cfg = config::Blob::new();
    cfg.add(&config::BiosInformation {
        bios_size_pages: (IMAGE_SIZE / HV_PAGE_SIZE) as u32,
        flags: 0,
    })
    .add_raw(config::BlobStructureType::Madt, madt)
    .add_raw(config::BlobStructureType::Srat, srat)
    .add_raw(config::BlobStructureType::MemoryMap, memory_map.as_bytes())
    .add(&config::BiosGuid(Guid::new_random()))
    .add(&config::Entropy(entropy))
    .add(&config::MmioRanges([
        config::Mmio {
            mmio_page_number_start: low_mmio.start() / HV_PAGE_SIZE,
            mmio_size_in_pages: (low_mmio.end() - low_mmio.start()) / HV_PAGE_SIZE,
        },
        config::Mmio {
            mmio_page_number_start: high_mmio.start() / HV_PAGE_SIZE,
            mmio_size_in_pages: (high_mmio.end() - high_mmio.start()) / HV_PAGE_SIZE,
        },
    ]))
    .add(&config::ProcessorInformation {
        max_processor_count: processor_topology.vp_count(),
        processor_count: processor_topology.vp_count(),
        processors_per_virtual_socket: processor_topology.reserved_vps_per_socket(),
        threads_per_processor: if processor_topology.smt_enabled() {
            2
        } else {
            1
        },
    })
    .add(&flags);

    #[cfg(guest_arch = "aarch64")]
    {
        cfg.add(&config::Gic {
            gic_distributor_base: processor_topology.gic_distributor_base(),
            gic_redistributors_base: processor_topology.gic_redistributors_base(),
        });
    }

    if let Some(pptt) = pptt {
        cfg.add_raw(config::BlobStructureType::Pptt, pptt);
    }

    let mut loader = Loader::new(gm.clone(), mem_layout, hvdef::Vtl::Vtl0);

    loader::uefi::load(
        &mut loader,
        image,
        loader::uefi::ConfigType::ConfigBlob(cfg),
    )
    .map_err(Error::Loader)?;

    Ok(loader.initial_regs())
}
