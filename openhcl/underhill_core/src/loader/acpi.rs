// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::Error;
use super::LoadKind;
use anyhow::Context;
use hvdef::HV_PAGE_SIZE;
use loader_defs::paravisor::PARAVISOR_CONFIG_SLIT_PAGE_INDEX;
use loader_defs::paravisor::PARAVISOR_CONFIG_SLIT_SIZE_PAGES;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vmm_core::acpi_builder::SlitInfo;
use zerocopy::FromBytes;

pub(super) const SLIT_MAX_SIZE: usize = (PARAVISOR_CONFIG_SLIT_SIZE_PAGES * HV_PAGE_SIZE) as usize;

#[derive(Debug)]
pub enum RuntimeSlit {
    Parsed(SlitInfo),
    TrustedTopologyPassthrough(Vec<u8>),
}

#[derive(Default, Debug, Clone, Copy)]
pub(super) struct GetAcpiSources {
    pub madt: bool,
    pub srat: bool,
    pub slit: bool,
    #[cfg(guest_arch = "aarch64")]
    pub pptt: bool,
}

impl GetAcpiSources {
    pub fn new(tables: &[Vec<u8>], load_kind: LoadKind, isolated: bool) -> Result<Self, Error> {
        let mut sources = Self::default();
        if isolated || load_kind != LoadKind::Uefi {
            return Ok(sources);
        }
        for table in tables {
            if !expose_get_table(table) {
                continue;
            }
            let (header, _) = acpi_spec::Header::read_from_prefix(table)
                .map_err(|_| Error::InvalidAcpiTableLength)?;
            let slot = match &header.signature {
                b"APIC" => {
                    sources.madt = true;
                    continue;
                }
                b"SRAT" => {
                    sources.srat = true;
                    continue;
                }
                b"SLIT" => &mut sources.slit,
                #[cfg(guest_arch = "aarch64")]
                b"PPTT" => &mut sources.pptt,
                _ => continue,
            };
            if *slot {
                return Err(Error::DuplicateAcpiTable(header.signature));
            }
            *slot = true;
        }
        Ok(sources)
    }

    fn passthrough_slit(self) -> bool {
        self.madt && self.srat
    }
}

pub(super) fn expose_get_table(table: &[u8]) -> bool {
    cfg!(guest_arch = "aarch64") || table.get(..4) != Some(b"PPTT")
}

pub(super) enum SlitSelection<'a> {
    None,
    Generate(&'a SlitInfo),
    Passthrough(&'a [u8]),
}

pub(super) fn select_slit<'a>(
    slit: Option<&'a RuntimeSlit>,
    sources: GetAcpiSources,
) -> Result<SlitSelection<'a>, Error> {
    let Some(slit) = slit else {
        return Ok(SlitSelection::None);
    };
    match slit {
        RuntimeSlit::Parsed(_) if sources.passthrough_slit() => {
            return Err(Error::InvalidSlitRepresentation);
        }
        RuntimeSlit::TrustedTopologyPassthrough(_) if !sources.passthrough_slit() => {
            return Err(Error::InvalidSlitRepresentation);
        }
        _ => {}
    }
    if sources.slit {
        return Ok(SlitSelection::None);
    }
    Ok(match slit {
        RuntimeSlit::Parsed(info) => SlitSelection::Generate(info),
        RuntimeSlit::TrustedTopologyPassthrough(bytes) => SlitSelection::Passthrough(bytes),
    })
}

pub(super) struct AcpiParameters {
    pub slit: Option<RuntimeSlit>,
    #[cfg(guest_arch = "aarch64")]
    pub pptt: Option<Vec<u8>>,
}

fn read_table(
    read: &mut impl FnMut(usize, &mut [u8]) -> anyhow::Result<()>,
    page_index: u64,
    max_size: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    let offset = usize::try_from(
        page_index
            .checked_mul(HV_PAGE_SIZE)
            .context("ACPI parameter offset overflow")?,
    )?;
    let mut header_bytes = [0; size_of::<acpi_spec::Header>()];
    read(offset, &mut header_bytes).context("reading ACPI parameter header")?;
    let header = acpi_spec::Header::read_from_bytes(&header_bytes)
        .map_err(|_| anyhow::anyhow!("copied ACPI header is truncated"))?;
    let length = usize::try_from(header.length.get())?;
    if length == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        (header_bytes.len()..=max_size).contains(&length),
        "ACPI parameter length {length} is outside {}..={max_size}",
        header_bytes.len()
    );
    offset
        .checked_add(length)
        .context("ACPI parameter end overflow")?;
    let mut table = vec![0; length];
    read(offset, &mut table).context("reading ACPI parameter body")?;
    let (copied_header, _) = acpi_spec::Header::read_from_prefix(&table)
        .map_err(|_| anyhow::anyhow!("copied ACPI header is truncated"))?;
    anyhow::ensure!(
        copied_header.length == header.length,
        "ACPI parameter length changed while reading"
    );
    Ok(Some(table))
}

pub(super) fn read_parameters(
    mut read: impl FnMut(usize, &mut [u8]) -> anyhow::Result<()>,
    sources: GetAcpiSources,
    isolated: bool,
) -> anyhow::Result<AcpiParameters> {
    let slit = read_table(&mut read, PARAVISOR_CONFIG_SLIT_PAGE_INDEX, SLIT_MAX_SIZE)
        .context("reading SLIT parameter")?
        .map(|table| {
            if sources.passthrough_slit() {
                anyhow::ensure!(!isolated, "isolated SLIT passthrough is not permitted");
                Ok(RuntimeSlit::TrustedTopologyPassthrough(table))
            } else {
                Ok(RuntimeSlit::Parsed(
                    SlitInfo::parse(&table, SLIT_MAX_SIZE).context("parsing SLIT parameter")?,
                ))
            }
        })
        .transpose()?;
    #[cfg(guest_arch = "aarch64")]
    let pptt = if isolated {
        // TODO: Validate and reconstruct PPTT before enabling it for CCA guests.
        None
    } else {
        read_table(
            &mut read,
            loader_defs::paravisor::PARAVISOR_CONFIG_PPTT_PAGE_INDEX,
            (loader_defs::paravisor::PARAVISOR_CONFIG_PPTT_SIZE_PAGES * HV_PAGE_SIZE) as usize,
        )
        .context("reading PPTT parameter")?
    };
    Ok(AcpiParameters {
        slit,
        #[cfg(guest_arch = "aarch64")]
        pptt,
    })
}

pub(super) fn validate_slit(
    slit: &SlitInfo,
    topology: &ProcessorTopology,
    memory: &MemoryLayout,
    check_domains: bool,
) -> Result<(), Error> {
    if check_domains {
        validate_slit_domains(
            slit,
            topology
                .vps()
                .map(|vp| vp.vnode)
                .chain(memory.ram().iter().map(|range| range.vnode)),
        )
    } else {
        validate_slit_domains(slit, [])
    }
}

fn validate_slit_domains(
    slit: &SlitInfo,
    domains: impl IntoIterator<Item = u32>,
) -> Result<(), Error> {
    let size = slit
        .num_nodes
        .checked_mul(slit.num_nodes)
        .and_then(|matrix| matrix.checked_add(44))
        .ok_or(Error::InvalidSlitSize)?;
    if slit.num_nodes == 0 || size > SLIT_MAX_SIZE {
        return Err(Error::InvalidSlitSize);
    }
    for domain in domains {
        if u64::from(domain) >= slit.num_nodes as u64 {
            return Err(Error::InvalidSlitDomain {
                domain,
                localities: slit.num_nodes,
            });
        }
    }
    Ok(())
}

pub(crate) fn select_load_kind(
    servicing: bool,
    forced: Option<&str>,
    pcat: bool,
) -> anyhow::Result<LoadKind> {
    if servicing {
        return Ok(LoadKind::None);
    }
    match forced {
        Some("linux") => Ok(LoadKind::Linux),
        Some("uefi") => Ok(LoadKind::Uefi),
        Some("pcat") => Ok(LoadKind::Pcat),
        Some(other) => anyhow::bail!("unexpected force load vtl0 type {other}"),
        None => Ok(if pcat { LoadKind::Pcat } else { LoadKind::Uefi }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn table(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut table = vec![0; 36];
        table[..4].copy_from_slice(signature);
        table[8] = 1;
        table.extend_from_slice(body);
        let length = table.len() as u32;
        table[4..8].copy_from_slice(&length.to_le_bytes());
        table[9] = 0u8.wrapping_sub(table.iter().fold(0u8, |s, &b| s.wrapping_add(b)));
        table
    }

    fn slit() -> Vec<u8> {
        let mut body = 2u64.to_le_bytes().to_vec();
        body.extend_from_slice(&[10, 17, 29, 10]);
        table(b"SLIT", &body)
    }

    fn sources(signatures: &[&[u8; 4]], kind: LoadKind, isolated: bool) -> GetAcpiSources {
        GetAcpiSources::new(
            &signatures.iter().map(|s| table(s, &[])).collect::<Vec<_>>(),
            kind,
            isolated,
        )
        .unwrap()
    }

    fn read(
        slit: &[u8],
        sources: GetAcpiSources,
        isolated: bool,
    ) -> anyhow::Result<(AcpiParameters, Vec<(usize, usize)>)> {
        let mut accesses = Vec::new();
        let parameters = read_parameters(
            |offset, bytes| {
                accesses.push((offset, bytes.len()));
                bytes.fill(0);
                if offset == 0 {
                    bytes.copy_from_slice(&slit[..bytes.len()]);
                }
                Ok(())
            },
            sources,
            isolated,
        )?;
        Ok((parameters, accesses))
    }

    #[test]
    fn ingestion_and_source_selection() {
        for (signatures, kind, isolated, passthrough, omitted) in [
            (vec![], LoadKind::Uefi, false, false, false),
            (vec![b"APIC"], LoadKind::Uefi, false, false, false),
            (vec![b"SRAT"], LoadKind::Uefi, false, false, false),
            (vec![b"APIC", b"SRAT"], LoadKind::Uefi, false, true, false),
            (
                vec![b"APIC", b"SRAT", b"SLIT"],
                LoadKind::Uefi,
                false,
                true,
                true,
            ),
            (vec![b"SLIT"], LoadKind::Uefi, false, false, true),
            (
                vec![b"APIC", b"SRAT", b"SLIT"],
                LoadKind::Uefi,
                true,
                false,
                false,
            ),
            (
                vec![b"APIC", b"SRAT", b"SLIT"],
                LoadKind::Linux,
                false,
                false,
                false,
            ),
        ] {
            let sources = sources(&signatures, kind, isolated);
            let (parameters, accesses) = read(&slit(), sources, isolated).unwrap();
            assert_eq!(
                matches!(
                    parameters.slit,
                    Some(RuntimeSlit::TrustedTopologyPassthrough(_))
                ),
                passthrough
            );
            assert_eq!(
                matches!(
                    select_slit(parameters.slit.as_ref(), sources).unwrap(),
                    SlitSelection::None
                ),
                omitted
            );
            if isolated || cfg!(guest_arch = "x86_64") {
                assert!(accesses.iter().all(|(offset, _)| *offset == 0));
            }
        }
    }

    #[test]
    fn raw_passthrough_is_not_a_parse_fallback() {
        let mut bytes = slit();
        bytes[8] = 99;
        bytes[9] = 0;
        let sources = sources(&[b"APIC", b"SRAT"], LoadKind::Uefi, false);
        let (parameters, _) = read(&bytes, sources, false).unwrap();
        match select_slit(parameters.slit.as_ref(), sources).unwrap() {
            SlitSelection::Passthrough(raw) => assert_eq!(raw, bytes),
            _ => panic!("expected raw passthrough"),
        }
        assert!(read(&bytes, GetAcpiSources::default(), false).is_err());
        assert!(matches!(
            select_slit(parameters.slit.as_ref(), GetAcpiSources::default()),
            Err(Error::InvalidSlitRepresentation)
        ));
        assert!(select_slit(None, sources).is_ok());
        assert!(read(&bytes, sources, true).is_err());
    }

    #[test]
    fn rejects_duplicate_overrides_and_ignores_unsupported_pptt() {
        assert!(matches!(
            GetAcpiSources::new(
                &[table(b"SLIT", &[]), table(b"SLIT", &[])],
                LoadKind::Uefi,
                false
            ),
            Err(Error::DuplicateAcpiTable(_))
        ));
        let tables = [table(b"PPTT", &[]), table(b"PPTT", &[])];
        if cfg!(guest_arch = "aarch64") {
            assert!(GetAcpiSources::new(&tables, LoadKind::Uefi, false).is_err());
        } else {
            assert!(GetAcpiSources::new(&tables, LoadKind::Uefi, false).is_ok());
            assert!(!expose_get_table(&tables[0]));
        }
        assert!(GetAcpiSources::new(&[vec![0]], LoadKind::Uefi, true).is_ok());
        assert!(GetAcpiSources::new(&[vec![0]], LoadKind::Linux, false).is_ok());
    }

    #[test]
    fn bounded_snapshot_reads() {
        for length in [1, 35, SLIT_MAX_SIZE + 1] {
            let mut bytes = table(b"SLIT", &[]);
            bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
            let mut reads = 0;
            assert!(
                read_table(
                    &mut |_, buf| {
                        reads += 1;
                        buf.copy_from_slice(&bytes);
                        Ok(())
                    },
                    0,
                    SLIT_MAX_SIZE
                )
                .is_err()
            );
            assert_eq!(reads, 1);
        }
        for length in [36, SLIT_MAX_SIZE] {
            let mut bytes = vec![0; length];
            bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
            assert_eq!(
                read_table(
                    &mut |_, buf| {
                        buf.copy_from_slice(&bytes[..buf.len()]);
                        Ok(())
                    },
                    0,
                    SLIT_MAX_SIZE
                )
                .unwrap()
                .unwrap(),
                bytes
            );
        }
        assert!(
            read(&[0; 36], GetAcpiSources::default(), true)
                .unwrap()
                .0
                .slit
                .is_none()
        );
        assert!(read_table(&mut |_, _| anyhow::bail!("read failure"), 0, SLIT_MAX_SIZE).is_err());
        let mut reads = 0;
        assert!(
            read_table(
                &mut |_, buf| {
                    reads += 1;
                    buf.fill(0);
                    buf[4..8]
                        .copy_from_slice(&(if reads == 1 { 36u32 } else { 37u32 }).to_le_bytes());
                    Ok(())
                },
                0,
                SLIT_MAX_SIZE
            )
            .is_err()
        );
        assert!(
            read_table(
                &mut |_, _| panic!("offset overflow must precede reads"),
                u64::MAX,
                SLIT_MAX_SIZE
            )
            .is_err()
        );
    }

    #[test]
    fn load_kind_precedence() {
        assert_eq!(
            select_load_kind(true, Some("invalid"), false).unwrap(),
            LoadKind::None
        );
        assert_eq!(
            select_load_kind(false, Some("linux"), true).unwrap(),
            LoadKind::Linux
        );
        assert_eq!(
            select_load_kind(false, Some("uefi"), true).unwrap(),
            LoadKind::Uefi
        );
        assert_eq!(
            select_load_kind(false, Some("pcat"), false).unwrap(),
            LoadKind::Pcat
        );
        assert_eq!(select_load_kind(false, None, true).unwrap(), LoadKind::Pcat);
        assert_eq!(
            select_load_kind(false, None, false).unwrap(),
            LoadKind::Uefi
        );
        assert!(select_load_kind(false, Some("invalid"), false).is_err());
    }

    #[test]
    fn local_domain_coverage_and_size() {
        let info = SlitInfo {
            num_nodes: 4,
            distances: vec![],
        };
        assert!(validate_slit_domains(&info, [0, 3]).is_ok());
        assert!(matches!(
            validate_slit_domains(&info, [4]),
            Err(Error::InvalidSlitDomain { domain: 4, .. })
        ));
        assert!(matches!(
            validate_slit_domains(&info, [u32::MAX]),
            Err(Error::InvalidSlitDomain { .. })
        ));
        for num_nodes in [0, 287, usize::MAX] {
            assert!(matches!(
                validate_slit_domains(
                    &SlitInfo {
                        num_nodes,
                        distances: vec![]
                    },
                    []
                ),
                Err(Error::InvalidSlitSize)
            ));
        }
    }
}
