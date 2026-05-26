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

/// Size in pages of the VTL2 measured config region.
///
/// The region carries the [`ParavisorMeasuredVtl2Config`] struct
/// followed in-place by the optional [`ContainerPolicy`] payload (see
/// [`CONTAINER_POLICY_INLINE_OFFSET`] and
/// [`ParavisorMeasuredVtl2Config::container_policy_size`]). The region
/// has a fixed page count; the encoded policy body must fit within
/// `SIZE_PAGES * HV_PAGE_SIZE - sizeof::<ParavisorMeasuredVtl2Config>()`
/// bytes (see [`container_policy_max_size_bytes`]).
///
/// If a product's encoded policy outgrows this budget the IGVM build
/// will panic. Bumping this constant to fit a larger payload is
/// intentional friction: it changes the measured size of every IGVM
/// (including ones that don't carry a policy) and therefore the
/// attestation contract — the bump needs to be a conscious decision.
pub const PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES: u64 = 2;

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
///
/// May be followed in-place (on the same measured region) by an
/// optional [`ContainerPolicy`] payload starting at byte
/// [`CONTAINER_POLICY_INLINE_OFFSET`]. The payload's size in bytes is
/// recorded in [`Self::container_policy_size`]; the measured config
/// region is always [`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES`]
/// pages regardless of policy size. A size of zero — including
/// all-zero trailing bytes in IGVMs that pre-date the container
/// policy feature — signals absent.
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
    /// Byte length of the [`ContainerPolicy`] payload that follows the
    /// struct on the same measured region. A value of `0` means no
    /// policy is configured (the typical / legacy state).
    ///
    /// The runtime reads exactly this many bytes from
    /// [`CONTAINER_POLICY_INLINE_OFFSET`] and decodes them as the
    /// `mesh_protobuf`-encoded [`ContainerPolicy`].
    pub container_policy_size: u32,
    /// Reserved for future use. Must be zero on builds that don't use
    /// it (and is naturally zero in pre-feature IGVMs).
    pub reserved: [u8; 4],
}

impl ParavisorMeasuredVtl2Config {
    /// Magic value for the measured config, which is "OHCLVTL2".
    pub const MAGIC: u64 = 0x4F48434C56544C32;
}

const_assert_eq!(size_of::<ParavisorMeasuredVtl2Config>(), 24);

/// Byte offset within the measured VTL2 config region at which the
/// optional [`ContainerPolicy`] payload begins. Builders write the
/// `mesh_protobuf`-encoded body at this offset; runtime readers
/// consume the next
/// [`ParavisorMeasuredVtl2Config::container_policy_size`] bytes.
pub const CONTAINER_POLICY_INLINE_OFFSET: usize = size_of::<ParavisorMeasuredVtl2Config>();

/// Maximum byte size of a [`ContainerPolicy`] payload supported by this
/// build. Equal to
/// `PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES * HV_PAGE_SIZE` minus the
/// struct that precedes it.
#[inline]
pub const fn container_policy_max_size_bytes() -> usize {
    (PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES as usize) * (HV_PAGE_SIZE as usize)
        - CONTAINER_POLICY_INLINE_OFFSET
}

pub use container_policy::ContainerPolicy;
pub use container_policy::ContainerPolicyDecodeError;
pub use container_policy::CwcowPolicy;
pub use container_policy::decode_container_policy_page;
pub use container_policy::encode_container_policy_page;

/// On-wire types for the optional measured container policy payload.
///
/// The payload (when present) is appended in-place to the same measured
/// VTL2 config region that carries [`ParavisorMeasuredVtl2Config`],
/// starting at byte [`CONTAINER_POLICY_INLINE_OFFSET`]. Its length in
/// bytes is recorded in
/// [`ParavisorMeasuredVtl2Config::container_policy_size`]; a size of
/// zero — including all-zero trailing bytes in IGVMs that pre-date the
/// feature — signals absent.
///
/// # Build-time sizing
///
/// The measured config region is a fixed
/// [`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES`] pages — currently two
/// — for every IGVM regardless of whether a policy is configured.
/// Absent-policy builds carry `container_policy_size == 0` and a
/// fully-zero payload area; their measurement is determined by
/// `SIZE_PAGES` (so bumping `SIZE_PAGES` re-measures every IGVM, not
/// just policy-carrying ones). The encoded policy body must fit in
/// [`container_policy_max_size_bytes`]; the IGVM builder panics
/// otherwise, and the developer adding the policy must bump
/// [`PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES`] (which also changes
/// the measurement of every IGVM and therefore the attestation
/// contract).
///
/// # Onboarding a new product
///
/// **Default case** (manifest fields match wire fields by name):
///   1. Define a `#[derive(mesh_protobuf::Protobuf)]` body struct with
///      `#[cfg_attr(feature = "manifest", derive(serde::Serialize,
///      serde::Deserialize))]`.
///   2. Add a `#[mesh(N)] Foo(FooPolicy)` variant to [`ContainerPolicy`]
///      with a **fresh** mesh tag (never reuse a tag — it would
///      silently change the measured wire format for an existing
///      product).
///
/// **Custom JSON encoding for individual fields** (e.g. a manifest
/// field is a base64-encoded binary blob): use a *symmetric*
/// `#[serde(with = "module_name")]` field adapter where the helper
/// module exposes matching `serialize` and `deserialize` functions.
/// Symmetry is mandatory: the JSON value produced by `serialize` must
/// deserialize back to a byte-identical Rust value. The compiler
/// enforces both directions exist; round-trip tests catch any encoding
/// mismatch.
///
/// **Do not** use one-directional `#[serde(deserialize_with = "...")]`
/// adapters: they leave the matching `Serialize` impl free to emit a
/// shape that won't deserialize again, silently corrupting any future
/// manifest dump. The `custom_uefi_json` field on [`CwcowPolicy`] is
/// the canonical symmetric-adapter example (base64 ↔ bytes via
/// `custom_uefi_json_serde`).
pub mod container_policy {
    extern crate alloc;

    use alloc::vec::Vec;

    /// The wire format for the optional measured container policy
    /// payload. The encoded bytes are appended in-place after
    /// [`super::ParavisorMeasuredVtl2Config`] on the same measured
    /// config region.
    ///
    /// Each variant is a product; the `#[mesh(N)]` tag is the product
    /// identifier on the wire and **must never be reused**. Adding a new
    /// product is purely additive — new variants get a fresh tag.
    #[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq)]
    #[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(
        feature = "manifest",
        serde(rename_all = "snake_case", deny_unknown_fields)
    )]
    #[mesh(package = "openhcl.container_policy")]
    pub enum ContainerPolicy {
        /// Confidential Windows Container on Windows (CWCOW) policy:
        /// locks down OpenHCL behaviour for the CWCOW product
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
    #[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
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

        /// Custom UEFI JSON bytes embedded into the measured policy
        /// payload. In manifest JSON the field is a **base64-encoded
        /// string** (RFC 4648 standard alphabet) and is **mandatory**
        /// — there is no serde default, so omitting it in a CWCOW
        /// manifest is a deserialization error. The field's serde
        /// adapter handles both directions symmetrically: encoding
        /// raw bytes back to base64 on serialize, decoding base64 to
        /// raw bytes on deserialize. Manifest authors can embed
        /// arbitrary binary directly in JSON without referencing an
        /// out-of-band file.
        ///
        /// The bytes themselves must be **non-empty**; an empty
        /// payload is rejected at IGVM build time with a panic from
        /// `encode_container_policy_bytes`, because CWCOW relies on
        /// the custom UEFI JSON to lock down secure-boot variables
        /// and BCD integrity.
        #[mesh(6)]
        #[cfg_attr(feature = "manifest", serde(with = "custom_uefi_json_serde"))]
        pub custom_uefi_json: Vec<u8>,
    }

    /// Symmetric serde adapter for [`CwcowPolicy::custom_uefi_json`].
    /// Both `serialize` and `deserialize` go through RFC 4648
    /// standard base64 — bytes are emitted as a base64 string on
    /// serialize, and a base64 string is decoded to bytes on
    /// deserialize.
    ///
    /// Symmetry is the whole point: any future code that re-serializes
    /// a `ContainerPolicy` to JSON produces a manifest that, when
    /// deserialized again, yields a byte-identical value. The
    /// previous one-way `deserialize_with` adapter required a "never
    /// derive Serialize" hard rule to prevent silent corruption;
    /// using `#[serde(with = "...")]` makes that rule structurally
    /// unnecessary.
    ///
    /// `Default::default()` (empty) is accepted via `serde(default)`
    /// on the field so builds that don't ship a custom UEFI JSON
    /// work out of the box; the empty string also decodes to an
    /// empty `Vec`.
    #[cfg(feature = "manifest")]
    mod custom_uefi_json_serde {
        extern crate alloc;

        use alloc::format;
        use alloc::string::String;
        use alloc::vec::Vec;
        use base64::Engine as _;
        use serde::Deserialize as _;

        pub fn serialize<S>(bytes: &Vec<u8>, s: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            s.serialize_str(&encoded)
        }

        pub fn deserialize<'de, D>(d: D) -> Result<Vec<u8>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            let s = String::deserialize(d)?;
            if s.is_empty() {
                return Ok(Vec::new());
            }
            base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map_err(|e| {
                    serde::de::Error::custom(format!("failed to base64-decode bytes: {e}"))
                })
        }
    }

    /// Errors that may arise while decoding the inline measured
    /// container policy bytes back into a [`ContainerPolicy`].
    #[derive(Debug)]
    pub enum ContainerPolicyDecodeError {
        /// The mesh_protobuf decoder rejected the bytes.
        Mesh(mesh_protobuf::Error),
    }

    impl core::fmt::Display for ContainerPolicyDecodeError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::Mesh(_) => write!(f, "container policy mesh decode error"),
            }
        }
    }

    impl core::error::Error for ContainerPolicyDecodeError {
        fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
            match self {
                Self::Mesh(e) => Some(e),
            }
        }
    }

    /// Encode a [`ContainerPolicy`] into the on-wire bytes that will be
    /// appended in-place to the measured config region after
    /// [`super::ParavisorMeasuredVtl2Config`]. The build records the
    /// returned length in
    /// [`super::ParavisorMeasuredVtl2Config::container_policy_size`]
    /// so the runtime knows how many bytes to read.
    pub fn encode_container_policy_page(policy: &ContainerPolicy) -> Vec<u8> {
        mesh_protobuf::encode(policy.clone())
    }

    /// Decode `bytes` (exactly the byte range
    /// `[CONTAINER_POLICY_INLINE_OFFSET .. CONTAINER_POLICY_INLINE_OFFSET +
    /// container_policy_size]`) into a [`ContainerPolicy`]. Callers
    /// must check `container_policy_size != 0` before invoking — a
    /// size of zero means no policy was configured.
    pub fn decode_container_policy_page(
        bytes: &[u8],
    ) -> Result<ContainerPolicy, ContainerPolicyDecodeError> {
        mesh_protobuf::decode(bytes).map_err(ContainerPolicyDecodeError::Mesh)
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::vec;

    // ---------------------------------------------------------------
    // ParavisorMeasuredVtl2Config: struct layout
    // ---------------------------------------------------------------

    #[test]
    fn measured_vtl2_config_size_is_24_bytes() {
        // Static guard against accidental field-add. The struct grew
        // from 16 to 24 bytes when container_policy_size + reserved
        // were added; further growth needs an explicit decision.
        assert_eq!(size_of::<ParavisorMeasuredVtl2Config>(), 24);
    }

    #[test]
    fn measured_vtl2_config_field_offsets() {
        let cfg = ParavisorMeasuredVtl2Config {
            magic: 0x1122_3344_5566_7788,
            vtom_offset_bit: 0x99,
            padding: [0; 7],
            container_policy_size: 0xABCDu32,
            reserved: [0; 4],
        };
        let bytes = cfg.as_bytes();
        // magic at [0..8] (little-endian).
        assert_eq!(&bytes[0..8], &0x1122_3344_5566_7788u64.to_le_bytes());
        // vtom_offset_bit at [8].
        assert_eq!(bytes[8], 0x99);
        // padding at [9..16].
        assert_eq!(&bytes[9..16], &[0u8; 7]);
        // container_policy_size at [16..20].
        assert_eq!(&bytes[16..20], &0xABCDu32.to_le_bytes());
        // reserved at [20..24].
        assert_eq!(&bytes[20..24], &[0u8; 4]);
    }

    #[test]
    fn measured_vtl2_config_round_trips() {
        let cfg = ParavisorMeasuredVtl2Config {
            magic: ParavisorMeasuredVtl2Config::MAGIC,
            vtom_offset_bit: 47,
            padding: [0; 7],
            container_policy_size: 256,
            reserved: [0; 4],
        };
        let bytes = cfg.as_bytes().to_vec();
        let (decoded, rest) = ParavisorMeasuredVtl2Config::ref_from_prefix(&bytes).unwrap();
        assert!(rest.is_empty());
        assert_eq!(decoded.magic, ParavisorMeasuredVtl2Config::MAGIC);
        assert_eq!(decoded.vtom_offset_bit, 47);
        assert_eq!(decoded.container_policy_size, 256);
    }

    #[test]
    fn container_policy_inline_offset_matches_struct_size() {
        assert_eq!(
            CONTAINER_POLICY_INLINE_OFFSET,
            size_of::<ParavisorMeasuredVtl2Config>()
        );
    }

    #[test]
    fn container_policy_max_size_matches_region_minus_struct() {
        assert_eq!(
            container_policy_max_size_bytes(),
            (PARAVISOR_MEASURED_VTL2_CONFIG_SIZE_PAGES as usize) * (HV_PAGE_SIZE as usize)
                - CONTAINER_POLICY_INLINE_OFFSET
        );
    }

    #[test]
    fn pre_feature_zeroed_page_decodes_as_absent() {
        // Pre-change builders wrote 16 bytes onto a zero-padded page.
        // The new struct read at offset 0 gets magic + vtom intact, and
        // bytes 16..24 (container_policy_size + reserved) are all zero
        // ⇒ size == 0 ⇒ runtime treats the policy as absent.
        let mut page = [0u8; HV_PAGE_SIZE as usize];
        page[0..8].copy_from_slice(&ParavisorMeasuredVtl2Config::MAGIC.to_le_bytes());
        page[8] = 17;
        let (decoded, _rest) = ParavisorMeasuredVtl2Config::ref_from_prefix(&page).unwrap();
        assert_eq!(decoded.magic, ParavisorMeasuredVtl2Config::MAGIC);
        assert_eq!(decoded.vtom_offset_bit, 17);
        assert_eq!(decoded.container_policy_size, 0);
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
            custom_uefi_json: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }
    }

    #[test]
    fn encode_decode_round_trip_default_cwcow() {
        let policy = ContainerPolicy::Cwcow(CwcowPolicy::default());
        let bytes = encode_container_policy_page(&policy);
        let decoded = decode_container_policy_page(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn encode_decode_round_trip_nontrivial_cwcow() {
        let policy = ContainerPolicy::Cwcow(sample_cwcow_policy());
        let bytes = encode_container_policy_page(&policy);
        let decoded = decode_container_policy_page(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn decode_rejects_garbage() {
        let bad = [0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8];
        assert!(matches!(
            decode_container_policy_page(&bad),
            Err(ContainerPolicyDecodeError::Mesh(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated() {
        let policy = ContainerPolicy::Cwcow(sample_cwcow_policy());
        let mut bytes = encode_container_policy_page(&policy);
        bytes.pop();
        assert!(matches!(
            decode_container_policy_page(&bytes),
            Err(ContainerPolicyDecodeError::Mesh(_))
        ));
    }

    #[test]
    fn encoded_size_fits_within_max() {
        let mut p = sample_cwcow_policy();
        p.custom_uefi_json = vec![0u8; 1024];
        let policy = ContainerPolicy::Cwcow(p);
        let bytes = encode_container_policy_page(&policy);
        assert!(bytes.len() <= container_policy_max_size_bytes());
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
                    assert!(p.custom_uefi_json.is_empty());
                }
            }
        }

        #[test]
        fn deserialize_cwcow_missing_custom_uefi_json_is_an_error() {
            // `custom_uefi_json` is mandatory in the manifest JSON —
            // omitting it must fail deserialization rather than
            // silently defaulting to an empty payload.
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": false,
                    "require_secure_boot": true,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false
                }
            }"#;
            let err = from_json(json).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(
                msg.contains("custom_uefi_json"),
                "expected error to mention custom_uefi_json, got: {msg}"
            );
        }

        #[test]
        fn deserialize_cwcow_decodes_base64_custom_uefi_json() {
            // Standard RFC 4648 base64 with padding. The decoded
            // bytes match the original string content.
            //
            // payload: b"{\"uefi\": \"sample\"}"
            //          base64: e30iOiAic2FtcGxlIn0= ❌ (wrong — recompute)
            // payload: b"{\"uefi\": \"sample\"}" (18 bytes)
            //          base64 of "{\"uefi\": \"sample\"}" -> "eyJ1ZWZpIjogInNhbXBsZSJ9"
            let payload = b"{\"uefi\": \"sample\"}";
            let b64 = "eyJ1ZWZpIjogInNhbXBsZSJ9";
            let json = alloc::format!(
                r#"{{
                    "cwcow": {{
                        "vmgs_read_only": false,
                        "require_secure_boot": false,
                        "require_secure_boot_vars": false,
                        "require_bcd_integrity": false,
                        "require_secure_avic": false,
                        "custom_uefi_json": "{b64}"
                    }}
                }}"#
            );
            let policy: ContainerPolicy = from_json(&json).unwrap();
            match policy {
                ContainerPolicy::Cwcow(p) => assert_eq!(p.custom_uefi_json, payload.to_vec()),
            }
        }

        #[test]
        fn deserialize_cwcow_decodes_empty_base64_as_empty_bytes() {
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "custom_uefi_json": ""
                }
            }"#;
            let policy: ContainerPolicy = from_json(json).unwrap();
            match policy {
                ContainerPolicy::Cwcow(p) => assert!(p.custom_uefi_json.is_empty()),
            }
        }

        #[test]
        fn deserialize_cwcow_invalid_base64_is_an_error() {
            // `***` is not valid base64; the adapter must surface a
            // serde error rather than panic.
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "custom_uefi_json": "***"
                }
            }"#;
            let err = from_json(json);
            assert!(err.is_err(), "expected base64 error, got: {err:?}");
        }

        #[test]
        fn json_round_trip_is_byte_identical() {
            // Serialize then re-deserialize a fully populated
            // ContainerPolicy and confirm the value survives.
            //
            // This is the structural enforcement of the
            // serialize/deserialize symmetry that replaces the old
            // "never derive Serialize" hard rule: any field whose
            // serde adapter is asymmetric will break this test.
            let original = ContainerPolicy::Cwcow(CwcowPolicy {
                vmgs_read_only: true,
                require_secure_boot: true,
                require_secure_boot_vars: true,
                require_bcd_integrity: true,
                require_secure_avic: true,
                custom_uefi_json: alloc::vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0x00, 0xFF],
            });
            let json = serde_json::to_string(&original).unwrap();
            let restored: ContainerPolicy = from_json(&json).unwrap();
            assert_eq!(restored, original);

            // Round-trip a default-shaped policy too (exercises the
            // empty-bytes path through the base64 adapter).
            let default_policy = ContainerPolicy::Cwcow(CwcowPolicy::default());
            let json = serde_json::to_string(&default_policy).unwrap();
            let restored: ContainerPolicy = from_json(&json).unwrap();
            assert_eq!(restored, default_policy);
        }

        #[test]
        fn serialize_emits_custom_uefi_json_as_base64_string() {
            // Verify the symmetric adapter emits base64 (not a JSON
            // array of bytes) on the serialize side. Otherwise the
            // round-trip test above would pass with a "JSON array of
            // numbers" round trip — that's not the manifest contract
            // we want.
            let policy = ContainerPolicy::Cwcow(CwcowPolicy {
                custom_uefi_json: alloc::vec![b'A', b'B', b'C'], // base64: "QUJD"
                ..Default::default()
            });
            let json = serde_json::to_string(&policy).unwrap();
            assert!(
                json.contains("\"custom_uefi_json\":\"QUJD\""),
                "expected base64 string in JSON, got: {json}"
            );
        }

        #[test]
        fn deserialize_rejects_unknown_variant() {
            let err = from_json(r#"{"unknown_product":{}}"#);
            assert!(err.is_err());
        }

        #[test]
        fn deserialize_rejects_unknown_field() {
            let err = from_json(
                r#"{"cwcow":{
                    "vmgs_read_only": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "extra": 0
                }}"#,
            );
            assert!(err.is_err(), "expected error, got: {err:?}");
        }

        #[test]
        fn deserialize_rejects_pascal_case_variant() {
            let err = from_json(r#"{"Cwcow":{}}"#);
            assert!(err.is_err(), "expected error, got: {err:?}");
        }
    }
}
