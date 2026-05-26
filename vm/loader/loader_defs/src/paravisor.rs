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
/// Maximum size in pages of the optional ContainerPolicy measured page
/// region. The region is always reserved in the layout so page indices are
/// deterministic, but its pages are only imported into the IGVM file when a
/// container policy is configured at build time.
pub const PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_SIZE_PAGES: u64 = 2;

/// Count for vtl 2 measured config region size.
pub const PARAVISOR_MEASURED_VTL2_CONFIG_REGION_PAGE_COUNT: u64 =
    PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_SIZE_PAGES
        + PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES
        + PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_SIZE_PAGES;

// Measured config comes after the unmeasured config
/// The page index to the list of accepted pages
pub const PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX: u64 =
    PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_BASE_INDEX
        + PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_PAGE_COUNT_MAX;

/// The page index for measured VTL2 config.
pub const PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX: u64 =
    PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_PAGE_INDEX
        + PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_SIZE_PAGES;

/// The page index for the optional ContainerPolicy measured page(s).
/// The first page contains the framed [`ContainerPolicy`] mesh-encoded
/// payload (see [`encode_container_policy_page`]); subsequent pages (up
/// to [`PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_SIZE_PAGES`]
/// total) hold the remainder of the body when needed.
pub const PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_PAGE_INDEX: u64 =
    PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX + PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES;

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
    /// Packed pointer locating the optional [`ContainerPolicy`] measured
    /// page(s) within the VTL2 config region.
    ///
    /// Encoding:
    /// - **low 52 bits** = page index (region-relative, same convention as
    ///   the other `*_PAGE_INDEX` constants in this module).
    /// - **high 12 bits** = page count.
    ///
    /// A `page_count` of zero means absent (no container policy is
    /// configured). Older IGVMs that pre-date this field naturally read
    /// back as zero because the page is zero-padded to 4 KiB before
    /// measurement.
    ///
    /// Use [`Self::pack_container_policy_location`] /
    /// [`Self::container_policy_page_index`] /
    /// [`Self::container_policy_page_count`] for unambiguous access.
    container_policy_location: u64,
}

impl ParavisorMeasuredVtl2Config {
    /// Magic value for the measured config, which is "OHCLVTL2".
    pub const MAGIC: u64 = 0x4F48434C56544C32;

    /// Number of bits used to encode the page index in
    /// [`Self::container_policy_location`].
    pub const CONTAINER_POLICY_INDEX_BITS: u32 = 52;

    /// Bit mask covering the page index half of
    /// [`Self::container_policy_location`].
    pub const CONTAINER_POLICY_INDEX_MASK: u64 = (1u64 << Self::CONTAINER_POLICY_INDEX_BITS) - 1;

    /// Number of bits used to encode the page count in
    /// [`Self::container_policy_location`].
    pub const CONTAINER_POLICY_COUNT_BITS: u32 = 64 - Self::CONTAINER_POLICY_INDEX_BITS;

    /// Maximum representable page index in
    /// [`Self::container_policy_location`].
    pub const CONTAINER_POLICY_INDEX_MAX: u64 = Self::CONTAINER_POLICY_INDEX_MASK;

    /// Maximum representable page count in
    /// [`Self::container_policy_location`].
    pub const CONTAINER_POLICY_COUNT_MAX: u64 = (1u64 << Self::CONTAINER_POLICY_COUNT_BITS) - 1;

    /// Pack a (page_index, page_count) pair into the wire encoding used
    /// by [`Self::container_policy_location`].
    ///
    /// Panics in debug builds if either component exceeds its respective
    /// bit budget. Release builds wrap, so callers should always be
    /// bound by the corresponding constants.
    #[inline]
    pub const fn pack_container_policy_location(page_index: u64, page_count: u64) -> u64 {
        debug_assert!(page_index <= Self::CONTAINER_POLICY_INDEX_MAX);
        debug_assert!(page_count <= Self::CONTAINER_POLICY_COUNT_MAX);
        (page_index & Self::CONTAINER_POLICY_INDEX_MASK)
            | ((page_count & Self::CONTAINER_POLICY_COUNT_MAX) << Self::CONTAINER_POLICY_INDEX_BITS)
    }

    /// Region-relative page index of the container policy page(s), or
    /// undefined if [`Self::container_policy_page_count`] returns zero.
    #[inline]
    pub const fn container_policy_page_index(&self) -> u64 {
        self.container_policy_location & Self::CONTAINER_POLICY_INDEX_MASK
    }

    /// Number of container policy pages, or zero if no container policy
    /// was configured at IGVM build time.
    #[inline]
    pub const fn container_policy_page_count(&self) -> u64 {
        self.container_policy_location >> Self::CONTAINER_POLICY_INDEX_BITS
    }
}

const_assert_eq!(size_of::<ParavisorMeasuredVtl2Config>(), 24);

pub use container_policy::CONTAINER_POLICY_LEN_PREFIX_BYTES;
pub use container_policy::ContainerPolicy;
pub use container_policy::ContainerPolicyDecodeError;
pub use container_policy::CwcowPolicy;
pub use container_policy::decode_container_policy_page;
pub use container_policy::encode_container_policy_page;

/// On-wire types for the optional measured container policy page.
///
/// The page (when present) holds a small framing prefix followed by a
/// `mesh_protobuf`-encoded [`ContainerPolicy`]. The location of the page
/// within the VTL2 config region is recorded in
/// [`ParavisorMeasuredVtl2Config::container_policy_location`].
///
/// # Page payload framing
///
/// The pages start with a fixed
/// [`CONTAINER_POLICY_LEN_PREFIX_BYTES`]-byte little-endian `u32` length
/// header giving the size of the protobuf payload that follows. The
/// remainder of the reserved page span is zero-padded. The length
/// prefix is required because `mesh_protobuf` does not natively tolerate
/// trailing zero bytes after a complete message (it interprets them as
/// the start of additional fields and errors out).
///
/// Use [`encode_container_policy_page`] / [`decode_container_policy_page`]
/// to produce / consume the page bytes; both ends of the wire share the
/// same framing helpers so the format can only ever change in lockstep.
///
/// # Onboarding a new product
///
/// **Default case** (manifest fields match wire fields by name):
///   1. Define a `#[derive(mesh_protobuf::Protobuf)]` body struct with
///      `#[cfg_attr(feature = "manifest", derive(serde::Deserialize))]`.
///   2. Add a `#[mesh(N)] Foo(FooPolicy)` variant to [`ContainerPolicy`]
///      with a **fresh** mesh tag (never reuse a tag — it would silently
///      change the measured wire format for an existing product).
///
/// **Build-side translation when needed** (e.g. a manifest field is a
/// file path whose contents must be embedded into the measured bytes):
/// use a *field-level* `#[serde(deserialize_with = "...")]` adapter on
/// the wire body. The adapter runs only during deserialization (it is
/// gated by the `manifest` feature), so the wire type stays a single
/// struct and the runtime build does not pull in std.
///
/// # Hard rule
///
/// **Never** derive `serde::Serialize` on [`ContainerPolicy`] or any of
/// its body structs. The wire bytes are the only canonical export; a
/// symmetric Serialize impl would silently break the asymmetry of any
/// `deserialize_with` adapter (e.g. a path read in via deserialize would
/// round-trip back as raw bytes, not a path).
pub mod container_policy {
    extern crate alloc;

    use alloc::vec::Vec;

    /// Number of bytes in the length-prefix framing header that precedes
    /// the mesh-encoded payload on the measured container policy page.
    pub const CONTAINER_POLICY_LEN_PREFIX_BYTES: usize = 4;

    /// The wire format for the optional measured container policy page.
    ///
    /// Each variant is a product; the `#[mesh(N)]` tag is the product
    /// identifier on the wire and **must never be reused**. Adding a new
    /// product is purely additive — new variants get a fresh tag.
    #[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq)]
    #[cfg_attr(feature = "manifest", derive(serde::Deserialize))]
    #[cfg_attr(
        feature = "manifest",
        serde(rename_all = "snake_case", deny_unknown_fields)
    )]
    #[mesh(package = "openhcl.container_policy")]
    pub enum ContainerPolicy {
        /// Confidential Windows Container Optimized Workload (CWCOW)
        /// policy: locks down OpenHCL behaviour for the CWCOW product
        /// family (read-only VMGS, required secure boot variables,
        /// required BCD integrity, required Secure AVIC on platforms
        /// that support it, etc.).
        #[mesh(1)]
        Cwcow(CwcowPolicy),
    }

    /// CWCOW container policy body.
    ///
    /// All fields are independently measurable so attestation can
    /// reason about each enforcement requirement in isolation. The
    /// shape is intentionally flat — products that need richer
    /// configuration should compose sub-structs (with their own
    /// `#[mesh(N)]` tags) rather than overload existing fields.
    #[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq, Default)]
    #[cfg_attr(feature = "manifest", derive(serde::Deserialize))]
    #[cfg_attr(
        feature = "manifest",
        serde(rename_all = "snake_case", deny_unknown_fields)
    )]
    #[mesh(package = "openhcl.container_policy")]
    pub struct CwcowPolicy {
        /// Enforce read-only mode for the VMGS partition. With this
        /// set, OpenHCL refuses writes to the VMGS (including any
        /// host-initiated change attempt).
        #[mesh(1)]
        pub vmgs_read_only: bool,

        /// Require secure-boot-only mode: refuse to boot if secure
        /// boot is not enabled.
        #[mesh(2)]
        pub require_secure_boot: bool,

        /// Require the presence of secure boot variables (PK, KEK,
        /// db, dbx, etc.) in the UEFI nvram. Builds without the
        /// expected variables are refused.
        #[mesh(3)]
        pub require_secure_boot_vars: bool,

        /// Require the `BootConfigurationDataHash` UEFI variable to
        /// be set via the custom UEFI JSON below, providing BCD
        /// integrity at boot.
        #[mesh(4)]
        pub require_bcd_integrity: bool,

        /// Require Secure AVIC to be enabled on platforms that
        /// support it (currently Turin SNP). OpenHCL refuses to
        /// continue if this is set but Secure AVIC is disabled.
        #[mesh(5)]
        pub require_secure_avic: bool,

        /// Debug mode relaxes the secure-boot **presence** check
        /// (`require_secure_boot` and `require_secure_boot_vars`)
        /// to aid local development. All other checks (Secure
        /// AVIC, BCD integrity, VMGS read-only) remain in force.
        #[mesh(6)]
        pub debug_mode: bool,

        /// Custom UEFI JSON file embedded into the measured policy
        /// page. The manifest schema accepts a **path** (string)
        /// which is read into bytes at IGVM build time via the
        /// field-level [`read_custom_uefi_json_path`] adapter; the
        /// wire format only carries the bytes.
        #[mesh(7)]
        #[cfg_attr(
            feature = "manifest",
            serde(default, deserialize_with = "read_custom_uefi_json_path")
        )]
        pub custom_uefi_json: Vec<u8>,
    }

    /// Field-level serde adapter for [`CwcowPolicy::custom_uefi_json`].
    /// In manifest JSON the field is a path string; at deserialization
    /// time we read the file into the byte buffer embedded into the
    /// wire enum. This keeps the wire type single-struct — no separate
    /// `*Input` mirror — while still allowing the build to absorb
    /// out-of-band assets.
    ///
    /// `Default::default()` (empty) is accepted via `serde(default)` so
    /// builds that don't ship a custom UEFI JSON work out of the box.
    #[cfg(feature = "manifest")]
    fn read_custom_uefi_json_path<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::Deserialize;

        // Manifest accepts a JSON string holding the path.
        let path = std::path::PathBuf::deserialize(d)?;
        if path.as_os_str().is_empty() {
            return Ok(Vec::new());
        }
        std::fs::read(&path).map_err(|e| {
            serde::de::Error::custom(format!(
                "failed to read custom UEFI JSON at {}: {}",
                path.display(),
                e
            ))
        })
    }

    /// Errors that may arise while decoding the framed measured page
    /// bytes back into a [`ContainerPolicy`].
    #[derive(Debug)]
    pub enum ContainerPolicyDecodeError {
        /// Buffer was too small to contain the length prefix.
        PrefixMissing {
            /// Bytes available in the buffer.
            available: usize,
        },
        /// Buffer was big enough for the length prefix, but the
        /// declared body length runs past the buffer end.
        TruncatedBody {
            /// Declared body length (bytes after the prefix).
            declared: usize,
            /// Actual bytes available after the prefix.
            available: usize,
        },
        /// The mesh_protobuf decoder rejected the body bytes.
        Mesh(mesh_protobuf::Error),
    }

    impl core::fmt::Display for ContainerPolicyDecodeError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::PrefixMissing { available } => write!(
                    f,
                    "container policy buffer too small for length prefix: {available} bytes"
                ),
                Self::TruncatedBody {
                    declared,
                    available,
                } => write!(
                    f,
                    "container policy declared {declared} body bytes but only {available} available"
                ),
                Self::Mesh(_) => write!(f, "container policy mesh decode error"),
            }
        }
    }

    impl core::error::Error for ContainerPolicyDecodeError {
        fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
            match self {
                Self::Mesh(e) => Some(e),
                _ => None,
            }
        }
    }

    /// Encode a [`ContainerPolicy`] into the on-wire framed bytes that
    /// will be measured onto the policy page(s).
    ///
    /// The returned buffer is NOT zero-padded to a page boundary; the
    /// IGVM importer pads when it calls `import_pages`.
    pub fn encode_container_policy_page(policy: &ContainerPolicy) -> Vec<u8> {
        let body = mesh_protobuf::encode(policy.clone());
        let mut out = Vec::with_capacity(CONTAINER_POLICY_LEN_PREFIX_BYTES + body.len());
        // Length is bounded by the region capacity (< 8 KiB), but we
        // store it as u32 LE for forward headroom.
        let len = u32::try_from(body.len()).expect("policy body fits in u32");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Decode the framed page bytes back into a [`ContainerPolicy`].
    ///
    /// `bytes` may include arbitrary trailing zero padding (typical for
    /// a page-aligned read) — only the declared `declared_len` body
    /// bytes after the prefix are interpreted.
    pub fn decode_container_policy_page(
        bytes: &[u8],
    ) -> Result<ContainerPolicy, ContainerPolicyDecodeError> {
        if bytes.len() < CONTAINER_POLICY_LEN_PREFIX_BYTES {
            return Err(ContainerPolicyDecodeError::PrefixMissing {
                available: bytes.len(),
            });
        }
        let mut prefix = [0u8; CONTAINER_POLICY_LEN_PREFIX_BYTES];
        prefix.copy_from_slice(&bytes[..CONTAINER_POLICY_LEN_PREFIX_BYTES]);
        let declared = u32::from_le_bytes(prefix) as usize;
        let payload = &bytes[CONTAINER_POLICY_LEN_PREFIX_BYTES..];
        if payload.len() < declared {
            return Err(ContainerPolicyDecodeError::TruncatedBody {
                declared,
                available: payload.len(),
            });
        }
        mesh_protobuf::decode(&payload[..declared]).map_err(ContainerPolicyDecodeError::Mesh)
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::vec;

    // ---------------------------------------------------------------
    // ParavisorMeasuredVtl2Config: pack / unpack helpers
    // ---------------------------------------------------------------

    #[test]
    fn pack_zero_is_absent() {
        let packed = ParavisorMeasuredVtl2Config::pack_container_policy_location(0, 0);
        assert_eq!(packed, 0);
        let cfg = ParavisorMeasuredVtl2Config {
            magic: ParavisorMeasuredVtl2Config::MAGIC,
            vtom_offset_bit: 0,
            padding: [0; 7],
            container_policy_location: packed,
        };
        assert_eq!(cfg.container_policy_page_index(), 0);
        assert_eq!(cfg.container_policy_page_count(), 0);
    }

    #[test]
    fn pack_unpack_round_trip_representative_pairs() {
        let pairs: &[(u64, u64)] = &[(0, 1), (1, 0), (1, 1), (106, 2), (4095, 1)];
        for &(index, count) in pairs {
            let packed = ParavisorMeasuredVtl2Config::pack_container_policy_location(index, count);
            let cfg = ParavisorMeasuredVtl2Config {
                magic: ParavisorMeasuredVtl2Config::MAGIC,
                vtom_offset_bit: 0,
                padding: [0; 7],
                container_policy_location: packed,
            };
            assert_eq!(
                cfg.container_policy_page_index(),
                index,
                "pair: {:?}",
                (index, count)
            );
            assert_eq!(
                cfg.container_policy_page_count(),
                count,
                "pair: {:?}",
                (index, count)
            );
        }
    }

    #[test]
    fn pack_bit_layout_count_in_high_bits() {
        // count==1, index==0 -> bit 52 set, no other bits.
        let packed = ParavisorMeasuredVtl2Config::pack_container_policy_location(0, 1);
        assert_eq!(packed, 1u64 << 52);
        // count==0, index==1 -> bit 0 set.
        let packed = ParavisorMeasuredVtl2Config::pack_container_policy_location(1, 0);
        assert_eq!(packed, 1);
    }

    #[test]
    fn pack_unpack_boundary_max_values() {
        let index = ParavisorMeasuredVtl2Config::CONTAINER_POLICY_INDEX_MAX;
        let count = ParavisorMeasuredVtl2Config::CONTAINER_POLICY_COUNT_MAX;
        let packed = ParavisorMeasuredVtl2Config::pack_container_policy_location(index, count);
        let cfg = ParavisorMeasuredVtl2Config {
            magic: ParavisorMeasuredVtl2Config::MAGIC,
            vtom_offset_bit: 0,
            padding: [0; 7],
            container_policy_location: packed,
        };
        assert_eq!(cfg.container_policy_page_index(), index);
        assert_eq!(cfg.container_policy_page_count(), count);
        // All 64 bits used at the maximum.
        assert_eq!(packed, u64::MAX);
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn pack_panics_on_index_overflow() {
        let _ = ParavisorMeasuredVtl2Config::pack_container_policy_location(
            ParavisorMeasuredVtl2Config::CONTAINER_POLICY_INDEX_MAX + 1,
            0,
        );
    }

    #[test]
    #[should_panic]
    #[cfg(debug_assertions)]
    fn pack_panics_on_count_overflow() {
        let _ = ParavisorMeasuredVtl2Config::pack_container_policy_location(
            0,
            ParavisorMeasuredVtl2Config::CONTAINER_POLICY_COUNT_MAX + 1,
        );
    }

    // ---------------------------------------------------------------
    // ParavisorMeasuredVtl2Config: struct layout & legacy back-compat
    // ---------------------------------------------------------------

    #[test]
    fn measured_vtl2_config_size_is_24_bytes() {
        // Static guard against accidental field-add that would change the
        // measured wire format.
        assert_eq!(size_of::<ParavisorMeasuredVtl2Config>(), 24);
    }

    #[test]
    fn measured_vtl2_config_field_offsets() {
        // Spot-check that the binary layout matches the documented
        // encoding so older readers / hardware analysers stay happy.
        let cfg = ParavisorMeasuredVtl2Config {
            magic: 0x1122_3344_5566_7788,
            vtom_offset_bit: 0x99,
            padding: [0; 7],
            container_policy_location: ParavisorMeasuredVtl2Config::pack_container_policy_location(
                0xABCD, 0x002,
            ),
        };
        let bytes = cfg.as_bytes();
        // magic at [0..8] (little-endian).
        assert_eq!(&bytes[0..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        // vtom_offset_bit at [8].
        assert_eq!(bytes[8], 0x99);
        // padding at [9..16].
        assert_eq!(&bytes[9..16], &[0u8; 7]);
        // container_policy_location at [16..24].
        let location = ParavisorMeasuredVtl2Config::pack_container_policy_location(0xABCD, 0x002);
        assert_eq!(&bytes[16..24], &location.to_le_bytes());
    }

    #[test]
    fn measured_vtl2_config_round_trips() {
        let cfg = ParavisorMeasuredVtl2Config {
            magic: ParavisorMeasuredVtl2Config::MAGIC,
            vtom_offset_bit: 47,
            padding: [0; 7],
            container_policy_location: ParavisorMeasuredVtl2Config::pack_container_policy_location(
                106, 2,
            ),
        };
        let bytes = cfg.as_bytes().to_vec();
        let (decoded, rest) = ParavisorMeasuredVtl2Config::ref_from_prefix(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded.magic, ParavisorMeasuredVtl2Config::MAGIC);
        assert_eq!(decoded.vtom_offset_bit, 47);
        assert_eq!(decoded.container_policy_page_index(), 106);
        assert_eq!(decoded.container_policy_page_count(), 2);
    }

    #[test]
    fn legacy_zero_padded_page_decodes_as_absent() {
        // Pre-change builders wrote 16 bytes onto a zero-padded 4 KiB
        // page. Read with the new (24-byte) struct, those bytes must
        // round-trip the original magic / vtom_offset_bit and present an
        // absent container policy.
        let mut page = [0u8; HV_PAGE_SIZE as usize];
        // Hand-craft the legacy 16-byte struct.
        page[0..8].copy_from_slice(&ParavisorMeasuredVtl2Config::MAGIC.to_le_bytes());
        page[8] = 17; // vtom_offset_bit
        // bytes 9..16 are padding (zero), and bytes 16.. (the new field)
        // are also zero — that's the "absent" signal.

        let (decoded, _rest) = ParavisorMeasuredVtl2Config::ref_from_prefix(&page).unwrap();
        assert_eq!(decoded.magic, ParavisorMeasuredVtl2Config::MAGIC);
        assert_eq!(decoded.vtom_offset_bit, 17);
        assert_eq!(decoded.container_policy_location, 0);
        assert_eq!(decoded.container_policy_page_index(), 0);
        assert_eq!(decoded.container_policy_page_count(), 0);
    }

    // ---------------------------------------------------------------
    // Region layout constants
    // ---------------------------------------------------------------

    #[test]
    fn container_policy_region_layout() {
        // The container policy region must come immediately after the
        // existing measured VTL2 config page so the build-side default
        // placement remains deterministic.
        assert_eq!(
            PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_PAGE_INDEX,
            PARAVISOR_MEASURED_VTL2_CONFIG_PAGE_INDEX + PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES
        );
        assert_eq!(
            PARAVISOR_MEASURED_VTL2_CONFIG_REGION_PAGE_COUNT,
            PARAVISOR_MEASURED_VTL2_CONFIG_ACCEPTED_MEMORY_SIZE_PAGES
                + PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES
                + PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_SIZE_PAGES
        );
    }

    #[test]
    fn region_budget_includes_container_policy() {
        let measured = PARAVISOR_MEASURED_VTL2_CONFIG_REGION_PAGE_COUNT;
        let max = PARAVISOR_VTL2_CONFIG_REGION_PAGE_COUNT_MAX;
        let unmeasured = PARAVISOR_UNMEASURED_VTL2_CONFIG_REGION_PAGE_COUNT_MAX;
        assert!(
            max >= unmeasured + measured,
            "VTL2 config region budget {max} does not fit unmeasured ({unmeasured}) + measured ({measured})"
        );
    }

    // ---------------------------------------------------------------
    // ContainerPolicy: mesh_protobuf round trip
    // ---------------------------------------------------------------

    fn sample_cwcow_policy() -> CwcowPolicy {
        CwcowPolicy {
            vmgs_read_only: true,
            require_secure_boot: true,
            require_secure_boot_vars: true,
            require_bcd_integrity: true,
            require_secure_avic: false,
            debug_mode: false,
            custom_uefi_json: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }
    }

    #[test]
    fn container_policy_round_trip_default_cwcow() {
        // All-false / empty body to exercise the cheapest encoding.
        let policy = ContainerPolicy::Cwcow(CwcowPolicy::default());
        let bytes = mesh_protobuf::encode(policy.clone());
        let decoded: ContainerPolicy = mesh_protobuf::decode(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn container_policy_round_trip_nontrivial_cwcow() {
        let policy = ContainerPolicy::Cwcow(sample_cwcow_policy());
        let bytes = mesh_protobuf::encode(policy.clone());
        let decoded: ContainerPolicy = mesh_protobuf::decode(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn container_policy_decode_tolerates_trailing_zero_padding() {
        let policy = ContainerPolicy::Cwcow(sample_cwcow_policy());
        let mut bytes = encode_container_policy_page(&policy);
        // Simulate a page-padded buffer (the IGVM importer zero-pads to
        // the page boundary).
        bytes.resize(HV_PAGE_SIZE as usize, 0);
        let decoded = decode_container_policy_page(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn container_policy_decode_rejects_garbage() {
        // Random bytes that don't form a valid framed message must fail
        // to decode rather than silently round-tripping garbage. We
        // feed a buffer whose length prefix declares more bytes than
        // are available, plus an empty buffer that lacks a prefix.
        let truncated_prefix = [0xFFu8, 0xFE]; // < CONTAINER_POLICY_LEN_PREFIX_BYTES
        assert!(matches!(
            decode_container_policy_page(&truncated_prefix),
            Err(ContainerPolicyDecodeError::PrefixMissing { .. })
        ));

        // Prefix declares 1000 bytes but only 4 (the prefix itself) are
        // available — truncated body error.
        let mut header_only = vec![0u8; 4];
        let declared_len: u32 = 1000;
        header_only[..4].copy_from_slice(&declared_len.to_le_bytes());
        assert!(matches!(
            decode_container_policy_page(&header_only),
            Err(ContainerPolicyDecodeError::TruncatedBody { .. })
        ));

        // Length prefix that declares a small but malformed body
        // produces a mesh decode error.
        let mut bad_body = vec![0u8; 4 + 8];
        let declared_len: u32 = 8;
        bad_body[..4].copy_from_slice(&declared_len.to_le_bytes());
        bad_body[4..].copy_from_slice(&[0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8]);
        assert!(matches!(
            decode_container_policy_page(&bad_body),
            Err(ContainerPolicyDecodeError::Mesh(_))
        ));
    }

    #[test]
    fn container_policy_page_round_trip() {
        // End-to-end framing helper round-trip.
        let policy = ContainerPolicy::Cwcow(sample_cwcow_policy());
        let bytes = encode_container_policy_page(&policy);
        let decoded = decode_container_policy_page(&bytes).unwrap();
        assert_eq!(decoded, policy);
        // Length prefix matches the declared body length.
        let mut prefix = [0u8; CONTAINER_POLICY_LEN_PREFIX_BYTES];
        prefix.copy_from_slice(&bytes[..CONTAINER_POLICY_LEN_PREFIX_BYTES]);
        let declared = u32::from_le_bytes(prefix) as usize;
        assert_eq!(declared, bytes.len() - CONTAINER_POLICY_LEN_PREFIX_BYTES);
    }

    #[test]
    fn container_policy_encoded_size_within_region_budget() {
        // A CWCOW policy with a modest custom UEFI JSON payload must
        // fit comfortably in the reserved region. Oversize cases are
        // exercised in the loader crate's tests where the size check
        // lives.
        let mut p = sample_cwcow_policy();
        p.custom_uefi_json = vec![0u8; 1024];
        let policy = ContainerPolicy::Cwcow(p);
        let bytes = encode_container_policy_page(&policy);
        let region_capacity =
            (PARAVISOR_MEASURED_VTL2_CONFIG_CONTAINER_POLICY_SIZE_PAGES * HV_PAGE_SIZE) as usize;
        assert!(bytes.len() <= region_capacity);
    }

    // ---------------------------------------------------------------
    // Serde manifest deserialization (only under the `manifest` feature)
    // ---------------------------------------------------------------

    #[cfg(feature = "manifest")]
    mod serde_tests {
        use super::*;

        fn from_json(s: &str) -> Result<ContainerPolicy, serde_json::Error> {
            serde_json::from_str(s)
        }

        #[test]
        fn deserialize_cwcow_full() {
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": true,
                    "require_secure_boot": true,
                    "require_secure_boot_vars": true,
                    "require_bcd_integrity": true,
                    "require_secure_avic": true,
                    "debug_mode": false,
                    "custom_uefi_json": ""
                }
            }"#;
            let policy: ContainerPolicy = from_json(json).unwrap();
            match policy {
                ContainerPolicy::Cwcow(p) => {
                    assert!(p.vmgs_read_only);
                    assert!(p.require_secure_boot);
                    assert!(p.require_secure_boot_vars);
                    assert!(p.require_bcd_integrity);
                    assert!(p.require_secure_avic);
                    assert!(!p.debug_mode);
                    assert!(p.custom_uefi_json.is_empty());
                }
            }
        }

        #[test]
        fn deserialize_cwcow_omits_custom_uefi_json() {
            // The field is `serde(default)` so absent keys default to
            // an empty body.
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": false,
                    "require_secure_boot": true,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "debug_mode": true
                }
            }"#;
            let policy: ContainerPolicy = from_json(json).unwrap();
            match policy {
                ContainerPolicy::Cwcow(p) => {
                    assert!(p.debug_mode);
                    assert!(p.custom_uefi_json.is_empty());
                }
            }
        }

        #[test]
        fn deserialize_cwcow_reads_path_into_bytes() {
            // Write a temp file and reference it from the manifest;
            // the field-level adapter must read its contents into the
            // wire `custom_uefi_json` bytes.
            let path = std::env::temp_dir().join("container_policy_cwcow_test.json");
            let contents = b"{\"uefi\": \"sample\"}";
            std::fs::write(&path, contents).expect("write temp");
            let json = format!(
                r#"{{
                    "cwcow": {{
                        "vmgs_read_only": false,
                        "require_secure_boot": false,
                        "require_secure_boot_vars": false,
                        "require_bcd_integrity": false,
                        "require_secure_avic": false,
                        "debug_mode": false,
                        "custom_uefi_json": "{}"
                    }}
                }}"#,
                path.to_str().unwrap().replace('\\', "\\\\")
            );
            let policy: ContainerPolicy = from_json(&json).unwrap();
            match policy {
                ContainerPolicy::Cwcow(p) => assert_eq!(p.custom_uefi_json, contents.to_vec()),
            }
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn deserialize_cwcow_missing_path_is_an_error() {
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "debug_mode": false,
                    "custom_uefi_json": "/nonexistent/path/to/uefi.json"
                }
            }"#;
            let err = from_json(json);
            assert!(err.is_err(), "expected error, got: {err:?}");
        }

        #[test]
        fn deserialize_rejects_unknown_variant() {
            let err = from_json(r#"{"unknown_product":{}}"#);
            assert!(err.is_err());
        }

        #[test]
        fn deserialize_rejects_unknown_field() {
            // `deny_unknown_fields` on CwcowPolicy.
            let err = from_json(
                r#"{"cwcow":{
                    "vmgs_read_only": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "debug_mode": false,
                    "extra": 0
                }}"#,
            );
            assert!(err.is_err(), "expected error, got: {err:?}");
        }

        #[test]
        fn deserialize_rejects_pascal_case_variant() {
            // `rename_all = "snake_case"` on ContainerPolicy.
            let err = from_json(r#"{"Cwcow":{}}"#);
            assert!(err.is_err(), "expected error, got: {err:?}");
        }
    }
}
