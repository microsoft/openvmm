// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::worker::memory_layout::ChipsetMmioRanges;
use guestmem::GuestMemory;
use guid::Guid;
use hvdef::HV_PAGE_SIZE;
use loader::importer::Register;
use loader::uefi::IMAGE_SIZE;
use loader::uefi::config;
use openvmm_defs::config::UefiConsoleMode;
use std::io::Read;
use std::io::Seek;
use thiserror::Error;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;
use vm_topology::pcie::PcieHostBridge;
use vm_topology::processor::ProcessorTopology;
use zerocopy::IntoBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read uefi firmware file")]
    Firmware(#[source] std::io::Error),
    #[error("uefi loader error")]
    Loader(#[source] loader::uefi::Error),
    #[error("failed to build PCIe ACPI tables")]
    PcieAcpi(#[source] vmm_core::acpi_builder::PcieAcpiBuildError),
    #[cfg(guest_arch = "aarch64")]
    #[error("UEFI boot with GICv2 is not supported")]
    GicV2NotSupported,
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
    pub default_boot_always_attempt: bool,
    pub bios_guid: Guid,
    /// Whether VMBus is present in this VM. When `false`, the firmware's
    /// `vmbus_disabled` flag is set; the `MmioRanges` blob is still provided
    /// but the high MMIO range will be empty. The firmware must support this
    /// mode.
    pub vmbus: bool,
}

/// Loads the UEFI firmware.
///
/// If `firmware` is `None`, load the embedded firmware.
pub fn load_uefi(
    mut firmware: &std::fs::File,
    gm: &GuestMemory,
    processor_topology: &ProcessorTopology,
    mem_layout: &MemoryLayout,
    pcie_host_bridges: &[PcieHostBridge],
    load_settings: UefiLoadSettings,
    chipset_mmio: &ChipsetMmioRanges,
    madt: &[u8],
    srat: &[u8],
    mcfg: Option<&[u8]>,
    pptt: Option<&[u8]>,
) -> Result<Vec<Register>, Error> {
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
    getrandom::fill(&mut entropy).expect("rng failure");

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
        )
        .with_default_boot_always_attempt(load_settings.default_boot_always_attempt)
        .with_vmbus_disabled(!load_settings.vmbus);

    let mut cfg = config::Blob::new();
    cfg.add(&config::BiosInformation {
        bios_size_pages: (IMAGE_SIZE / HV_PAGE_SIZE) as u32,
        flags: 0,
    })
    .add_raw(config::BlobStructureType::Madt, madt)
    .add_raw(config::BlobStructureType::Srat, srat)
    .add_raw(config::BlobStructureType::MemoryMap, memory_map.as_bytes())
    .add(&config::BiosGuid(load_settings.bios_guid))
    .add(&config::Entropy(entropy))
    .add(&config::MmioRanges([
        config::Mmio {
            mmio_page_number_start: chipset_mmio.low.start() / HV_PAGE_SIZE,
            mmio_size_in_pages: chipset_mmio.low.len() / HV_PAGE_SIZE,
        },
        config::Mmio {
            mmio_page_number_start: chipset_mmio.high.start() / HV_PAGE_SIZE,
            mmio_size_in_pages: chipset_mmio.high.len() / HV_PAGE_SIZE,
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
        let redistributors_base = match processor_topology.gic_version() {
            vm_topology::processor::aarch64::GicVersion::V3 {
                redistributors_base,
            } => redistributors_base,
            vm_topology::processor::aarch64::GicVersion::V2 { .. } => {
                return Err(Error::GicV2NotSupported);
            }
        };
        cfg.add(&config::Gic {
            gic_distributor_base: processor_topology.gic_distributor_base(),
            gic_redistributors_base: redistributors_base,
        });
    }

    if let Some(mcfg) = mcfg {
        cfg.add_raw(config::BlobStructureType::Mcfg, mcfg);
    }

    if let Some(pptt) = pptt {
        cfg.add_raw(config::BlobStructureType::Pptt, pptt);
    }

    if !pcie_host_bridges.is_empty() {
        let pcie_tables = vmm_core::acpi_builder::build_pcie_acpi_tables(pcie_host_bridges)
            .map_err(Error::PcieAcpi)?;
        cfg.add_raw(config::BlobStructureType::Ssdt, &pcie_tables.ssdt);
        if let Some(cedt) = pcie_tables.cedt {
            cfg.add_raw(config::BlobStructureType::AcpiTable, &cedt);
        }
    }

    if !pcie_host_bridges.is_empty() {
        let entries: Vec<config::PcieBarApertureEntry> = pcie_host_bridges
            .iter()
            .map(|b| config::PcieBarApertureEntry {
                segment: b.segment,
                start_bus: b.start_bus,
                end_bus: b.end_bus,
                uid: b.index,
                low_mmio_base: b.low_mmio.start(),
                low_mmio_length: b.low_mmio.len(),
                high_mmio_base: b.high_mmio.start(),
                high_mmio_length: b.high_mmio.len(),
            })
            .collect();
        cfg.add_raw(
            config::BlobStructureType::PcieBarApertures,
            entries.as_bytes(),
        );
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
