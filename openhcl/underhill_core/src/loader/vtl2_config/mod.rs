// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to read and validate runtime parameters. These come from a variety of
//! sources, such as the host or openhcl_boot.
//!
//! Note that host provided IGVM parameters are untrusted and dynamic at
//! runtime, unlike measured config. Parameters provided by openhcl_boot are
//! expected to be already validated by the bootloader.

use crate::nvme_manager::save_restore_helpers::VPInterruptState;
use acpi::slit::Slit;
use anyhow::Context;
use bootloader_fdt_parser::IsolationType;
use bootloader_fdt_parser::ParsedBootDtInfo;
use cvm_tracing::CVM_ALLOWED;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use loader_defs::paravisor;
use loader_defs::paravisor::PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_RESERVED_VTL2_SNP_CPUID_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_RESERVED_VTL2_SNP_CPUID_SIZE_PAGES;
use loader_defs::paravisor::PARAVISOR_RESERVED_VTL2_SNP_SECRETS_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_RESERVED_VTL2_SNP_SECRETS_SIZE_PAGES;
#[cfg(feature = "product_policy")]
use loader_defs::paravisor::PRODUCT_POLICY_INLINE_OFFSET;
#[cfg(feature = "product_policy")]
use loader_defs::paravisor::PRODUCT_POLICY_MAX_SIZE_BYTES;
use loader_defs::paravisor::ParavisorMeasuredVtl2Config;
use loader_defs::shim::MemoryVtlType;
use memory_range::MemoryRange;
use sparse_mmap::SparseMapping;
use string_page_buf::StringBuffer;
use vm_topology::memory::MemoryRangeWithNode;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

pub(super) const SLIT_MAX_SIZE: usize =
    (paravisor::PARAVISOR_CONFIG_SLIT_SIZE_PAGES * HV_PAGE_SIZE) as usize;

/// Parsed SLIT data and original table bytes retained for non-isolated VMs.
#[derive(Debug, Inspect)]
pub(super) struct SlitParameter {
    #[inspect(flatten, with = "inspect_slit")]
    pub(super) parsed: Slit,
    pub(super) host_igvm_parameter: Option<Vec<u8>>,
}

/// Inspects the locality count and overrides of the default SLIT distances.
fn inspect_slit(slit: &Slit) -> impl Inspect + '_ {
    inspect::adhoc(move |req| {
        req.respond()
            .field("num_nodes", slit.num_nodes())
            .child("distance_overrides", |req| {
                let mut resp = req.respond();
                for (src, dst, distance) in slit.distances() {
                    resp.field(&format!("{src}->{dst}"), distance);
                }
            });
    })
}

impl SlitParameter {
    /// Parses a SLIT and retains its original bytes only for non-isolated VMs.
    fn new(bytes: Vec<u8>, isolated: bool) -> anyhow::Result<Self> {
        let parsed = Slit::parse(&bytes, SLIT_MAX_SIZE).context("parsing IGVM SLIT")?;
        Ok(Self {
            parsed,
            host_igvm_parameter: (!isolated).then_some(bytes),
        })
    }
}

/// Structure that holds parameters provided at runtime. Some are read from the
/// guest address space, and others from openhcl_boot provided via devicetree.
#[derive(Debug, Inspect)]
pub struct RuntimeParameters {
    parsed_openhcl_boot: ParsedBootDtInfo,
    slit: Option<SlitParameter>,
    pptt: Option<Vec<u8>>,
    cvm_cpuid_info: Option<Vec<u8>>,
    snp_secrets: Option<Vec<u8>>,
    #[inspect(iter_by_index)]
    bootshim_logs: Vec<String>,
    bootshim_log_dropped: u16,
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

    /// The VM's parsed SLIT and original table bytes provided by the host.
    pub(super) fn slit(&self) -> Option<&SlitParameter> {
        self.slit.as_ref()
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

    /// The memory ranges to use for the private pool
    pub fn private_pool_ranges(&self) -> &[MemoryRangeWithNode] {
        &self.parsed_openhcl_boot.private_pool_ranges
    }
}

/// Structure that holds the read IGVM parameters from the guest address space.
#[derive(Debug, Inspect)]
pub struct MeasuredVtl2Info {
    #[inspect(with = "inspect_helpers::accepted_regions")]
    accepted_regions: Vec<MemoryRange>,
    pub vtom_offset_bit: Option<u8>,
    /// Per-VM measured product policy. Built once during VTL2
    /// config read and cloned into `LoadedVm`.
    #[cfg(feature = "product_policy")]
    measured_product_policy: product_policy::MeasuredProductPolicy,
}

impl MeasuredVtl2Info {
    pub fn accepted_regions(&self) -> &[MemoryRange] {
        &self.accepted_regions
    }

    #[cfg(feature = "product_policy")]
    pub fn measured_product_policy(&self) -> &product_policy::MeasuredProductPolicy {
        &self.measured_product_policy
    }
}

/// Map of the portion of memory that contains the VTL2 parameters to read.
///
/// If configured, on drop this mapping zeroes out the specified config ranges.
struct Vtl2ParamsMap<'a> {
    mapping: SparseMapping,
    zero_on_drop: bool,
    ranges: &'a [MemoryRange],
}

#[derive(Debug, thiserror::Error)]
enum AcpiParameterError {
    #[error("ACPI parameter offset overflow")]
    OffsetOverflow,
    #[error("reading ACPI parameter failed")]
    Read(#[source] anyhow::Error),
    #[error("ACPI parameter header is truncated")]
    TruncatedHeader,
    #[error("ACPI parameter signature {actual:?} does not match {expected:?}")]
    Signature { expected: [u8; 4], actual: [u8; 4] },
    #[error("ACPI parameter length {length} is outside the permitted header..={limit} range")]
    Length { length: usize, limit: usize },
    #[error("ACPI parameter length changed while reading")]
    ChangedLength,
    #[error("ACPI parameter checksum is invalid")]
    Checksum,
}

/// Checks an ACPI table's signature, exact length, and checksum, not its body.
fn validate_parameter(
    bytes: &[u8],
    expected_signature: [u8; 4],
    max_size: usize,
) -> Result<(), AcpiParameterError> {
    let (header, _) = acpi_spec::Header::read_from_prefix(bytes)
        .map_err(|_| AcpiParameterError::TruncatedHeader)?;
    if header.signature != expected_signature {
        return Err(AcpiParameterError::Signature {
            expected: expected_signature,
            actual: header.signature,
        });
    }

    let length = header.length.get() as usize;
    if !(size_of::<acpi_spec::Header>()..=max_size).contains(&length) {
        return Err(AcpiParameterError::Length {
            length,
            limit: max_size,
        });
    }
    if length != bytes.len() {
        return Err(AcpiParameterError::ChangedLength);
    }

    if bytes.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) != 0 {
        return Err(AcpiParameterError::Checksum);
    }
    Ok(())
}

/// Reads an ACPI table from the caller's offset within the size limit.
/// Zero declared length means no table. Otherwise, checks the signature,
/// declared length, and checksum of the table read into the buffer.
fn read_acpi_parameter(
    offset: usize,
    max_size: usize,
    expected_signature: [u8; 4],
    read: &mut impl FnMut(usize, &mut [u8]) -> anyhow::Result<()>,
) -> Result<Option<Vec<u8>>, AcpiParameterError> {
    let mut header_bytes = [0; size_of::<acpi_spec::Header>()];
    if max_size < header_bytes.len() {
        return Err(AcpiParameterError::Length {
            length: header_bytes.len(),
            limit: max_size,
        });
    }
    offset
        .checked_add(max_size)
        .ok_or(AcpiParameterError::OffsetOverflow)?;
    read(offset, &mut header_bytes).map_err(AcpiParameterError::Read)?;
    let header = acpi_spec::Header::read_from_bytes(&header_bytes)
        .map_err(|_| AcpiParameterError::TruncatedHeader)?;

    let length = header.length.get() as usize;
    if length == 0 {
        return Ok(None);
    }
    if header.signature != expected_signature {
        return Err(AcpiParameterError::Signature {
            expected: expected_signature,
            actual: header.signature,
        });
    }
    if !(header_bytes.len()..=max_size).contains(&length) {
        return Err(AcpiParameterError::Length {
            length,
            limit: max_size,
        });
    }

    let mut bytes = vec![0; length];
    read(offset, &mut bytes).map_err(AcpiParameterError::Read)?;

    validate_parameter(&bytes, expected_signature, max_size)?;
    Ok(Some(bytes))
}

impl<'a> Vtl2ParamsMap<'a> {
    fn new_internal(
        ranges: &'a [MemoryRange],
        writeable: bool,
        zero_on_drop: bool,
    ) -> anyhow::Result<Self> {
        // No overlaps.
        if let Some((l, r)) = ranges
            .iter()
            .zip(ranges.iter().skip(1))
            .find(|(l, r)| r.start() < l.end())
        {
            anyhow::bail!("range {r} overlaps {l}");
        }

        tracing::trace!("requested mapping ranges {:x?}", ranges);

        let base = ranges.first().context("no ranges")?.start();
        let size = ranges.last().unwrap().end() - base;

        let mapping = SparseMapping::new(size as usize)
            .context("failed to create a sparse mapping for vtl2params")?;

        let writeable = writeable || zero_on_drop;
        let dev_mem = fs_err::OpenOptions::new()
            .read(true)
            .write(writeable)
            .open("/dev/mem")?;
        for range in ranges {
            mapping
                .map_file(
                    (range.start() - base) as usize,
                    range.len() as usize,
                    dev_mem.file(),
                    range.start(),
                    writeable,
                )
                .context("failed to memory map igvm parameters")?;
        }

        Ok(Self {
            mapping,
            ranges,
            zero_on_drop,
        })
    }

    fn new(config_ranges: &'a [MemoryRange], zero_on_drop: bool) -> anyhow::Result<Self> {
        Self::new_internal(config_ranges, false, zero_on_drop)
    }

    // TODO: Consider not using /dev/mem and instead using mshv_vtl_low, which
    // would require not describing the memory to the kernel in the E820 map.
    fn new_writeable(ranges: &'a [MemoryRange]) -> anyhow::Result<Self> {
        Self::new_internal(ranges, true, false)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> anyhow::Result<()> {
        Ok(self.mapping.write_at(offset, buf)?)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> anyhow::Result<()> {
        Ok(self.mapping.read_at(offset, buf)?)
    }

    fn read_plain<T: IntoBytes + FromBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
    ) -> anyhow::Result<T> {
        Ok(self.mapping.read_plain(offset)?)
    }
}

impl Drop for Vtl2ParamsMap<'_> {
    fn drop(&mut self) {
        if self.zero_on_drop {
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
}

/// Write persisted info into the bootshim described persisted region.
pub fn write_persisted_info(
    parsed: &ParsedBootDtInfo,
    interrupt_state: VPInterruptState,
) -> anyhow::Result<()> {
    use loader_defs::shim::PersistedStateHeader;
    use loader_defs::shim::save_restore::MemoryEntry;
    use loader_defs::shim::save_restore::MmioEntry;
    use loader_defs::shim::save_restore::SavedState;

    tracing::trace!(
        protobuf_region = ?parsed.vtl2_persisted_protobuf_region,
        "writing persisted protobuf"
    );

    let ranges = [parsed.vtl2_persisted_protobuf_region];
    let mapping =
        Vtl2ParamsMap::new_writeable(&ranges).context("failed to map persisted protobuf region")?;

    let VPInterruptState {
        vps_with_mapped_interrupts_no_io: cpus_with_mapped_interrupts_no_io,
        vps_with_outstanding_io: cpus_with_outstanding_io,
    } = interrupt_state;

    // Create the serialized data to write.
    let state = SavedState {
        partition_memory: parsed
            .partition_memory_map
            .iter()
            .filter_map(|r| match r {
                bootloader_fdt_parser::AddressRange::Memory(memory) => Some(MemoryEntry {
                    range: memory.range.range,
                    vnode: memory.range.vnode,
                    vtl_type: memory.vtl_usage,
                    igvm_type: memory.igvm_type.into(),
                }),
                bootloader_fdt_parser::AddressRange::Mmio(_) => None,
            })
            .collect(),
        partition_mmio: parsed
            .partition_memory_map
            .iter()
            .filter_map(|r| match r {
                bootloader_fdt_parser::AddressRange::Mmio(mmio) => Some(MmioEntry {
                    range: mmio.range,
                    vtl_type: match mmio.vtl {
                        bootloader_fdt_parser::Vtl::Vtl0 => MemoryVtlType::VTL0_MMIO,
                        bootloader_fdt_parser::Vtl::Vtl2 => MemoryVtlType::VTL2_MMIO,
                    },
                }),
                bootloader_fdt_parser::AddressRange::Memory(_) => None,
            })
            .collect(),
        cpus_with_mapped_interrupts_no_io,
        cpus_with_outstanding_io,
    };

    let protobuf = mesh_protobuf::encode(state);
    tracing::trace!(len = protobuf.len(), "persisted protobuf len");

    mapping
        .write_at(0, protobuf.as_bytes())
        .context("failed to write persisted state protobuf")?;

    tracing::trace!(
        header_region = ?parsed.vtl2_persisted_header,
        "writing persisted header"
    );

    let ranges = [parsed.vtl2_persisted_header];
    let mapping =
        Vtl2ParamsMap::new_writeable(&ranges).context("unable to map persisted header")?;

    let header = PersistedStateHeader {
        magic: PersistedStateHeader::MAGIC,
        protobuf_base: parsed.vtl2_persisted_protobuf_region.start(),
        protobuf_region_len: parsed.vtl2_persisted_protobuf_region.len(),
        protobuf_payload_len: protobuf.len() as u64,
    };

    mapping.write_at(0, header.as_bytes())?;

    Ok(())
}

/// Reads and parses the supplied SLIT, and reads PPTT for non-isolated VMs.
fn read_acpi_parameters(
    isolated: bool,
    mapping: &Vtl2ParamsMap<'_>,
) -> anyhow::Result<(Option<SlitParameter>, Option<Vec<u8>>)> {
    let mut read = |offset, bytes: &mut [u8]| mapping.read_at(offset, bytes);
    let slit = read_acpi_parameter(
        (paravisor::PARAVISOR_CONFIG_SLIT_PAGE_INDEX * HV_PAGE_SIZE) as usize,
        SLIT_MAX_SIZE,
        *b"SLIT",
        &mut read,
    )
    .context("reading IGVM SLIT")?
    .map(|bytes| SlitParameter::new(bytes, isolated))
    .transpose()?;

    let pptt = if !isolated {
        read_acpi_parameter(
            (paravisor::PARAVISOR_CONFIG_PPTT_PAGE_INDEX * HV_PAGE_SIZE) as usize,
            (paravisor::PARAVISOR_CONFIG_PPTT_SIZE_PAGES * HV_PAGE_SIZE) as usize,
            *b"PPTT",
            &mut read,
        )
        .context("reading IGVM PPTT")?
    } else {
        None
    };
    Ok((slit, pptt))
}

/// Reads the VTL 2 parameters from the config region and VTL2 reserved region.
pub fn read_vtl2_params() -> anyhow::Result<(RuntimeParameters, MeasuredVtl2Info)> {
    let parsed_openhcl_boot = ParsedBootDtInfo::new().context("failed to parse openhcl_boot dt")?;

    let mapping = Vtl2ParamsMap::new(&parsed_openhcl_boot.config_ranges, true)
        .context("failed to map igvm parameters")?;

    let isolated = parsed_openhcl_boot.isolation != IsolationType::None;
    let (slit, pptt) = read_acpi_parameters(isolated, &mapping)?;

    // Read SNP specific information from the reserved region.
    let (cvm_cpuid_info, snp_secrets) = {
        if parsed_openhcl_boot.isolation == IsolationType::Snp {
            let ranges = &[parsed_openhcl_boot.vtl2_reserved_range];
            let reserved_mapping =
                Vtl2ParamsMap::new(ranges, false).context("failed to map vtl2 reserved region")?;

            let mut cpuid_pages: Vec<u8> =
                vec![0; (PARAVISOR_RESERVED_VTL2_SNP_CPUID_SIZE_PAGES * HV_PAGE_SIZE) as usize];
            reserved_mapping
                .read_at(
                    (PARAVISOR_RESERVED_VTL2_SNP_CPUID_PAGE_INDEX * HV_PAGE_SIZE) as usize,
                    cpuid_pages.as_mut_slice(),
                )
                .context("failed to read cpuid pages")?;
            let mut secrets =
                vec![0; (PARAVISOR_RESERVED_VTL2_SNP_SECRETS_SIZE_PAGES * HV_PAGE_SIZE) as usize];
            reserved_mapping
                .read_at(
                    (PARAVISOR_RESERVED_VTL2_SNP_SECRETS_PAGE_INDEX * HV_PAGE_SIZE) as usize,
                    secrets.as_mut_slice(),
                )
                .context("failed to read secrets page")?;

            (Some(cpuid_pages), Some(secrets))
        } else {
            (None, None)
        }
    };

    // Read bootshim logs.
    let (bootshim_logs, bootshim_log_dropped) = {
        let range = *parsed_openhcl_boot
            .partition_memory_map
            .iter()
            .find(|range| range.vtl_usage() == MemoryVtlType::VTL2_BOOTSHIM_LOG_BUFFER)
            .context("no bootshim log buffer found")?
            .range();
        let ranges = &[range];
        let mapping =
            Vtl2ParamsMap::new(ranges, false).context("failed to map bootshim log buffer")?;

        let mut raw = vec![0; range.len() as usize];
        mapping
            .read_at(0, raw.as_mut_slice())
            .context("unable to read raw bootshim logs")?;

        let buf = StringBuffer::from_existing(raw.as_mut_slice())
            .context("bootshim buffer contents invalid")?;

        let bootshim_log_dropped = buf.dropped_messages();
        if bootshim_log_dropped != 0 {
            tracing::warn!(
                CVM_ALLOWED,
                bootshim_log_dropped,
                "bootshim logger dropped messages"
            );
        }

        (
            buf.contents().lines().map(|s| s.to_string()).collect(),
            bootshim_log_dropped,
        )
    };

    for line in &bootshim_logs {
        tracing::info!(CVM_ALLOWED, line, "openhcl_boot log");
    }

    let accepted_regions = if parsed_openhcl_boot.isolation != IsolationType::None {
        parsed_openhcl_boot.accepted_ranges.clone()
    } else {
        Vec::new()
    };

    // The optional `ProductPolicy` payload is appended in-place
    // immediately after `ParavisorMeasuredVtl2Config`. Its byte
    // length lives in `product_policy_size`; 0 (including the
    // all-zero trailing bytes of pre-feature IGVMs) means absent.
    let measured_config = mapping
        .read_plain::<ParavisorMeasuredVtl2Config>(
            (PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX * HV_PAGE_SIZE) as usize,
        )
        .context("failed to read measured vtl2 config")?;

    assert_eq!(measured_config.magic, ParavisorMeasuredVtl2Config::MAGIC);

    // The optional `ProductPolicy` payload is only read when the
    // `product_policy` feature is enabled; otherwise it is ignored.
    #[cfg(feature = "product_policy")]
    let product_policy = {
        let size = measured_config.product_policy_size as usize;
        if size == 0 {
            product_policy::MeasuredProductPolicy::new(None)
        } else {
            // Defence-in-depth: the IGVM importer caps this at build
            // time; reject anything larger rather than reading past
            // the reserved region.
            if size > PRODUCT_POLICY_MAX_SIZE_BYTES {
                anyhow::bail!(
                    "product policy size {size} exceeds maximum {}",
                    PRODUCT_POLICY_MAX_SIZE_BYTES
                );
            }
            let off = (PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX * HV_PAGE_SIZE) as usize
                + PRODUCT_POLICY_INLINE_OFFSET;
            let mut buf = vec![0u8; size];
            mapping
                .read_at(off, buf.as_mut_slice())
                .context("failed to read product policy bytes")?;

            crate::measured_product_policy::decode(&buf, size)?
        }
    };

    // Product policy support is not compiled in; the measured config must
    // not carry one.
    #[cfg(not(feature = "product_policy"))]
    assert_eq!(
        measured_config.product_policy_size, 0,
        "measured config carries a product policy but the product_policy feature is not enabled"
    );

    drop(mapping);

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
        bootshim_logs,
        bootshim_log_dropped,
    };

    let measured_vtl2_info = MeasuredVtl2Info {
        accepted_regions,
        vtom_offset_bit,
        #[cfg(feature = "product_policy")]
        measured_product_policy: product_policy,
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use test_with_tracing::test;

    pub(crate) fn checksum(table: &mut [u8]) {
        table[9] = 0;
        table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |s, &b| s.wrapping_add(b)));
    }

    pub(crate) fn table(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut table = vec![0; 36];
        table[..4].copy_from_slice(signature);
        table[8] = 1;
        table[10..16].copy_from_slice(b"HOST  ");
        table[16..24].copy_from_slice(b"HOSTIGVM");
        table.extend_from_slice(body);
        let length = table.len() as u32;
        table[4..8].copy_from_slice(&length.to_le_bytes());
        checksum(&mut table);
        table
    }

    pub(crate) fn slit_bytes() -> Vec<u8> {
        let mut body = 2u64.to_le_bytes().to_vec();
        body.extend_from_slice(&[10, 17, 29, 10]);
        table(b"SLIT", &body)
    }

    /// Backs parameter reads with anonymous memory, leaving PPTT inaccessible
    /// when no PPTT fixture is supplied.
    fn acpi_mapping(slit: &[u8], pptt: Option<&[u8]>) -> anyhow::Result<Vtl2ParamsMap<'static>> {
        let page_size = SparseMapping::page_size();
        let slit_offset = (paravisor::PARAVISOR_CONFIG_SLIT_PAGE_INDEX * HV_PAGE_SIZE) as usize;
        let pptt_offset = (paravisor::PARAVISOR_CONFIG_PPTT_PAGE_INDEX * HV_PAGE_SIZE) as usize;
        let size = (pptt_offset
            + (paravisor::PARAVISOR_CONFIG_PPTT_SIZE_PAGES * HV_PAGE_SIZE) as usize)
            .next_multiple_of(page_size);
        let mapping = SparseMapping::new(size)?;
        if pptt.is_some() {
            mapping.alloc(0, size)?;
        } else {
            let start = slit_offset / page_size * page_size;
            let end = (slit_offset + slit.len().max(size_of::<acpi_spec::Header>()))
                .next_multiple_of(page_size);
            mapping.alloc(start, end - start)?;
        }
        mapping.write_at(slit_offset, slit)?;
        if let Some(pptt) = pptt {
            mapping.write_at(pptt_offset, pptt)?;
        }
        Ok(Vtl2ParamsMap {
            mapping,
            ranges: &[],
            zero_on_drop: false,
        })
    }

    pub(crate) fn runtime(
        slit: &[u8],
        pptt: &[u8],
        isolated: bool,
    ) -> anyhow::Result<RuntimeParameters> {
        let mapping = acpi_mapping(slit, (!isolated).then_some(pptt))?;
        let (slit, pptt) = read_acpi_parameters(isolated, &mapping)?;
        Ok(RuntimeParameters {
            parsed_openhcl_boot: ParsedBootDtInfo {
                cpus: vec![],
                vtl0_alias_map: None,
                vtl2_memory: vec![],
                partition_memory_map: vec![],
                vtl0_mmio: vec![],
                config_ranges: vec![],
                vtl2_reserved_range: MemoryRange::EMPTY,
                vtl2_persisted_header: MemoryRange::EMPTY,
                vtl2_persisted_protobuf_region: MemoryRange::EMPTY,
                accepted_ranges: vec![],
                memory_allocation_mode: bootloader_fdt_parser::MemoryAllocationMode::Host,
                isolation: if isolated {
                    IsolationType::Vbs
                } else {
                    IsolationType::None
                },
                private_pool_ranges: vec![],
                gic: None,
            },
            slit,
            pptt,
            cvm_cpuid_info: None,
            snp_secrets: None,
            bootshim_logs: vec![],
            bootshim_log_dropped: 0,
        })
    }

    /// Isolated parsing must not read PPTT, even when its backing is absent.
    #[test]
    fn isolated_parameters_skip_inaccessible_pptt() {
        let mapping = acpi_mapping(&slit_bytes(), None).unwrap();
        let pptt_offset = (paravisor::PARAVISOR_CONFIG_PPTT_PAGE_INDEX * HV_PAGE_SIZE) as usize;
        let mut header = [0; size_of::<acpi_spec::Header>()];
        assert!(mapping.read_at(pptt_offset, &mut header).is_err());

        let (slit, pptt) = read_acpi_parameters(true, &mapping).unwrap();
        assert!(slit.is_some());
        assert!(pptt.is_none());

        let error = read_acpi_parameters(false, &mapping).unwrap_err();
        assert_eq!(error.to_string(), "reading IGVM PPTT");
        assert!(matches!(
            error.downcast_ref::<AcpiParameterError>(),
            Some(AcpiParameterError::Read(_))
        ));
    }

    fn read_table(bytes: &[u8], signature: [u8; 4]) -> Result<Option<Vec<u8>>, AcpiParameterError> {
        read_acpi_parameter(4096, SLIT_MAX_SIZE, signature, &mut |offset, output| {
            assert_eq!(offset, 4096);
            output.copy_from_slice(&bytes[..output.len()]);
            Ok(())
        })
    }

    #[test]
    fn signatures_and_changes_between_reads() {
        assert!(matches!(
            read_table(&table(b"PPTT", &[]), *b"SLIT"),
            Err(AcpiParameterError::Signature { .. })
        ));
        assert!(matches!(
            read_table(&slit_bytes(), *b"PPTT"),
            Err(AcpiParameterError::Signature { .. })
        ));
        let valid = slit_bytes();
        for change in 0..3 {
            let mut calls = 0;
            let result = read_acpi_parameter(0, SLIT_MAX_SIZE, *b"SLIT", &mut |_, output| {
                calls += 1;
                output.copy_from_slice(&valid[..output.len()]);
                if calls == 2 {
                    match change {
                        0 => {
                            output[..4].copy_from_slice(b"PPTT");
                            checksum(output);
                        }
                        1 => output[4..8].copy_from_slice(&36u32.to_le_bytes()),
                        _ => output[9] = output[9].wrapping_add(1),
                    }
                }
                Ok(())
            });
            match change {
                0 => assert!(matches!(result, Err(AcpiParameterError::Signature { .. }))),
                1 => assert!(matches!(result, Err(AcpiParameterError::ChangedLength))),
                _ => assert!(matches!(result, Err(AcpiParameterError::Checksum))),
            }
            assert_eq!(calls, 2);
        }
    }

    #[test]
    fn bounds_absence_and_read_failures() {
        for length in [1, 35, SLIT_MAX_SIZE + 1] {
            let mut bytes = table(b"SLIT", &[]);
            bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
            let mut calls = 0;
            let result = read_acpi_parameter(0, SLIT_MAX_SIZE, *b"SLIT", &mut |_, output| {
                calls += 1;
                output.copy_from_slice(&bytes);
                Ok(())
            });
            assert!(matches!(result, Err(AcpiParameterError::Length { .. })));
            assert_eq!(calls, 1);
        }
        for length in [36, SLIT_MAX_SIZE] {
            let bytes = table(b"SLIT", &vec![0; length - 36]);
            assert_eq!(read_table(&bytes, *b"SLIT").unwrap().unwrap(), bytes);
        }
        assert!(read_table(&[0; 36], *b"SLIT").unwrap().is_none());
        for fail_on in [1, 2] {
            let bytes = slit_bytes();
            let mut calls = 0;
            let result = read_acpi_parameter(0, SLIT_MAX_SIZE, *b"SLIT", &mut |_, output| {
                calls += 1;
                anyhow::ensure!(calls != fail_on, "injected read failure");
                output.copy_from_slice(&bytes[..output.len()]);
                Ok(())
            });
            assert!(matches!(result, Err(AcpiParameterError::Read(_))));
        }
        assert!(matches!(
            read_acpi_parameter(usize::MAX, SLIT_MAX_SIZE, *b"SLIT", &mut |_, _| {
                panic!("overflow must fail before reading")
            }),
            Err(AcpiParameterError::OffsetOverflow)
        ));
        assert!(matches!(
            read_acpi_parameter(0, 35, *b"SLIT", &mut |_, _| {
                panic!("short region must fail before reading")
            }),
            Err(AcpiParameterError::Length { .. })
        ));
    }

    #[test]
    fn table_byte_ownership_and_isolated_pptt() {
        let bytes = slit_bytes();
        let pointer = bytes.as_ptr();
        let slit = SlitParameter::new(bytes.clone(), true).unwrap();
        assert!(slit.host_igvm_parameter.is_none());
        let slit = SlitParameter::new(bytes, false).unwrap();
        assert_eq!(slit.host_igvm_parameter.as_ref().unwrap().as_ptr(), pointer);
        assert_eq!(
            slit.parsed.distances().collect::<Vec<_>>(),
            [(0, 1, 17), (1, 0, 29)]
        );

        let isolated = runtime(&slit_bytes(), &[], true).unwrap();
        assert!(isolated.pptt().is_none());
        assert!(isolated.slit().unwrap().host_igvm_parameter.is_none());
        let nonisolated = runtime(&slit_bytes(), &table(b"PPTT", &[]), false).unwrap();
        assert_eq!(
            nonisolated.slit().unwrap().host_igvm_parameter.as_deref(),
            Some(slit_bytes().as_slice())
        );
        assert_eq!(nonisolated.pptt().unwrap(), table(b"PPTT", &[]));
    }

    #[test]
    fn slit_parameters_are_inspectable() {
        let params = runtime(&slit_bytes(), &[0; 36], false).unwrap();
        for (path, expected) in [
            ("slit/num_nodes", 2u64),
            ("slit/distance_overrides/0->1", 17),
            ("slit/distance_overrides/1->0", 29),
        ] {
            assert_eq!(
                inspect::inspect(path, &params).results(),
                inspect::Node::Value(expected.into())
            );
        }
        assert_eq!(
            inspect::inspect("slit/host_igvm_parameter", &params).results(),
            inspect::inspect("", slit_bytes().as_slice()).results()
        );
        let isolated = runtime(&slit_bytes(), &[], true).unwrap();
        assert_eq!(
            inspect::inspect("slit/host_igvm_parameter", &isolated).results(),
            inspect::Node::Failed(inspect::Error::NotFound)
        );
    }

    #[test]
    fn supplied_table_length_signature_and_checksum_are_checked() {
        for length in 0..size_of::<acpi_spec::Header>() {
            assert!(matches!(
                validate_parameter(&vec![0; length], *b"SLIT", 4096),
                Err(AcpiParameterError::TruncatedHeader)
            ));
        }

        let mut bytes = table(b"PPTT", &[]);
        assert!(validate_parameter(&bytes, *b"PPTT", 36).is_ok());
        assert!(matches!(
            validate_parameter(&bytes, *b"SLIT", 36),
            Err(AcpiParameterError::Signature { .. })
        ));
        assert!(matches!(
            validate_parameter(&bytes, *b"PPTT", 35),
            Err(AcpiParameterError::Length { .. })
        ));

        bytes.push(0);
        assert!(matches!(
            validate_parameter(&bytes, *b"PPTT", 4096),
            Err(AcpiParameterError::ChangedLength)
        ));
        bytes.pop();
        bytes[9] = bytes[9].wrapping_add(1);
        assert!(matches!(
            validate_parameter(&bytes, *b"PPTT", 4096),
            Err(AcpiParameterError::Checksum)
        ));
    }

    #[test]
    fn rejects_invalid_supplied_parameters() {
        let valid_slit = slit_bytes();
        let valid_pptt = table(b"PPTT", &[]);
        for signature in [b"SLIT", b"PPTT"] {
            let mut bad = if signature == b"SLIT" {
                valid_slit.clone()
            } else {
                valid_pptt.clone()
            };
            bad[9] = bad[9].wrapping_add(1);
            let result = if signature == b"SLIT" {
                runtime(&bad, &valid_pptt, false)
            } else {
                runtime(&valid_slit, &bad, false)
            };
            assert!(matches!(
                result.unwrap_err().downcast_ref::<AcpiParameterError>(),
                Some(AcpiParameterError::Checksum)
            ));
        }
        let mut bad = valid_slit;
        bad[44] = 9;
        checksum(&mut bad);
        assert!(runtime(&bad, &valid_pptt, false).is_err());
        assert!(runtime(&bad, &[], true).is_err());
    }
}
