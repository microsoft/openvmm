// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a loader that serializes the loaded state into the IGVM binary format.

use crate::identity_mapping::Measurement;
use crate::identity_mapping::SnpMeasurement;
use crate::identity_mapping::TdxMeasurement;
use crate::identity_mapping::VbsMeasurement;
use crate::signed_measurement::generate_snp_measurement;
use crate::signed_measurement::generate_tdx_measurement;
use crate::signed_measurement::generate_vbs_measurement;
use crate::vp_context_builder::VpContextBuilder;
use crate::vp_context_builder::VpContextPageState;
use crate::vp_context_builder::VpContextState;
use crate::vp_context_builder::snp::InjectionType;
use crate::vp_context_builder::snp::SnpHardwareContext;
use crate::vp_context_builder::tdx::TdxHardwareContext;
use crate::vp_context_builder::vbs::VbsRegister;
use crate::vp_context_builder::vbs::VbsVpContext;
use anyhow::Context;
use hvdef::Vtl;
use igvm::IgvmDirectiveHeader;
use igvm::IgvmFile;
use igvm::IgvmInitializationHeader;
use igvm::IgvmPlatformHeader;
use igvm::IgvmRelocatableRegion;
use igvm::IgvmRevision;
use igvm::snp_defs::SevVmsa;
use igvm_defs::IGVM_VHS_PARAMETER;
use igvm_defs::IGVM_VHS_PARAMETER_INSERT;
use igvm_defs::IGVM_VHS_SUPPORTED_PLATFORM;
use igvm_defs::IgvmPageDataFlags;
use igvm_defs::IgvmPageDataType;
use igvm_defs::IgvmPlatformType;
use igvm_defs::PAGE_SIZE_4K;
use igvm_defs::SnpPolicy;
use igvm_defs::TdxPolicy;
use loader::importer::Aarch64Register;
use loader::importer::BootPageAcceptance;
use loader::importer::GuestArch;
use loader::importer::GuestArchKind;
use loader::importer::IgvmParameterType;
use loader::importer::ImageLoad;
use loader::importer::IsolationConfig;
use loader::importer::IsolationType;
use loader::importer::ParameterAreaIndex;
use loader::importer::X86Register;
use memory_range::MemoryRange;
use range_map_vec::Entry;
use range_map_vec::RangeMap;
use sha2::Digest;
use sha2::Sha384;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fmt::Display;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

pub const DEFAULT_COMPATIBILITY_MASK: u32 = 0x1;

const TDX_SHARED_GPA_BOUNDARY_BITS: u8 = 47;

fn to_igvm_vtl(vtl: Vtl) -> igvm::hv_defs::Vtl {
    match vtl {
        Vtl::Vtl0 => igvm::hv_defs::Vtl::Vtl0,
        Vtl::Vtl1 => igvm::hv_defs::Vtl::Vtl1,
        Vtl::Vtl2 => igvm::hv_defs::Vtl::Vtl2,
    }
}

/// Page table relocation information kept for debugging purposes.
// Allow dead code because clippy doesn't count #[derive(Debug)] as non-dead code usage.
#[expect(dead_code)]
#[derive(Debug, Clone)]
struct PageTableRegion {
    gpa: u64,
    size_pages: u64,
    used_size_pages: u64,
}

#[derive(Debug, Clone)]
enum RelocationType {
    PageTable(PageTableRegion),
    Normal(IgvmRelocatableRegion),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RangeInfo {
    tag: String,
    acceptance: BootPageAcceptance,
}

pub struct IgvmLoader<R: VbsRegister + GuestArch> {
    accepted_ranges: RangeMap<u64, RangeInfo>,
    relocatable_regions: RangeMap<u64, RelocationType>,
    required_memory: Vec<RequiredMemory>,
    page_table_region: Option<PageTableRegion>,
    platform_header: IgvmPlatformHeader,
    initialization_headers: Vec<IgvmInitializationHeader>,
    directives: Vec<IgvmDirectiveHeader>,
    page_data_directives: Vec<IgvmDirectiveHeader>,
    vp_context: Option<Box<dyn VpContextBuilder<Register = R>>>,
    max_vtl: Vtl,
    parameter_areas: BTreeMap<(u64, u32), u32>,
    isolation_type: LoaderIsolationType,
    paravisor_present: bool,
    imported_regions_config_page: Option<u64>,
}

pub struct IgvmVtlLoader<'a, R: VbsRegister + GuestArch> {
    loader: &'a mut IgvmLoader<R>,
    vtl: Vtl,
    vp_context: Option<VbsVpContext<R>>,
}

impl<R: VbsRegister + GuestArch> IgvmVtlLoader<'_, R> {
    pub fn loader(&self) -> &IgvmLoader<R> {
        self.loader
    }

    /// Returns a loader for importing an inner image as part of the actual
    /// (paravisor) image to load.
    ///
    /// Use `take_vp_context` on the returned loader to get the VP context that
    /// the paravisor should load.
    pub fn nested_loader(&mut self) -> IgvmVtlLoader<'_, R> {
        IgvmVtlLoader {
            loader: &mut *self.loader,
            vtl: Vtl::Vtl0,
            vp_context: Some(VbsVpContext::new(self.vtl)),
        }
    }

    pub fn take_vp_context(&mut self) -> Vec<u8> {
        self.vp_context
            .take()
            .map_or_else(Vec::new, |vp| vp.as_page())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LoaderIsolationType {
    None,
    Vbs {
        enable_debug: bool,
    },
    Snp {
        shared_gpa_boundary_bits: Option<u8>,
        policy: SnpPolicy,
        injection_type: InjectionType,
        // TODO SNP: SNP Keys? Other data?
    },
    Tdx {
        policy: TdxPolicy,
    },
}

/// A trait to specialize behavior based on different register types for
/// different architectures.
pub trait IgvmLoaderRegister: VbsRegister {
    /// Perform arch specific initialization.
    fn init(
        with_paravisor: bool,
        max_vtl: Vtl,
        isolation: LoaderIsolationType,
    ) -> (
        IgvmPlatformHeader,
        Vec<IgvmInitializationHeader>,
        Box<dyn VpContextBuilder<Register = Self>>,
    );

    /// Generate a measurement based on isolation type.
    fn generate_measurement(
        isolation: LoaderIsolationType,
        initialization_headers: &[IgvmInitializationHeader],
        directive_headers: &[IgvmDirectiveHeader],
        svn: u32,
        debug_enabled: bool,
    ) -> anyhow::Result<Option<Measurement>>;

    /// The IGVM file revision to use for the built igvm file.
    fn igvm_revision() -> IgvmRevision;
}

impl IgvmLoaderRegister for X86Register {
    fn init(
        with_paravisor: bool,
        max_vtl: Vtl,
        isolation: LoaderIsolationType,
    ) -> (
        IgvmPlatformHeader,
        Vec<IgvmInitializationHeader>,
        Box<dyn VpContextBuilder<Register = Self>>,
    ) {
        match isolation {
            LoaderIsolationType::None | LoaderIsolationType::Vbs { .. } => {
                unreachable!("should be handled by common code")
            }
            LoaderIsolationType::Snp {
                shared_gpa_boundary_bits,
                policy,
                injection_type,
            } => {
                // TODO SNP: assumed that shared_gpa_boundary is always available.
                let shared_gpa_boundary =
                    1 << shared_gpa_boundary_bits.expect("shared gpa boundary must be set");

                // Add SNP Platform header
                let info = IGVM_VHS_SUPPORTED_PLATFORM {
                    compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                    highest_vtl: max_vtl as u8,
                    platform_type: IgvmPlatformType::SEV_SNP,
                    platform_version: igvm_defs::IGVM_SEV_SNP_PLATFORM_VERSION,
                    shared_gpa_boundary,
                };

                let platform_header = IgvmPlatformHeader::SupportedPlatform(info);

                let init_header = IgvmInitializationHeader::GuestPolicy {
                    policy: policy.into(),
                    compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                };

                let vp_context_builder = Box::new(SnpHardwareContext::new(
                    max_vtl,
                    !with_paravisor,
                    shared_gpa_boundary,
                    injection_type,
                ));

                (platform_header, vec![init_header], vp_context_builder)
            }
            LoaderIsolationType::Tdx { policy } => {
                // NOTE: TDX always has a shared_gpa_boundary and has it at 47 bits.
                let info = IGVM_VHS_SUPPORTED_PLATFORM {
                    compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                    highest_vtl: max_vtl as u8,
                    platform_type: IgvmPlatformType::TDX,
                    platform_version: igvm_defs::IGVM_TDX_PLATFORM_VERSION,
                    shared_gpa_boundary: 1 << TDX_SHARED_GPA_BOUNDARY_BITS,
                };

                let platform_header = IgvmPlatformHeader::SupportedPlatform(info);

                let mut init_headers = Vec::new();
                if u64::from(policy) != 0 {
                    init_headers.push(IgvmInitializationHeader::GuestPolicy {
                        policy: policy.into(),
                        compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                    });
                }

                let vp_context_builder = Box::new(TdxHardwareContext::new(!with_paravisor));

                (platform_header, init_headers, vp_context_builder)
            }
        }
    }

    fn generate_measurement(
        isolation: LoaderIsolationType,
        initialization_headers: &[IgvmInitializationHeader],
        directive_headers: &[IgvmDirectiveHeader],
        svn: u32,
        debug_enabled: bool,
    ) -> anyhow::Result<Option<Measurement>> {
        let measurement = match isolation {
            LoaderIsolationType::Snp { .. } => {
                let ld = generate_snp_measurement(initialization_headers, directive_headers, svn)
                    .context("generating snp measurement failed")?;
                Some(Measurement::Snp(SnpMeasurement::new(
                    ld,
                    svn,
                    debug_enabled,
                )))
            }
            LoaderIsolationType::Tdx { .. } => {
                let mrtd = generate_tdx_measurement(directive_headers)
                    .context("generating tdx measurement failed")?;
                Some(Measurement::Tdx(TdxMeasurement::new(
                    mrtd,
                    svn,
                    debug_enabled,
                )))
            }
            LoaderIsolationType::Vbs { enable_debug } => {
                let boot_digest = generate_vbs_measurement(directive_headers, enable_debug, svn)
                    .context("generating vbs measurement failed")?;
                Some(Measurement::Vbs(VbsMeasurement::new(
                    boot_digest,
                    svn,
                    debug_enabled,
                )))
            }
            _ => None,
        };
        Ok(measurement)
    }

    fn igvm_revision() -> IgvmRevision {
        // For now, x86 built files always uses V1 of the IGVM format. This is
        // to maintain compatibility with older OS repo loaders that do not
        // understand the V2 format.
        IgvmRevision::V1
    }
}

impl IgvmLoaderRegister for Aarch64Register {
    fn init(
        _with_paravisor: bool,
        _max_vtl: Vtl,
        _isolation: LoaderIsolationType,
    ) -> (
        IgvmPlatformHeader,
        Vec<IgvmInitializationHeader>,
        Box<dyn VpContextBuilder<Register = Self>>,
    ) {
        unreachable!("should never be called")
    }

    fn generate_measurement(
        _isolation: LoaderIsolationType,
        _initialization_headers: &[IgvmInitializationHeader],
        _directive_headers: &[IgvmDirectiveHeader],
        _svn: u32,
        _debug_enabled: bool,
    ) -> anyhow::Result<Option<Measurement>> {
        Ok(None)
    }

    fn igvm_revision() -> IgvmRevision {
        // AArch64 IGVM files are always V2.
        IgvmRevision::V2 {
            arch: igvm::Arch::AArch64,
            page_size: 4096,
        }
    }
}

#[derive(Debug, Clone)]
struct RequiredMemory {
    range: MemoryRange,
    vtl2_protectable: bool,
}

/// A map file representing information about a given generated IGVM file from a
/// loader.
///
/// This can be used to save additional information about the layout of the
/// address space that importing an IGVM file will create.
#[derive(Debug)]
pub struct MapFile {
    isolation: LoaderIsolationType,
    required_memory: Vec<RequiredMemory>,
    accepted_ranges: Vec<(MemoryRange, RangeInfo)>,
    relocatable_regions: Vec<(MemoryRange, RelocationType)>,
}

impl MapFile {
    /// Emit this map file information to tracing::info.
    pub fn emit_tracing(&self) {
        tracing::info!(isolation = ?self.isolation, "IGVM file isolation");
        tracing::info!("IGVM file layout:");
        for (range, info) in self.accepted_ranges.iter() {
            tracing::info!(
                tag = info.tag,
                size_bytes = range.len(),
                "{:#x} - {:#x}",
                range.start(),
                range.end(),
            );
        }

        if !self.required_memory.is_empty() {
            tracing::info!("IGVM file required memory:");
            for region in &self.required_memory {
                tracing::info!(
                    size_bytes = region.range.len(),
                    vtl2_protectable = region.vtl2_protectable,
                    "{:#x} - {:#x}",
                    region.range.start(),
                    region.range.end(),
                );
            }
        }

        if !self.relocatable_regions.is_empty() {
            tracing::info!("IGVM file relocatable regions:");
            for (range, info) in self.relocatable_regions.iter().rev() {
                match info {
                    RelocationType::PageTable(region) => {
                        tracing::info!(
                            size_bytes = region.size_pages * PAGE_SIZE_4K,
                            "{:#x} - {:#x} pagetable relocation region",
                            region.gpa,
                            range.end(),
                        );
                    }
                    RelocationType::Normal(region) => {
                        tracing::info!(
                            base_gpa = format_args!("{:#x}", region.base_gpa),
                            size_bytes = region.size,
                            minimum_relocation_gpa =
                                format_args!("{:#x}", region.minimum_relocation_gpa),
                            maximum_relocation_gpa =
                                format_args!("{:#x}", region.maximum_relocation_gpa),
                            relocation_alignment = region.relocation_alignment,
                            "{:#x} - {:#x} relocation region",
                            region.base_gpa,
                            range.end(),
                        );
                    }
                }
            }
        }
    }
}

impl Display for MapFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "IGVM file isolation: {:?}", self.isolation)?;

        writeln!(f, "IGVM file layout:")?;
        for (range, info) in &self.accepted_ranges {
            writeln!(
                f,
                "  {:016x} - {:016x} ({:#x} bytes) {}",
                range.start(),
                range.end(),
                range.len(),
                info.tag
            )?;
        }

        if !self.required_memory.is_empty() {
            writeln!(f, "IGVM file required memory:")?;
            for region in &self.required_memory {
                writeln!(
                    f,
                    "  {:016x} - {:016x} ({:#x} bytes) {}",
                    region.range.start(),
                    region.range.end(),
                    region.range.len(),
                    if region.vtl2_protectable {
                        "VTL2 protectable"
                    } else {
                        ""
                    }
                )?;
            }
        }

        if !self.relocatable_regions.is_empty() {
            writeln!(f, "IGVM file relocatable regions:")?;
            for (range, info) in &self.relocatable_regions {
                match info {
                    RelocationType::PageTable(region) => {
                        writeln!(
                            f,
                            "  {:016x} - {:016x} ({:#x} bytes) pagetable relocation region",
                            region.gpa,
                            range.end(),
                            region.size_pages * PAGE_SIZE_4K,
                        )?;
                    }
                    RelocationType::Normal(region) => {
                        writeln!(
                            f,
                            "  {:016x} - {:016x} ({:#x} bytes) relocation region",
                            region.base_gpa,
                            range.end(),
                            region.size
                        )?;
                    }
                }
            }
        }

        Ok(())
    }
}

/// Returns output from finalize
#[derive(Debug)]
pub struct IgvmOutput {
    pub guest: IgvmFile,
    pub map: MapFile,
    pub doc: Option<Measurement>,
}

impl<R: IgvmLoaderRegister + GuestArch + 'static> IgvmLoader<R> {
    pub fn new(with_paravisor: bool, isolation_type: LoaderIsolationType) -> Self {
        let vp_context_builder: Option<Box<dyn VpContextBuilder<Register = R>>>;
        let platform_header;
        let max_vtl = if with_paravisor { Vtl::Vtl2 } else { Vtl::Vtl0 };
        let initialization_headers;

        match isolation_type {
            LoaderIsolationType::None | LoaderIsolationType::Vbs { .. } => {
                vp_context_builder = Some(Box::new(VbsVpContext::<R>::new(max_vtl)));

                // Add VBS platform header
                let info = IGVM_VHS_SUPPORTED_PLATFORM {
                    compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                    highest_vtl: max_vtl as u8,
                    platform_type: IgvmPlatformType::VSM_ISOLATION,
                    platform_version: igvm_defs::IGVM_VSM_ISOLATION_PLATFORM_VERSION,
                    shared_gpa_boundary: 0,
                };

                platform_header = IgvmPlatformHeader::SupportedPlatform(info);
                initialization_headers = Vec::new();
            }
            _ => {
                let (header, init_headers, vp_builder) =
                    R::init(with_paravisor, max_vtl, isolation_type);
                platform_header = header;
                initialization_headers = init_headers;
                vp_context_builder = Some(vp_builder);
            }
        }

        IgvmLoader {
            accepted_ranges: RangeMap::new(),
            relocatable_regions: RangeMap::new(),
            required_memory: Vec::new(),
            page_table_region: None,
            platform_header,
            initialization_headers,
            directives: Vec::new(),
            page_data_directives: Vec::new(),
            vp_context: vp_context_builder,
            max_vtl,
            parameter_areas: BTreeMap::new(),
            isolation_type,
            paravisor_present: with_paravisor,
            imported_regions_config_page: None,
        }
    }

    fn generate_cryptographic_hash_of_shared_pages(&mut self) -> Vec<u8> {
        // Sort the page data directives by GPA to ensure the hash is consistent.
        self.page_data_directives
            .sort_unstable_by_key(|directive| match directive {
                IgvmDirectiveHeader::PageData { gpa, .. } => *gpa,
                _ => unreachable!("all directives should be IgvmDirectiveHeader::PageData"),
            });

        // Generate the hash of the unaccepted pages.
        let mut hasher = Sha384::new();
        self.page_data_directives.iter().for_each(|directive| {
            if let IgvmDirectiveHeader::PageData {
                gpa: _,
                compatibility_mask: _,
                flags,
                data_type,
                data,
            } = directive
            {
                if *data_type == IgvmPageDataType::NORMAL && flags.shared() {
                    // Measure the pages. If the data length is smaller than a page then zero extend
                    // the data to a full page.
                    let mut zero_data;
                    let data_to_hash = if data.len() < PAGE_SIZE_4K as usize {
                        zero_data = vec![0; PAGE_SIZE_4K as usize];
                        zero_data[..data.len()].copy_from_slice(data);
                        &zero_data
                    } else {
                        data
                    };

                    hasher.update(data_to_hash);
                }
            }
        });
        hasher.finalize().to_vec()
    }

    /// Finalize the loader state, returning an IGVM file.
    pub fn finalize(mut self, guest_svn: u32) -> anyhow::Result<IgvmOutput> {
        // Finalize any VP state.
        let mut state = Vec::new();
        self.vp_context.take().unwrap().finalize(&mut state);

        for context in state {
            match context {
                VpContextState::Page(VpContextPageState {
                    page_base,
                    page_count,
                    acceptance,
                    data,
                }) => {
                    self.import_pages(page_base, page_count, "vp-context-page", acceptance, &data)
                        .context("failed to import vp context page")?;
                }
                VpContextState::Directive(directive) => {
                    self.directives.push(directive);
                }
            }
        }

        // Put list of accepted pages into the config region, if there
        if let Some(page_base) = self.imported_regions_config_page {
            let mut imported_regions_data: Vec<_> = self.imported_regions();

            // Add this config page as well
            imported_regions_data.push(loader_defs::paravisor::ImportedRegionDescriptor::new(
                page_base, 1, true,
            ));

            // The accepted regions have been guaranteed to not overlap,
            // so just sort by the base page number
            imported_regions_data.sort_by_key(|region| region.base_page_number);

            // All shared pages have been imported. Generate the secure cryptographic hash of the unaccepted
            // imported pages.
            let hash = self.generate_cryptographic_hash_of_shared_pages();
            let page_header = loader_defs::paravisor::ImportedRegionsPageHeader {
                sha384_hash: hash
                    .as_bytes()
                    .try_into()
                    .expect("hash should be correct size"),
            };

            let mut imported_regions_page = page_header.as_bytes().to_vec();

            // Append the (sorted) imported region data.
            imported_regions_page.extend_from_slice(imported_regions_data.as_bytes());

            // This list should be measured
            self.import_pages(
                page_base,
                1,
                "loader-imported-regions",
                BootPageAcceptance::Exclusive,
                imported_regions_page.as_bytes(),
            )
            .context("failed to import config regions")?;
        }

        // Finalize parameter pages with insert directives.
        for ((page_base, _page_count), index) in self.parameter_areas.iter() {
            self.directives.push(IgvmDirectiveHeader::ParameterInsert(
                IGVM_VHS_PARAMETER_INSERT {
                    gpa: page_base * PAGE_SIZE_4K,
                    compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                    parameter_area_index: *index,
                },
            ));
        }

        // Merge the page_data_directives into the others directives. This must be done before
        // generating the launch measurement.
        self.directives.append(&mut self.page_data_directives);

        // Generate the launch measurement for the isolation type being used.
        // The measurement is output for external signing.
        let doc = R::generate_measurement(
            self.isolation_type,
            &self.initialization_headers,
            &self.directives,
            guest_svn,
            self.confidential_debug(),
        )?;

        // Display a report about the build igvm file's layout.
        let map_file = MapFile {
            isolation: self.isolation_type,
            required_memory: self.required_memory,
            accepted_ranges: self
                .accepted_ranges
                .iter()
                .rev()
                .map(|(range, info)| {
                    (
                        MemoryRange::from_4k_gpn_range(*range.start()..(range.end() + 1)),
                        info.clone(),
                    )
                })
                .collect(),
            relocatable_regions: self
                .relocatable_regions
                .iter()
                .rev()
                .map(|(range, info)| {
                    (
                        MemoryRange::new(*range.start()..(range.end() + 1)),
                        info.clone(),
                    )
                })
                .collect(),
        };

        map_file.emit_tracing();

        // Create an IGVM file with the loader's internal state.
        let igvm_file = IgvmFile::new(
            R::igvm_revision(),
            vec![self.platform_header],
            self.initialization_headers,
            self.directives,
        )
        .context("unable to create igvm file")?;

        let output = IgvmOutput {
            guest: igvm_file,
            map: map_file,
            doc,
        };
        Ok(output)
    }

    /// Accept a new page range with a given acceptance into the map of accepted
    /// ranges.
    fn accept_new_range(
        &mut self,
        page_base: u64,
        page_count: u64,
        tag: &str,
        acceptance: BootPageAcceptance,
    ) -> anyhow::Result<()> {
        let page_end = page_base + page_count - 1;
        match self.accepted_ranges.entry(page_base..=page_end) {
            Entry::Overlapping(entry) => {
                let (overlap_start, overlap_end, ref overlap_info) = *entry.get();
                Err(anyhow::anyhow!(
                    "{} at {} ({:?}) overlaps {} at {}",
                    tag,
                    MemoryRange::from_4k_gpn_range(page_base..page_end + 1),
                    acceptance,
                    overlap_info.tag,
                    MemoryRange::from_4k_gpn_range(overlap_start..overlap_end + 1),
                ))
            }
            Entry::Vacant(entry) => {
                entry.insert(RangeInfo {
                    tag: tag.to_string(),
                    acceptance,
                });
                Ok(())
            }
        }
    }

    fn imported_regions(&self) -> Vec<loader_defs::paravisor::ImportedRegionDescriptor> {
        let regions: Vec<_> = self
            .accepted_ranges
            .iter()
            .map(|(r, info)| {
                loader_defs::paravisor::ImportedRegionDescriptor::new(
                    *r.start(),
                    r.end() - r.start() + 1,
                    info.acceptance != BootPageAcceptance::Shared,
                )
            })
            .collect();
        regions
    }

    /// The guest architecture used by this loader.
    pub fn arch(&self) -> GuestArchKind {
        R::arch()
    }

    /// Returns true if this is an isolated guest with debug enabled, false
    /// otherwise.
    pub fn confidential_debug(&self) -> bool {
        match self.isolation_type {
            LoaderIsolationType::Vbs { enable_debug } => enable_debug,
            LoaderIsolationType::Snp { policy, .. } => policy.debug() == 1,
            LoaderIsolationType::Tdx { policy } => policy.debug_allowed() == 1,
            _ => false,
        }
    }

    pub fn loader(&mut self) -> IgvmVtlLoader<'_, R> {
        IgvmVtlLoader {
            vtl: self.max_vtl,
            loader: self,
            vp_context: None,
        }
    }

    fn import_pages(
        &mut self,
        page_base: u64,
        page_count: u64,
        debug_tag: &str,
        acceptance: BootPageAcceptance,
        mut data: &[u8],
    ) -> Result<(), anyhow::Error> {
        tracing::debug!(
            page_base,
            ?acceptance,
            page_count,
            data_size = data.len(),
            "Importing page",
        );

        // Pages must not overlap already accepted ranges
        self.accept_new_range(page_base, page_count, debug_tag, acceptance)?;

        // Page count must be larger or equal to data.
        if page_count * PAGE_SIZE_4K < data.len() as u64 {
            anyhow::bail!(
                "data len {:x} is larger than page_count {page_count:x}",
                data.len()
            );
        }

        // VpContext imports are handled differently, as they have a different IGVM header
        // type than normal data pages.
        if acceptance == BootPageAcceptance::VpContext {
            // This is only supported on SNP currently.
            match self.isolation_type {
                LoaderIsolationType::Snp { .. } => {}
                _ => {
                    anyhow::bail!("vpcontext acceptance only supported on SNP");
                }
            }

            // Data size must match SNP VMSA size.
            if data.len() != size_of::<SevVmsa>() {
                anyhow::bail!("data len {:x} does not match VMSA size", data.len());
            }

            // Page count must be 1.
            if page_count != 1 {
                anyhow::bail!("page count {page_count:x} for snp vmsa is not 1");
            }

            self.directives.push(IgvmDirectiveHeader::SnpVpContext {
                gpa: page_base * PAGE_SIZE_4K,
                compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                vp_index: 0,
                vmsa: Box::new(SevVmsa::read_from_bytes(data).expect("should be correct size")), // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            });
        } else {
            for page in page_base..page_base + page_count {
                let (data_type, flags) = match acceptance {
                    BootPageAcceptance::Exclusive => {
                        (IgvmPageDataType::NORMAL, IgvmPageDataFlags::new())
                    }
                    BootPageAcceptance::ExclusiveUnmeasured => (
                        IgvmPageDataType::NORMAL,
                        IgvmPageDataFlags::new().with_unmeasured(true),
                    ),
                    BootPageAcceptance::ErrorPage => todo!(),
                    BootPageAcceptance::SecretsPage => {
                        (IgvmPageDataType::SECRETS, IgvmPageDataFlags::new())
                    }
                    BootPageAcceptance::CpuidPage => {
                        (IgvmPageDataType::CPUID_DATA, IgvmPageDataFlags::new())
                    }
                    BootPageAcceptance::CpuidExtendedStatePage => {
                        (IgvmPageDataType::CPUID_XF, IgvmPageDataFlags::new())
                    }
                    BootPageAcceptance::VpContext => unreachable!(),
                    BootPageAcceptance::Shared => (
                        IgvmPageDataType::NORMAL,
                        IgvmPageDataFlags::new().with_shared(true),
                    ),
                };

                // Split data slice into data to be imported for this page and remaining.
                let import_data_len = std::cmp::min(PAGE_SIZE_4K as usize, data.len());
                let (import_data, data_remaining) = data.split_at(import_data_len);
                data = data_remaining;

                self.page_data_directives
                    .push(IgvmDirectiveHeader::PageData {
                        gpa: page * PAGE_SIZE_4K,
                        compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                        flags,
                        data_type,
                        data: import_data.to_vec(),
                    });
            }
        }

        Ok(())
    }
}

impl<R: IgvmLoaderRegister + GuestArch + 'static> ImageLoad<R> for IgvmVtlLoader<'_, R> {
    fn isolation_config(&self) -> IsolationConfig {
        match self.loader.isolation_type {
            LoaderIsolationType::None => IsolationConfig {
                paravisor_present: self.loader.paravisor_present,
                isolation_type: IsolationType::None,
                shared_gpa_boundary_bits: None,
            },
            LoaderIsolationType::Vbs { .. } => IsolationConfig {
                paravisor_present: self.loader.paravisor_present,
                isolation_type: IsolationType::Vbs,
                shared_gpa_boundary_bits: None,
            },
            LoaderIsolationType::Snp {
                shared_gpa_boundary_bits,
                policy: _,
                injection_type: _,
            } => IsolationConfig {
                paravisor_present: self.loader.paravisor_present,
                isolation_type: IsolationType::Snp,
                shared_gpa_boundary_bits,
            },
            LoaderIsolationType::Tdx { .. } => IsolationConfig {
                paravisor_present: self.loader.paravisor_present,
                isolation_type: IsolationType::Tdx,
                shared_gpa_boundary_bits: Some(TDX_SHARED_GPA_BOUNDARY_BITS),
            },
        }
    }

    fn create_parameter_area(
        &mut self,
        page_base: u64,
        page_count: u32,
        debug_tag: &str,
    ) -> anyhow::Result<ParameterAreaIndex> {
        self.create_parameter_area_with_data(page_base, page_count, debug_tag, &[])
    }

    fn create_parameter_area_with_data(
        &mut self,
        page_base: u64,
        page_count: u32,
        debug_tag: &str,
        initial_data: &[u8],
    ) -> anyhow::Result<ParameterAreaIndex> {
        let area_id = (page_base, page_count);

        // Allocate a new parameter area, that must not overlap other accepted ranges.
        self.loader.accept_new_range(
            page_base,
            page_count as u64,
            debug_tag,
            BootPageAcceptance::ExclusiveUnmeasured,
        )?;

        let index: u32 = self
            .loader
            .parameter_areas
            .len()
            .try_into()
            .expect("parameter area greater than u32");
        self.loader.parameter_areas.insert(area_id, index);

        // Add the newly allocated parameter area index to headers.
        self.loader
            .directives
            .push(IgvmDirectiveHeader::ParameterArea {
                number_of_bytes: page_count as u64 * PAGE_SIZE_4K,
                parameter_area_index: index,
                initial_data: initial_data.to_vec(),
            });

        tracing::debug!(
            index,
            page_base,
            page_count,
            initial_data_len = initial_data.len(),
            "Creating new parameter area",
        );

        Ok(ParameterAreaIndex(index))
    }

    fn import_parameter(
        &mut self,
        parameter_area: ParameterAreaIndex,
        byte_offset: u32,
        parameter_type: IgvmParameterType,
    ) -> anyhow::Result<()> {
        let index = parameter_area.0;

        if index >= self.loader.parameter_areas.len() as u32 {
            anyhow::bail!("invalid parameter area index: {:x}", index);
        }

        tracing::debug!(
            ?parameter_type,
            parameter_area_index = parameter_area.0,
            byte_offset,
            "Importing parameter",
        );

        let info = IGVM_VHS_PARAMETER {
            parameter_area_index: index,
            byte_offset,
        };

        let header = match parameter_type {
            IgvmParameterType::VpCount => IgvmDirectiveHeader::VpCount(info),
            IgvmParameterType::Srat => IgvmDirectiveHeader::Srat(info),
            IgvmParameterType::Madt => IgvmDirectiveHeader::Madt(info),
            IgvmParameterType::Slit => IgvmDirectiveHeader::Slit(info),
            IgvmParameterType::Pptt => IgvmDirectiveHeader::Pptt(info),
            IgvmParameterType::MmioRanges => IgvmDirectiveHeader::MmioRanges(info),
            IgvmParameterType::MemoryMap => IgvmDirectiveHeader::MemoryMap(info),
            IgvmParameterType::CommandLine => IgvmDirectiveHeader::CommandLine(info),
            IgvmParameterType::DeviceTree => IgvmDirectiveHeader::DeviceTree(info),
        };

        self.loader.directives.push(header);

        Ok(())
    }

    fn import_pages(
        &mut self,
        page_base: u64,
        page_count: u64,
        debug_tag: &str,
        acceptance: BootPageAcceptance,
        data: &[u8],
    ) -> anyhow::Result<()> {
        self.loader
            .import_pages(page_base, page_count, debug_tag, acceptance, data)
    }

    fn import_vp_register(&mut self, register: R) -> anyhow::Result<()> {
        if let Some(vp_context) = &mut self.vp_context {
            vp_context.import_vp_register(register)
        } else {
            self.loader
                .vp_context
                .as_mut()
                .unwrap()
                .import_vp_register(register);
        }

        Ok(())
    }

    fn verify_startup_memory_available(
        &mut self,
        page_base: u64,
        page_count: u64,
        memory_type: loader::importer::StartupMemoryType,
    ) -> anyhow::Result<()> {
        let gpa = page_base * PAGE_SIZE_4K;
        let compatibility_mask = DEFAULT_COMPATIBILITY_MASK;
        let number_of_bytes = (page_count * PAGE_SIZE_4K)
            .try_into()
            .expect("startup memory request overflowed u32");

        tracing::trace!(
            page_base,
            page_count,
            ?memory_type,
            number_of_bytes,
            "verify memory"
        );

        // Set VTL2 protectable flag on isolation types which make sense
        // TODO SNP: Temporarily allow this on all isolation types to force the host to generate
        // the correct device tree structures.
        let vtl2_protectable =
            memory_type == loader::importer::StartupMemoryType::Vtl2ProtectableRam;

        self.loader
            .directives
            .push(IgvmDirectiveHeader::RequiredMemory {
                gpa,
                compatibility_mask,
                number_of_bytes,
                vtl2_protectable,
            });

        self.loader.required_memory.push(RequiredMemory {
            range: MemoryRange::new(gpa..gpa + number_of_bytes as u64),
            vtl2_protectable,
        });

        Ok(())
    }

    fn set_vp_context_page(&mut self, page_base: u64) -> anyhow::Result<()> {
        self.loader
            .vp_context
            .as_mut()
            .unwrap()
            .set_vp_context_memory(page_base);

        Ok(())
    }

    fn relocation_region(
        &mut self,
        gpa: u64,
        size_bytes: u64,
        relocation_alignment: u64,
        minimum_relocation_gpa: u64,
        maximum_relocation_gpa: u64,
        apply_rip_offset: bool,
        apply_gdtr_offset: bool,
        vp_index: u16,
    ) -> anyhow::Result<()> {
        if let Some(overlap) = self
            .loader
            .relocatable_regions
            .get_range(gpa..=(gpa + size_bytes - 1))
        {
            anyhow::bail!(
                "new relocation region overlaps existing region {:?}",
                overlap
            );
        }

        if !size_bytes.is_multiple_of(PAGE_SIZE_4K) {
            anyhow::bail!("relocation size {size_bytes:#x} must be a multiple of 4K");
        }

        if !relocation_alignment.is_multiple_of(PAGE_SIZE_4K) {
            anyhow::bail!(
                "relocation alignment {relocation_alignment:#x} must be a multiple of 4K"
            );
        }

        if !gpa.is_multiple_of(relocation_alignment) {
            anyhow::bail!(
                "relocation base {gpa:#x} must be aligned to relocation alignment {relocation_alignment:#x}"
            );
        }

        if !minimum_relocation_gpa.is_multiple_of(relocation_alignment) {
            anyhow::bail!(
                "relocation minimum GPA {minimum_relocation_gpa:#x} must be aligned to relocation alignment {relocation_alignment:#x}"
            );
        }

        if !maximum_relocation_gpa.is_multiple_of(relocation_alignment) {
            anyhow::bail!(
                "relocation maximum GPA {maximum_relocation_gpa:#x} must be aligned to relocation alignment {relocation_alignment:#x}"
            );
        }

        self.loader
            .initialization_headers
            .push(IgvmInitializationHeader::RelocatableRegion {
                compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                relocation_alignment,
                relocation_region_gpa: gpa,
                relocation_region_size: size_bytes,
                minimum_relocation_gpa,
                maximum_relocation_gpa,
                is_vtl2: self.vtl == Vtl::Vtl2,
                apply_rip_offset,
                apply_gdtr_offset,
                vp_index,
                vtl: to_igvm_vtl(self.vtl),
            });

        self.loader.relocatable_regions.insert(
            gpa..=gpa + size_bytes - 1,
            RelocationType::Normal(IgvmRelocatableRegion {
                base_gpa: gpa,
                size: size_bytes,
                minimum_relocation_gpa,
                maximum_relocation_gpa,
                relocation_alignment,
                is_vtl2: self.vtl == Vtl::Vtl2,
                apply_rip_offset,
                apply_gdtr_offset,
                vp_index,
                vtl: to_igvm_vtl(self.vtl),
            }),
        );

        Ok(())
    }

    fn page_table_relocation(
        &mut self,
        page_table_gpa: u64,
        size_pages: u64,
        used_size_pages: u64,
        vp_index: u16,
    ) -> anyhow::Result<()> {
        // can only be one set
        if let Some(region) = &self.loader.page_table_region {
            anyhow::bail!("page table relocation already set {:?}", region)
        }

        if used_size_pages > size_pages {
            anyhow::bail!(
                "used size pages {used_size_pages:#x} cannot be greater than size pages {size_pages:#x}"
            );
        }

        let end_gpa = page_table_gpa + size_pages * PAGE_SIZE_4K - 1;

        // cannot override other relocatable regions
        if let Some(overlap) = self
            .loader
            .relocatable_regions
            .get_range(page_table_gpa..=end_gpa)
        {
            anyhow::bail!(
                "new page table relocation region overlaps existing region {:?}",
                overlap
            );
        }

        self.loader.initialization_headers.push(
            IgvmInitializationHeader::PageTableRelocationRegion {
                compatibility_mask: DEFAULT_COMPATIBILITY_MASK,
                gpa: page_table_gpa,
                size: size_pages * PAGE_SIZE_4K,
                used_size: used_size_pages * PAGE_SIZE_4K,
                vp_index,
                vtl: to_igvm_vtl(self.vtl),
            },
        );

        let region = PageTableRegion {
            gpa: page_table_gpa,
            size_pages,
            used_size_pages,
        };

        self.loader.relocatable_regions.insert(
            page_table_gpa..=end_gpa,
            RelocationType::PageTable(region.clone()),
        );

        self.loader.page_table_region = Some(region);

        Ok(())
    }

    fn set_imported_regions_config_page(&mut self, page_base: u64) {
        self.loader.imported_regions_config_page = Some(page_base);
    }
}

#[cfg(test)]
mod tests {
    use super::IgvmLoader;
    use super::*;
    use crate::identity_mapping::Measurement;
    use loader::importer::BootPageAcceptance;
    use loader::importer::ImageLoad;
    use loader_defs::paravisor::ImportedRegionDescriptor;

    #[test]
    fn test_snp_measurement() {
        use igvm_defs::SnpPolicy;
        let ref_ld: [u8; 48] = [
            136, 154, 25, 56, 108, 130, 226, 33, 155, 222, 211, 233, 42, 118, 78, 140, 0, 194, 155,
            150, 109, 4, 166, 98, 188, 166, 207, 223, 236, 100, 123, 144, 81, 153, 86, 83, 57, 7,
            131, 132, 101, 87, 145, 50, 99, 215, 28, 79,
        ];

        let mut loader = IgvmLoader::<X86Register>::new(
            true,
            LoaderIsolationType::Snp {
                shared_gpa_boundary_bits: Some(39),
                policy: SnpPolicy::from((0x1 << 17) | (0x1 << 16) | (0x1f)),
                injection_type: InjectionType::Restricted,
            },
        );
        let data = vec![0, 5];
        loader
            .import_pages(0, 5, "data", BootPageAcceptance::Exclusive, &data)
            .unwrap();
        loader
            .import_pages(5, 5, "data", BootPageAcceptance::ExclusiveUnmeasured, &data)
            .unwrap();
        loader
            .import_pages(10, 1, "data", BootPageAcceptance::Exclusive, &data)
            .unwrap();
        loader
            .import_pages(20, 1, "data", BootPageAcceptance::Shared, &data)
            .unwrap();

        let igvm_output = loader.finalize(1).unwrap();
        let doc = igvm_output.doc.expect("doc");
        let Measurement::Snp(snp_measurement) = doc else {
            panic!("known to be snp")
        };
        assert_eq!(ref_ld, snp_measurement.series[0].reference.snp_ld);
    }

    #[test]
    fn test_tdx_measurement() {
        let ref_mrtd: [u8; 48] = [
            214, 88, 54, 138, 48, 41, 223, 124, 152, 113, 236, 159, 157, 24, 134, 87, 36, 12, 163,
            162, 115, 128, 222, 247, 130, 13, 114, 103, 87, 67, 73, 89, 166, 251, 86, 245, 63, 209,
            246, 246, 164, 240, 96, 164, 22, 183, 142, 219,
        ];

        let mut loader = IgvmLoader::<X86Register>::new(
            true,
            LoaderIsolationType::Tdx {
                policy: TdxPolicy::new()
                    .with_debug_allowed(0u8)
                    .with_sept_ve_disable(0u8),
            },
        );
        let data = vec![0, 5];
        loader
            .import_pages(0, 5, "data", BootPageAcceptance::Exclusive, &data)
            .unwrap();
        loader
            .import_pages(5, 5, "data", BootPageAcceptance::ExclusiveUnmeasured, &data)
            .unwrap();
        loader
            .import_pages(10, 1, "data", BootPageAcceptance::Exclusive, &data)
            .unwrap();
        loader
            .import_pages(20, 1, "data", BootPageAcceptance::Shared, &data)
            .unwrap();

        let igvm_output = loader.finalize(1).unwrap();
        let doc = igvm_output.doc.expect("doc");
        let Measurement::Tdx(tdx_measurement) = doc else {
            panic!("known to be tdx")
        };
        assert_eq!(ref_mrtd, tdx_measurement.series[0].reference.tdx_mrtd);
    }

    #[test]
    fn test_vbs_digest() {
        let ref_digest: [u8; 32] = [
            0x30, 0x13, 0x4C, 0x9B, 0xB8, 0x9C, 0xD7, 0x2D, 0x8A, 0x41, 0x8D, 0x1E, 0x7A, 0xFB,
            0x75, 0x92, 0x7F, 0x45, 0xE8, 0x57, 0x1D, 0xDA, 0x7A, 0xC7, 0xBE, 0x87, 0xD4, 0xB6,
            0xC7, 0x2C, 0xA6, 0x4C,
        ];
        let mut loader = IgvmLoader::<X86Register>::new(
            true,
            LoaderIsolationType::Vbs {
                enable_debug: false,
            },
        );
        {
            let mut loader = loader.loader();

            let data = vec![0, 5];
            loader
                .import_pages(0, 5, "data", BootPageAcceptance::Exclusive, &data)
                .unwrap();
            loader
                .import_pages(5, 5, "data", BootPageAcceptance::ExclusiveUnmeasured, &data)
                .unwrap();
            loader
                .import_pages(10, 1, "data", BootPageAcceptance::Exclusive, &data)
                .unwrap();
            loader
                .import_pages(20, 1, "data", BootPageAcceptance::Shared, &data)
                .unwrap();
        }

        let igvm_output = loader.finalize(1).unwrap();
        let doc = igvm_output.doc.expect("doc");
        let Measurement::Vbs(vbs_measurement) = doc else {
            panic!("known to be vbs")
        };
        assert_eq!(
            ref_digest,
            vbs_measurement.series[0].reference.vbs_boot_digest
        );
    }

    #[test]
    fn test_accepted_regions() {
        let mut loader = IgvmLoader::<X86Register>::new(true, LoaderIsolationType::None);

        let data = vec![0, 5];
        loader
            .import_pages(0, 5, "test1", BootPageAcceptance::Exclusive, &data)
            .unwrap();

        loader
            .import_pages(15, 5, "test2", BootPageAcceptance::Exclusive, &data)
            .unwrap();

        loader
            .import_pages(10, 5, "test3", BootPageAcceptance::Exclusive, &data)
            .unwrap();

        assert_eq!(
            loader.imported_regions(),
            vec![
                ImportedRegionDescriptor::new(15, 5, true),
                ImportedRegionDescriptor::new(10, 5, true),
                ImportedRegionDescriptor::new(0, 5, true),
            ]
        );

        loader
            .import_pages(20, 10, "test1", BootPageAcceptance::Exclusive, &data)
            .unwrap();

        loader
            .import_pages(30, 1, "test2", BootPageAcceptance::Exclusive, &data)
            .unwrap();

        assert_eq!(
            loader.imported_regions(),
            vec![
                ImportedRegionDescriptor::new(30, 1, true),
                ImportedRegionDescriptor::new(20, 10, true),
                ImportedRegionDescriptor::new(15, 5, true),
                ImportedRegionDescriptor::new(10, 5, true),
                ImportedRegionDescriptor::new(0, 5, true),
            ]
        );
    }
}
