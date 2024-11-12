// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to read and validate runtime parameters. These come from a variety of
//! sources, such as the host or openhcl_boot.
//!
//! Note that host provided IGVM parameters are untrusted and dynamic at
//! runtime, unlike measured config. Parameters provided by openhcl_boot are
//! expected to be already validated by the bootloader.

use anyhow::Context;
use bootloader_fdt_parser::IsolationType;
use bootloader_fdt_parser::ParsedBootDtInfo;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use loader_defs::paravisor::ParavisorMeasuredVtl2Config;
use loader_defs::paravisor::PARAVISOR_CONFIG_CPUID_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_CONFIG_CPUID_SIZE_PAGES;
use loader_defs::paravisor::PARAVISOR_CONFIG_PPTT_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_CONFIG_SECRETS_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_CONFIG_SECRETS_SIZE_PAGES;
use loader_defs::paravisor::PARAVISOR_CONFIG_SLIT_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX;
use memory_range::MemoryRange;
use sparse_mmap::SparseMapping;
use vm_topology::memory::MemoryRangeWithNode;
use zerocopy::AsBytes;

/// Structure that holds parameters provided at runtime. Some are read from the
/// guest address space, and others from openhcl_boot provided via devicetree.
#[derive(Debug, Inspect)]
pub struct RuntimeParameters {
    parsed_openhcl_boot: ParsedBootDtInfo,
    slit: Option<Vec<u8>>,
    pptt: Option<Vec<u8>>,
    cvm_cpuid_info: Option<Vec<u8>>,
    snp_secrets: Option<Vec<u8>>,
}

impl RuntimeParameters {
    /// The overall memory map of the partition provided by the bootloader,
    /// including VTL2.
    pub fn partition_memory_map(&self) -> &[bootloader_fdt_parser::AddressRange] {
        &self.parsed_openhcl_boot.partition_memory_map
    }

    /// The parsed settings from device tree provided by openhcl_boot.
    pub fn parsed_openhcl_boot(&self) -> &ParsedBootDtInfo {
        &self.parsed_openhcl_boot
    }

    /// A sorted slice representing the memory used by VTL2.
    pub fn vtl2_memory_map(&self) -> &[MemoryRangeWithNode] {
        &self.parsed_openhcl_boot.vtl2_memory
    }

    /// The VM's ACPI SLIT table provided by the host.
    pub fn slit(&self) -> Option<&[u8]> {
        self.slit.as_deref()
    }

    /// The VM's ACPI PPTT table provided by the host.
    pub fn pptt(&self) -> Option<&[u8]> {
        self.pptt.as_deref()
    }

    /// The hardware supplied cpuid information for a CVM.
    pub fn cvm_cpuid_info(&self) -> Option<&[u8]> {
        self.cvm_cpuid_info.as_deref()
    }
    pub fn snp_secrets(&self) -> Option<&[u8]> {
        self.snp_secrets.as_deref()
    }
}

/// Structure that holds the read IGVM parameters from the guest address space.
#[derive(Debug, Inspect)]
pub struct MeasuredVtl2Info {
    #[inspect(with = "inspect_helpers::accepted_regions")]
    accepted_regions: Vec<MemoryRange>,
    pub vtom_offset_bit: Option<u8>,
}

impl MeasuredVtl2Info {
    pub fn accepted_regions(&self) -> &[MemoryRange] {
        &self.accepted_regions
    }
}

#[derive(Debug)]
/// Map of the portion of memory that contains the VTL2 parameters.
///
/// On drop, this mapping zeroes out the specified config ranges.
struct Vtl2ParamsMap<'a> {
    mapping: SparseMapping,
    ranges: &'a [MemoryRange],
}

impl<'a> Vtl2ParamsMap<'a> {
    fn new(config_ranges: &'a [MemoryRange]) -> anyhow::Result<Self> {
        // No overlaps.
        // TODO: Move this check to host_fdt_parser?
        if let Some((l, r)) = config_ranges
            .iter()
            .zip(config_ranges.iter().skip(1))
            .find(|(l, r)| r.start() < l.end())
        {
            anyhow::bail!("vtl-boot-data range {r} overlaps {l}");
        }

        tracing::trace!("boot_data_gpa_ranges {:x?}", config_ranges);

        let base = config_ranges
            .first()
            .context("no vtl-boot-data ranges")?
            .start();
        let size = config_ranges.last().unwrap().end() - base;

        let mapping =
            SparseMapping::new(size as usize).context("failed to create a sparse mapping")?;

        let dev_mem = fs_err::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/mem")?;
        for range in config_ranges {
            mapping
                .map_file(
                    (range.start() - base) as usize,
                    range.len() as usize,
                    dev_mem.file(),
                    range.start(),
                    true,
                )
                .context("failed to memory map igvm parameters")?;
        }

        Ok(Self {
            mapping,
            ranges: config_ranges,
        })
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> anyhow::Result<()> {
        Ok(self.mapping.read_at(offset, buf)?)
    }

    fn read_plain<T: AsBytes + zerocopy::FromBytes>(&self, offset: usize) -> anyhow::Result<T> {
        Ok(self.mapping.read_plain(offset)?)
    }
}

impl Drop for Vtl2ParamsMap<'_> {
    fn drop(&mut self) {
        let base = self
            .ranges
            .first()
            .expect("already checked that there is at least one range")
            .start();

        for range in self.ranges {
            self.mapping
                .fill_at((range.start() - base) as usize, 0, range.len() as usize)
                .unwrap();
        }
    }
}

/// Map that reads the VTL 2 reserved region, that should not be zeroed after
/// reading.
struct Vtl2ReservedMap {
    mapping: SparseMapping,
}

impl Vtl2ReservedMap {
    fn new(reserved_range: MemoryRange) -> anyhow::Result<Self> {
        let size = reserved_range.len() as usize;

        let mapping = SparseMapping::new(size).context("failed to create a sparse mapping")?;

        let dev_mem = fs_err::OpenOptions::new().read(true).open("/dev/mem")?;

        mapping
            .map_file(0, size, dev_mem.file(), reserved_range.start(), false)
            .context("failed to memory map reserved region")?;

        Ok(Self { mapping })
    }
}

/// Reads the VTL 2 parameters from the vtl-boot-data region.
pub fn read_vtl2_params() -> anyhow::Result<(RuntimeParameters, MeasuredVtl2Info)> {
    let parsed_openhcl_boot = ParsedBootDtInfo::new().context("failed to parse openhcl_boot dt")?;

    let mapping = Vtl2ParamsMap::new(&parsed_openhcl_boot.config_ranges)
        .context("failed to map igvm parameters")?;

    // For the various ACPI tables, read the header to see how big the table
    // is, then read the exact table.

    let slit = {
        let table_header: acpi_spec::Header = mapping
            .read_plain((PARAVISOR_CONFIG_SLIT_PAGE_INDEX * HV_PAGE_SIZE) as usize)
            .context("failed to read slit header")?;
        tracing::trace!(?table_header, "Read SLIT ACPI header");

        if table_header.length.get() == 0 {
            None
        } else {
            let mut slit: Vec<u8> = vec![0; table_header.length.get() as usize];
            mapping
                .read_at(
                    (PARAVISOR_CONFIG_SLIT_PAGE_INDEX * HV_PAGE_SIZE) as usize,
                    slit.as_mut_slice(),
                )
                .context("failed to read slit")?;
            Some(slit)
        }
    };

    let pptt = {
        let table_header: acpi_spec::Header = mapping
            .read_plain((PARAVISOR_CONFIG_PPTT_PAGE_INDEX * HV_PAGE_SIZE) as usize)
            .context("failed to read pptt header")?;
        tracing::trace!(?table_header, "Read PPTT ACPI header");

        if table_header.length.get() == 0 {
            None
        } else {
            let mut pptt: Vec<u8> = vec![0; table_header.length.get() as usize];
            mapping
                .read_at(
                    (PARAVISOR_CONFIG_PPTT_PAGE_INDEX * HV_PAGE_SIZE) as usize,
                    pptt.as_mut_slice(),
                )
                .context("failed to read pptt")?;
            Some(pptt)
        }
    };

    let (cvm_cpuid_info, snp_secrets) = {
        if parsed_openhcl_boot.isolation == IsolationType::Snp {
            let mut cpuid_pages: Vec<u8> =
                vec![0; (HV_PAGE_SIZE * PARAVISOR_CONFIG_CPUID_SIZE_PAGES) as usize];
            mapping
                .read_at(
                    (PARAVISOR_CONFIG_CPUID_PAGE_INDEX * HV_PAGE_SIZE) as usize,
                    cpuid_pages.as_mut_slice(),
                )
                .context("failed to read cpuid pages")?;
            let mut secrets =
                vec![0; (HV_PAGE_SIZE * PARAVISOR_CONFIG_SECRETS_SIZE_PAGES) as usize];
            mapping
                .read_at(
                    (HV_PAGE_SIZE * PARAVISOR_CONFIG_SECRETS_PAGE_INDEX) as usize,
                    secrets.as_mut_slice(),
                )
                .context("failed to read secrets page")?;

            (Some(cpuid_pages), Some(secrets))
        } else {
            (None, None)
        }
    };

    let accepted_regions = if parsed_openhcl_boot.isolation != IsolationType::None {
        parsed_openhcl_boot.accepted_ranges.clone()
    } else {
        Vec::new()
    };

    let measured_config = mapping
        .read_plain::<ParavisorMeasuredVtl2Config>(
            (PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX * HV_PAGE_SIZE) as usize,
        )
        .context("failed to read measured vtl2 config")?;

    drop(mapping);

    assert_eq!(measured_config.magic, ParavisorMeasuredVtl2Config::MAGIC);

    let vtom_offset_bit = if measured_config.vtom_offset_bit == 0 {
        None
    } else {
        Some(measured_config.vtom_offset_bit)
    };

    let runtime_params = RuntimeParameters {
        parsed_openhcl_boot,
        slit,
        pptt,
        cvm_cpuid_info,
        snp_secrets,
    };

    let measured_vtl2_info = MeasuredVtl2Info {
        accepted_regions,
        vtom_offset_bit,
    };

    Ok((runtime_params, measured_vtl2_info))
}

mod inspect_helpers {
    use super::*;

    pub(super) fn accepted_regions(regions: &[MemoryRange]) -> impl Inspect + '_ {
        inspect::iter_by_key(
            regions
                .iter()
                .map(|region| (region, inspect::AsDebug(region))), // TODO ??
        )
    }
}
