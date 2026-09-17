// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use loader::importer::X86Register;
use thiserror::Error;
use vm_loader::Loader;
use vm_topology::memory::MemoryLayout;

#[derive(Debug, Error)]
pub enum Error {
    #[error("pcat loader error")]
    Loader(#[source] loader::pcat::Error),
    #[error("{0} is not configurable on PCAT boot")]
    UnsupportedSmbiosField(&'static str),
}

/// Load the PCAT BIOS.
///
/// Since the BIOS is in ROM, this actually just returns the PCAT initial
/// registers.
#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn load_pcat(gm: &GuestMemory, mem_layout: &MemoryLayout) -> Result<Vec<X86Register>, Error> {
    let mut loader = Loader::new(gm.clone(), mem_layout, hvdef::Vtl::Vtl0);
    loader::pcat::load(&mut loader, None, mem_layout.max_ram_below_4gb()).map_err(Error::Loader)?;
    Ok(loader.initial_regs())
}

/// Adapt the shared [`openvmm_defs::config::SmbiosConfig`] into the PCAT BIOS's
/// [`firmware_pcat::config::SmbiosConstants`].
///
/// The PCAT BIOS ROM builds the SMBIOS tables itself and only queries the host
/// for a fixed set of values over an I/O port. Only the system UUID (BIOS GUID)
/// and system serial number have a corresponding query, so those are the only
/// overrides that can be honored. Every other override is rejected (fail
/// closed) rather than being silently dropped, mirroring the UEFI path's
/// handling of fields the firmware self-describes.
#[cfg_attr(not(guest_arch = "x86_64"), expect(dead_code))]
pub fn smbios_constants_from_config(
    smbios: &openvmm_defs::config::SmbiosConfig,
) -> Result<firmware_pcat::config::SmbiosConstants, Error> {
    let openvmm_defs::config::SmbiosConfig {
        bios:
            openvmm_defs::config::SmbiosBiosOverrides {
                vendor,
                version: bios_version,
                release_date,
                release,
            },
        system:
            openvmm_defs::config::SmbiosSystemOverrides {
                manufacturer,
                product_name,
                version: system_version,
                serial_number,
                sku_number,
                family,
                uuid,
            },
    } = smbios;

    // Type 0 (BIOS Information): the PCAT BIOS ROM self-describes the BIOS, so
    // reject any override rather than silently dropping it.
    if vendor.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS BIOS vendor"));
    }
    if bios_version.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS BIOS version"));
    }
    if release_date.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS BIOS release date"));
    }
    if release.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS BIOS release"));
    }

    // Type 1 (System Information): the BIOS ROM hardcodes these strings and
    // exposes no query for them, so reject overrides. Only the UUID and serial
    // number are delivered to the ROM.
    if manufacturer.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS system manufacturer"));
    }
    if product_name.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS system product name"));
    }
    if system_version.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS system version"));
    }
    if sku_number.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS system SKU number"));
    }
    if family.is_some() {
        return Err(Error::UnsupportedSmbiosField("SMBIOS system family"));
    }

    Ok(firmware_pcat::config::SmbiosConstants {
        bios_guid: *uuid,
        system_serial_number: serial_number.clone().unwrap_or_default().into_bytes(),
        // The remaining fields have no shared-config representation yet. Leave
        // them empty.
        base_board_serial_number: Vec::new(),
        chassis_serial_number: Vec::new(),
        chassis_asset_tag: Vec::new(),
        bios_lock_string: Vec::new(),
        processor_manufacturer: b"\0".to_vec(),
        processor_version: b"\0".to_vec(),
        cpu_info_bundle: None,
    })
}
