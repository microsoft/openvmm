// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Underhill (paravisor) definitions.

use bitfield_struct::bitfield;
use core::mem::size_of;
use hvdef::HV_PAGE_SIZE;
#[cfg(feature = "inspect")]
use inspect::Inspect;
use open_enum::open_enum;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// Number of pages for each type of parameter in the vtl 2 unmeasured config
// region.
/// Size in pages for the SLIT.
pub const PARAVISOR_CONFIG_SLIT_SIZE_PAGES: u64 = 20;
/// Size in pages for the PPTT.
pub const PARAVISOR_CONFIG_PPTT_SIZE_PAGES: u64 = 20;
/// Size in pages for the device tree.
pub const PARAVISOR_CONFIG_DEVICE_TREE_SIZE_PAGES: u64 = 64;

/// The maximum size in pages of the unmeasured vtl 2 config region.
pub const PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_PAGE_COUNT_MAX: u64 =
    PARAVISOR_CONFIG_SLIT_SIZE_PAGES
        + PARAVISOR_CONFIG_PPTT_SIZE_PAGES
        + PARAVISOR_CONFIG_DEVICE_TREE_SIZE_PAGES;

// Page indices for different parameters within the unmeasured vtl 2 config region.
/// The page index to the SLIT.
pub const PARAVISOR_CONFIG_SLIT_PAGE_INDEX: u64 = 0;
/// The page index to the PPTT.
pub const PARAVISOR_CONFIG_PPTT_PAGE_INDEX: u64 =
    PARAVISOR_CONFIG_SLIT_PAGE_INDEX + PARAVISOR_CONFIG_SLIT_SIZE_PAGES;
/// The page index to the device tree.
pub const PARAVISOR_CONFIG_DEVICE_TREE_PAGE_INDEX: u64 =
    PARAVISOR_CONFIG_PPTT_PAGE_INDEX + PARAVISOR_CONFIG_PPTT_SIZE_PAGES;
/// Base index for the unmeasured vtl 2 config region
pub const PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_BASE_INDEX: u64 =
    PARAVISOR_CONFIG_SLIT_PAGE_INDEX;

/// Size in pages for the SNP CPUID pages.
pub const PARAVISOR_RESERVED_VTL2_SNP_CPUID_SIZE_PAGES: u64 = 2;
/// Size in pages for the VMSA page.
pub const PARAVISOR_RESERVED_VTL2_SNP_VMSA_SIZE_PAGES: u64 = 1;
/// Size in pages for the secrets page.
pub const PARAVISOR_RESERVED_VTL2_SNP_SECRETS_SIZE_PAGES: u64 = 1;

/// Total size of the reserved vtl2 range.
pub const PARAVISOR_RESERVED_VTL2_PAGE_COUNT_MAX: u64 = PARAVISOR_RESERVED_VTL2_SNP_CPUID_SIZE_PAGES
    + PARAVISOR_RESERVED_VTL2_SNP_VMSA_SIZE_PAGES
    + PARAVISOR_RESERVED_VTL2_SNP_SECRETS_SIZE_PAGES;

// Page indices for reserved vtl2 ranges, ranges that are marked as reserved to
// both the kernel and usermode. Today, these are SNP specific pages.
//
// TODO SNP: Does the kernel require that the CPUID and secrets pages are
// persisted, or after the kernel boots, and usermode reads them, can we discard
// them?
//
/// The page index to the SNP VMSA page.
pub const PARAVISOR_RESERVED_VTL2_SNP_VMSA_PAGE_INDEX: u64 = 0;
/// The page index to the first SNP CPUID page.
pub const PARAVISOR_RESERVED_VTL2_SNP_CPUID_PAGE_INDEX: u64 =
    PARAVISOR_RESERVED_VTL2_SNP_VMSA_PAGE_INDEX + PARAVISOR_RESERVED_VTL2_SNP_VMSA_SIZE_PAGES;
/// The page index to the first SNP secrets page.
pub const PARAVISOR_RESERVED_VTL2_SNP_SECRETS_PAGE_INDEX: u64 =
    PARAVISOR_RESERVED_VTL2_SNP_CPUID_PAGE_INDEX + PARAVISOR_RESERVED_VTL2_SNP_CPUID_SIZE_PAGES;

// Number of pages for each type of parameter in the vtl 2 measured config
// region.
/// Size in pages the list of accepted memory
pub const PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_SIZE_PAGES: u64 = 1;
/// Size in pages of VTL2 specific measured config
pub const PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES: u64 = 1;

/// Count for vtl 2 measured config region size.
pub const PARAVISOR_MEASURED_VTL2_CONFIG_REGION_PAGE_COUNT: u64 =
    PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_SIZE_PAGES
        + PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES;

// Measured config comes after the unmeasured config
/// The page index to the list of accepted pages
pub const PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX: u64 =
    PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_BASE_INDEX
        + PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_PAGE_COUNT_MAX;

/// The page index for measured VTL2 config.
pub const PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX: u64 =
    PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX
        + PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_SIZE_PAGES;

/// The maximum size in pages out of all isolation architectures.
pub const PARAVISOR_VTL2_CONFIG_REGION_PAGE_COUNT_MAX: u64 =
    PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_PAGE_COUNT_MAX
        + PARAVISOR_MEASURED_VTL2_CONFIG_REGION_PAGE_COUNT; // TODO: const fn max or macro possible?

// Default memory information.
/// The default base address for the paravisor, 128MB.
pub const PARAVISOR_DEFAULT_MEMORY_BASE_ADDRESS: u64 = 128 * 1024 * 1024;
/// The default page count for the memory size for the paravisor, 64MB.
pub const PARAVISOR_DEFAULT_MEMORY_PAGE_COUNT: u64 = 64 * 1024 * 1024 / HV_PAGE_SIZE;
/// The base VA for the local map, if present.
pub const PARAVISOR_LOCAL_MAP_VA: u64 = 0x200000;
/// The base size in bytes for the local map, if present.
pub const PARAVISOR_LOCAL_MAP_SIZE: u64 = 0x200000;

open_enum! {
    /// Underhill command line policy.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum CommandLinePolicy : u16 {
        /// Use the static command line encoded only.
        STATIC = 0,
        /// Append the host provided value in the device tree /chosen node to
        /// the static command line.
        APPEND_CHOSEN = 1,
    }
}

/// Maximum static command line size.
pub const COMMAND_LINE_SIZE: usize = 4092;

/// Command line information. This structure is an exclusive measured page.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ParavisorCommandLine {
    /// The policy Underhill should use.
    pub policy: CommandLinePolicy,
    /// The length of the command line.
    pub static_command_line_len: u16,
    /// The static command line. This is a valid utf8 string of length described
    /// by the field above. This field should normally not be used, instead the
    /// corresponding [`Self::command_line`] function should be used that
    /// returns a [`&str`].
    pub static_command_line: [u8; COMMAND_LINE_SIZE],
}

impl ParavisorCommandLine {
    /// Read the static command line as a [`&str`]. Returns None if the bytes
    /// are not a valid [`&str`].
    pub fn command_line(&self) -> Option<&str> {
        core::str::from_utf8(&self.static_command_line[..self.static_command_line_len as usize])
            .ok()
    }
}

const_assert_eq!(size_of::<ParavisorCommandLine>(), HV_PAGE_SIZE as usize);

/// Describes a region of guest memory.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct PageRegionDescriptor {
    /// Guest physical page number for the base of this region.
    pub base_page_number: u64,
    /// Number of pages in this region. 0 means this region is not valid.
    pub page_count: u64,
}

#[cfg(feature = "inspect")]
impl Inspect for PageRegionDescriptor {
    fn inspect(&self, req: inspect::Request<'_>) {
        let pages = self.pages();

        match pages {
            None => {
                req.ignore();
            }
            Some((base, count)) => {
                req.respond()
                    .field("base_page_number", base)
                    .field("page_count", count);
            }
        }
    }
}

impl PageRegionDescriptor {
    /// An empty region.
    pub const EMPTY: Self = PageRegionDescriptor {
        base_page_number: 0,
        page_count: 0,
    };

    /// Create a new page region descriptor with the given base page and page count.
    pub fn new(base_page_number: u64, page_count: u64) -> Self {
        PageRegionDescriptor {
            base_page_number,
            page_count,
        }
    }

    /// Returns `Some((base page number, page count))` described by the descriptor, if valid.
    pub fn pages(&self) -> Option<(u64, u64)> {
        if self.page_count != 0 {
            Some((self.base_page_number, self.page_count))
        } else {
            None
        }
    }
}

/// The header field of the imported pages region page.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct ImportedRegionsPageHeader {
    /// The cryptographic hash of the unaccepted pages.
    pub sha384_hash: [u8; 48],
}

/// Describes a region of guest memory that has been imported into VTL2.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes, PartialEq)]
pub struct ImportedRegionDescriptor {
    /// Guest physical page number for the base of this region.
    pub base_page_number: u64,
    /// Number of pages in this region. 0 means this region is not valid.
    pub page_count: u64,
    /// Whether the pages in this region were accepted during the import process.
    pub accepted: u8,
    /// Padding
    padding: [u8; 7],
}

#[cfg(feature = "inspect")]
impl Inspect for ImportedRegionDescriptor {
    fn inspect(&self, req: inspect::Request<'_>) {
        let pages = self.pages();

        match pages {
            None => {
                req.ignore();
            }
            Some((base, count, accepted)) => {
                req.respond()
                    .field("base_page_number", base)
                    .field("page_count", count)
                    .field("accepted", accepted);
            }
        }
    }
}

impl ImportedRegionDescriptor {
    /// An empty region.
    pub const EMPTY: Self = ImportedRegionDescriptor {
        base_page_number: 0,
        page_count: 0,
        accepted: false as u8,
        padding: [0; 7],
    };

    /// Create a new page region descriptor with the given base page and page count.
    pub fn new(base_page_number: u64, page_count: u64, accepted: bool) -> Self {
        ImportedRegionDescriptor {
            base_page_number,
            page_count,
            accepted: accepted as u8,
            padding: [0; 7],
        }
    }

    /// Returns `Some((base page number, page count, accepted))` described by the descriptor, if valid.
    pub fn pages(&self) -> Option<(u64, u64, bool)> {
        if self.page_count != 0 {
            Some((self.base_page_number, self.page_count, self.accepted != 0))
        } else {
            None
        }
    }
}

/// Measured config about linux loaded into VTL0.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct LinuxInfo {
    /// The memory the kernel was loaded into.
    pub kernel_region: PageRegionDescriptor,
    /// The gpa entrypoint of the kernel.
    pub kernel_entrypoint: u64,
    /// The memory region the initrd was loaded into.
    pub initrd_region: PageRegionDescriptor,
    /// The size of the initrd in bytes.
    pub initrd_size: u64,
    /// An ASCII command line to use for the kernel.
    pub command_line: PageRegionDescriptor,
}

/// Measured config about UEFI loaded into VTL0.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct UefiInfo {
    /// The information about where UEFI's firmware and misc pages are.
    pub firmware: PageRegionDescriptor,
    /// The location of VTL0's VP context data.
    pub vtl0_vp_context: PageRegionDescriptor,
}

/// Measured config about what this image can support loading in VTL0.
#[cfg_attr(feature = "inspect", derive(Inspect))]
#[bitfield(u64)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SupportedVtl0LoadInfo {
    /// This image supports UEFI.
    #[bits(1)]
    pub uefi_supported: bool,
    /// This image supports PCAT.
    #[bits(1)]
    pub pcat_supported: bool,
    /// This image supports Linux Direct.
    #[bits(1)]
    pub linux_direct_supported: bool,
    /// Currently reserved.
    #[bits(61)]
    pub reserved: u64,
}

/// Paravisor measured config information for vtl 0. Unlike the previous loader
/// block which contains dynamic parameter info written by the host, this config
/// information is known at file build time, measured, and deposited as part of
/// the initial launch data.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct ParavisorMeasuredVtl0Config {
    /// Magic value. Must be [`Self::MAGIC`].
    pub magic: u64,
    /// Supported VTL0 images.
    pub supported_vtl0: SupportedVtl0LoadInfo,
    /// If UEFI is supported, information about UEFI for VTL0.
    pub uefi_info: UefiInfo,
    /// If Linux is supported, information about Linux for VTL0.
    pub linux_info: LinuxInfo,
}

impl ParavisorMeasuredVtl0Config {
    /// Magic value for the measured config, which is "OHCLVTL0".
    pub const MAGIC: u64 = 0x4F48434C56544C30;
}

/// The physical page number for where the vtl 0 measured config is stored, x86_64.
/// This address is guaranteed to exist in the guest address space as it is
/// where the ISR table is located at reset.
pub const PARAVISOR_VTL0_MEASURED_CONFIG_BASE_PAGE_X64: u64 = 0;

/// The physical page number for where the vtl 0 measured config is stored, aarch64.
/// Not obvious about guaranteed existence. 16MiB might be a reasonable assumption as:
/// * UEFI uses the GPA range of [0; 0x800000), after that there are page tables,
///   stack, and the config blob at GPA 0x824000,
/// * Gen 2 VMs don't work with less than 32MiB,
/// * the loaders have checks for overlap.
pub const PARAVISOR_VTL0_MEASURED_CONFIG_BASE_PAGE_AARCH64: u64 = 16 << (20 - 12);

/// Paravisor measured config for vtl2.
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct ParavisorMeasuredVtl2Config {
    /// Magic value. Must be [`Self::MAGIC`].
    pub magic: u64,
    /// The bit offset of vTOM, if non-zero.
    pub vtom_offset_bit: u8,
    /// Padding.
    pub padding: [u8; 7],
}

impl ParavisorMeasuredVtl2Config {
    /// Magic value for the measured config, which is "OHCLVTL2".
    pub const MAGIC: u64 = 0x4F48434C56544C32;
}
