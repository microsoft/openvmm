// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parameters that are fixed at IGVM build time by the underhill loader.

use crate::arch::get_isolation_type;
use core::slice;
use loader_defs::paravisor::ImportedRegionDescriptor;
use loader_defs::paravisor::ParavisorCommandLine;
use loader_defs::shim::ShimParamsRaw;
use memory_range::MemoryRange;

/// Isolation type of the partition
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum IsolationType {
    None,
    Vbs,
    #[cfg_attr(target_arch = "aarch64", expect(dead_code))]
    Snp,
    #[cfg_attr(target_arch = "aarch64", expect(dead_code))]
    Tdx,
}

impl IsolationType {
    pub fn is_hardware_isolated(&self) -> bool {
        match self {
            IsolationType::None => false,
            IsolationType::Vbs => false,
            IsolationType::Snp => true,
            IsolationType::Tdx => true,
        }
    }
}

/// Iterator for the list of accepted regions in the IGVM VTL 2 config region.
/// Does not increment past the first with page count 0.
pub struct ImportedRegionIter<'a> {
    imported_regions: &'a [ImportedRegionDescriptor],
}

impl Iterator for ImportedRegionIter<'_> {
    type Item = (MemoryRange, bool);

    fn next(&mut self) -> Option<Self::Item> {
        if self.imported_regions.is_empty() {
            None
        } else {
            let element = self.imported_regions[0]
                .pages()
                .map(|(base_page, count, accepted)| {
                    let base_address = base_page * hvdef::HV_PAGE_SIZE;
                    let end_address = base_address + (count * hvdef::HV_PAGE_SIZE);
                    (MemoryRange::try_new(base_address..end_address).expect(
                    "page number conversion into addresses results in a valid address range",
                ), accepted)
                });

            if element.is_some() {
                self.imported_regions = &self.imported_regions[1..];
            } else {
                self.imported_regions = &[];
            }

            element
        }
    }
}

/// Parameters fixed at IGVM file build time. These contain information about
/// where certain sections are located, that are fixed up after figuring out
/// where the boot loader was relocated to.
#[derive(Debug)]
pub struct ShimParams {
    // TODO: replace all of these base/size pairs with MemoryRange
    /// The kernel entry address.
    pub kernel_entry_address: u64,
    /// The address of the [`ParavisorCommandLine`] structure.
    pub cmdline_base: u64,
    /// The initrd address.
    pub initrd_base: u64,
    /// The size of the inird, in bytes.
    pub initrd_size: u64,
    /// The crc32 of the initrd at file build time.
    pub initrd_crc: u32,
    /// The base address of the VTL2 memory region encoded at build time.
    pub memory_start_address: u64,
    /// The size of the VTL2 memory region encoded at build time.
    pub memory_size: u64,
    /// The base address of the parameter region.
    pub parameter_region_start: u64,
    /// The size of the parameter region.
    pub parameter_region_size: u64,
    /// The base address of the VTL2 reserved region.
    pub vtl2_reserved_region_start: u64,
    /// The size of the VTL2 reserved region.
    pub vtl2_reserved_region_size: u64,
    /// Isolation type supported by the boot shim.
    pub isolation_type: IsolationType,
    pub sidecar_entry_address: u64,
    pub sidecar_base: u64,
    pub sidecar_size: u64,
    /// Memory used by the shim.
    pub used: MemoryRange,
    pub bounce_buffer: Option<MemoryRange>,
    /// Log buffer region used by the shim.
    pub log_buffer: MemoryRange,
    /// Memory to be used for the heap.
    pub heap: MemoryRange,
    /// Memory region for persisted state.
    pub persisted_state: MemoryRange,
}

impl ShimParams {
    /// Create a new instance of [`ShimParams`] from the raw offset based
    /// [`ShimParamsRaw`] and shim base address.
    pub fn new(shim_base_address: u64, raw: &ShimParamsRaw) -> Self {
        let &ShimParamsRaw {
            kernel_entry_offset,
            cmdline_offset,
            initrd_offset,
            initrd_size,
            initrd_crc,
            supported_isolation_type,
            memory_start_offset,
            memory_size,
            parameter_region_offset,
            parameter_region_size,
            vtl2_reserved_region_offset,
            vtl2_reserved_region_size,
            sidecar_offset,
            sidecar_size,
            sidecar_entry_offset,
            used_start,
            used_end,
            bounce_buffer_start,
            bounce_buffer_size,
            log_buffer_start,
            log_buffer_size,
            heap_start_offset,
            heap_size,
            persisted_state_region_offset,
            persisted_state_region_size,
        } = raw;

        let isolation_type = get_isolation_type(supported_isolation_type);

        let bounce_buffer = if bounce_buffer_size == 0 {
            None
        } else {
            let base = shim_base_address.wrapping_add_signed(bounce_buffer_start);
            Some(MemoryRange::new(base..base + bounce_buffer_size))
        };

        let log_buffer = {
            let base = shim_base_address.wrapping_add_signed(log_buffer_start);
            MemoryRange::new(base..base + log_buffer_size)
        };

        let heap = {
            let base = shim_base_address.wrapping_add_signed(heap_start_offset);
            MemoryRange::new(base..base + heap_size)
        };

        let persisted_state = {
            let base = shim_base_address.wrapping_add_signed(persisted_state_region_offset);
            MemoryRange::new(base..base + persisted_state_region_size)
        };

        Self {
            kernel_entry_address: shim_base_address.wrapping_add_signed(kernel_entry_offset),
            cmdline_base: shim_base_address.wrapping_add_signed(cmdline_offset),
            initrd_base: shim_base_address.wrapping_add_signed(initrd_offset),
            initrd_size,
            initrd_crc,
            memory_start_address: shim_base_address.wrapping_add_signed(memory_start_offset),
            memory_size,
            parameter_region_start: shim_base_address.wrapping_add_signed(parameter_region_offset),
            parameter_region_size,
            vtl2_reserved_region_start: shim_base_address
                .wrapping_add_signed(vtl2_reserved_region_offset),
            vtl2_reserved_region_size,
            isolation_type,
            sidecar_entry_address: shim_base_address.wrapping_add_signed(sidecar_entry_offset),
            sidecar_base: shim_base_address.wrapping_add_signed(sidecar_offset),
            sidecar_size,
            used: MemoryRange::new(
                shim_base_address.wrapping_add_signed(used_start)
                    ..shim_base_address.wrapping_add_signed(used_end),
            ),
            bounce_buffer,
            log_buffer,
            heap,
            persisted_state,
        }
    }

    /// Get the base address of the secrets page.
    #[cfg(target_arch = "x86_64")]
    pub fn secrets_start(&self) -> u64 {
        self.vtl2_reserved_region_start
            + loader_defs::paravisor::PARAVISOR_RESERVED_VTL2_SNP_SECRETS_PAGE_INDEX
                * hvdef::HV_PAGE_SIZE
    }

    /// Get the size of the CPUID page.
    #[cfg(target_arch = "x86_64")]
    pub fn cpuid_start(&self) -> u64 {
        self.vtl2_reserved_region_start
            + loader_defs::paravisor::PARAVISOR_RESERVED_VTL2_SNP_CPUID_PAGE_INDEX
                * hvdef::HV_PAGE_SIZE
    }

    /// Get the base address of the host provided device tree.
    pub fn dt_start(&self) -> u64 {
        self.parameter_region_start
            + loader_defs::paravisor::PARAVISOR_CONFIG_DEVICE_TREE_PAGE_INDEX * hvdef::HV_PAGE_SIZE
    }

    /// The size of the device tree region.
    pub fn dt_size(&self) -> u64 {
        loader_defs::paravisor::PARAVISOR_CONFIG_DEVICE_TREE_SIZE_PAGES * hvdef::HV_PAGE_SIZE
    }

    /// Get the initrd as a byte slice.
    pub fn initrd(&self) -> &'static [u8] {
        // SAFETY: The initrd base and size are set at file build time, and the
        // host must relocate the whole region if relocations are performed.
        unsafe { slice::from_raw_parts(self.initrd_base as *const u8, self.initrd_size as usize) }
    }

    /// Get the [`ParavisorCommandLine`] structure that describes the command
    /// line information.
    pub fn command_line(&self) -> &'static ParavisorCommandLine {
        // SAFETY: cmdline_base is a valid address pointing to a valid instance
        // of a ParavisorCommandLine struct.
        unsafe {
            (self.cmdline_base as *const ParavisorCommandLine)
                .as_ref()
                .expect("should always be non null")
        }
    }

    /// Get the device tree parameter region as a byte slice. Note that the byte
    /// contents of this slice are written by the host which is untrusted and
    /// must be validated before usage.
    pub fn device_tree(&self) -> &'static [u8] {
        // SAFETY: dt_start() and dt_size() are a valid address, size pair being
        // generated at IGVM file build time.
        unsafe { slice::from_raw_parts(self.dt_start() as *const u8, self.dt_size() as usize) }
    }

    /// Get the list of accepted regions from the parameter region as a
    /// ImportedRegionDescriptor slice. Note that this list is provided by the IGVM
    /// file and measured.
    pub fn imported_regions(&self) -> ImportedRegionIter<'_> {
        use loader_defs::paravisor::ImportedRegionsPageHeader;

        let imported_region_page_address = self.parameter_region_start
            + (loader_defs::paravisor::PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX
                * hvdef::HV_PAGE_SIZE);

        assert!(
            imported_region_page_address + hvdef::HV_PAGE_SIZE
                <= self.parameter_region_start + self.parameter_region_size
        );

        let imported_region_start =
            imported_region_page_address + size_of::<ImportedRegionsPageHeader>() as u64;

        // SAFETY: accepted_region_start and HV_PAGE_SIZE are a valid address, size pair being
        // generated at IGVM file build time and validated to be within the parameter region.
        unsafe {
            ImportedRegionIter {
                imported_regions: slice::from_raw_parts(
                    imported_region_start as *const ImportedRegionDescriptor,
                    (hvdef::HV_PAGE_SIZE as usize - size_of::<ImportedRegionsPageHeader>())
                        / size_of::<ImportedRegionDescriptor>(),
                ),
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn imported_regions_hash(&self) -> &'static [u8] {
        let header_start = self.parameter_region_start
            + (loader_defs::paravisor::PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX
                * hvdef::HV_PAGE_SIZE);

        // SAFETY: header_start is a valid address pointing to a valid instance
        // of an imported region page header.
        unsafe {
            let header =
                &*(header_start as *const loader_defs::paravisor::ImportedRegionsPageHeader);
            &header.sha384_hash
        }
    }
}
