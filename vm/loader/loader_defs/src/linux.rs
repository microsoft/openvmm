// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux loader definitions.
//!
//! These structures are defined in the Linux kernel and can be found in the kernel docs under
//! [`The Linux/x86 Boot Protocol`](https://www.kernel.org/doc/html/latest/x86/boot.html).

#![expect(missing_docs)]

use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[allow(non_camel_case_types)]
mod packed_nums {
    pub type u16_ne = zerocopy::U16<zerocopy::NativeEndian>;
    pub type u32_ne = zerocopy::U32<zerocopy::NativeEndian>;
    pub type u64_ne = zerocopy::U64<zerocopy::NativeEndian>;
}
use self::packed_nums::*;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct apm_bios_info {
    pub version: u16,
    pub cseg: u16,
    pub offset: u32,
    pub cseg_16: u16,
    pub dseg: u16,
    pub flags: u16,
    pub cseg_len: u16,
    pub cseg_16_len: u16,
    pub dseg_len: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct screen_info {
    pub orig_x: u8,
    pub orig_y: u8,
    pub ext_mem_k: u16,
    pub orig_video_page: u16,
    pub orig_video_mode: u8,
    pub orig_video_cols: u8,
    pub flags: u8,
    pub unused2: u8,
    pub orig_video_ega_bx: u16,
    pub unused3: u16,
    pub orig_video_lines: u8,
    pub orig_video_is_vga: u8,
    pub orig_video_points: u16,
    pub lfb_width: u16,
    pub lfb_height: u16,
    pub lfb_depth: u16,
    pub lfb_base: u32,
    pub lfb_size: u32,
    pub cl_magic: u16,
    pub cl_offset: u16,
    pub lfb_linelength: u16,
    pub red_size: u8,
    pub red_pos: u8,
    pub green_size: u8,
    pub green_pos: u8,
    pub blue_size: u8,
    pub blue_pos: u8,
    pub rsvd_size: u8,
    pub rsvd_pos: u8,
    pub vesapm_seg: u16,
    pub vesapm_off: u16,
    pub pages: u16,
    pub vesa_attributes: u16,
    pub capabilities: u32_ne,
    pub _reserved: [u8; 6],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct sys_desc_table {
    pub length: u16,
    pub table: [u8; 14],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct olpc_ofw_header {
    pub ofw_magic: u32,
    pub ofw_version: u32,
    pub cif_handler: u32,
    pub irq_desc_table: u32,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct edid_info {
    pub dummy: [u8; 128],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct efi_info {
    pub efi_loader_signature: u32,
    pub efi_systab: u32,
    pub efi_memdesc_size: u32,
    pub efi_memdesc_version: u32,
    pub efi_memmap: u32,
    pub efi_memmap_size: u32,
    pub efi_systab_hi: u32,
    pub efi_memmap_hi: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct setup_header {
    pub setup_sects: u8,
    pub root_flags: u16_ne,
    pub syssize: u32_ne,
    pub ram_size: u16_ne,
    pub vid_mode: u16_ne,
    pub root_dev: u16_ne,
    pub boot_flag: u16_ne,
    pub jump: u16_ne,
    pub header: u32_ne,
    pub version: u16_ne,
    pub realmode_swtch: u32_ne,
    pub start_sys: u16_ne,
    pub kernel_version: u16_ne,
    pub type_of_loader: u8,
    pub loadflags: u8,
    pub setup_move_size: u16_ne,
    pub code32_start: u32_ne,
    pub ramdisk_image: u32_ne,
    pub ramdisk_size: u32_ne,
    pub bootsect_kludge: u32_ne,
    pub heap_end_ptr: u16_ne,
    pub ext_loader_ver: u8,
    pub ext_loader_type: u8,
    pub cmd_line_ptr: u32_ne,
    pub initrd_addr_max: u32_ne,
    pub kernel_alignment: u32_ne,
    pub relocatable_kernel: u8,
    pub min_alignment: u8,
    pub xloadflags: u16_ne,
    pub cmdline_size: u32_ne,
    pub hardware_subarch: u32_ne,
    pub hardware_subarch_data: u64_ne,
    pub payload_offset: u32_ne,
    pub payload_length: u32_ne,
    pub setup_data: u64_ne,
    pub pref_address: u64_ne,
    pub init_size: u32_ne,
    pub handover_offset: u32_ne,
}

// TODO: zerocopy doesn't support const new methods, so define them as u32 for now. (https://github.com/microsoft/openvmm/issues/759)
pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;
pub const E820_ACPI: u32 = 3;
pub const E820_NVS: u32 = 4;
pub const E820_UNUSABLE: u32 = 5;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct e820entry {
    pub addr: u64_ne,
    pub size: u64_ne,
    pub typ: u32_ne,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct edd_info {
    pub device: u8,
    pub version: u8,
    pub interface_support: u16,
    pub legacy_max_cylinder: u16,
    pub legacy_max_head: u8,
    pub legacy_sectors_per_track: u8,
    pub params: edd_device_params,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct edd_device_params {
    pub length: u16,
    pub info_flags: u16,
    pub num_default_cylinders: u32_ne,
    pub num_default_heads: u32_ne,
    pub sectors_per_track: u32_ne,
    pub number_of_sectors: u64_ne,
    pub bytes_per_sector: u16,
    pub dpte_ptr: u32_ne,
    pub key: u16,
    pub device_path_info_length: u8,
    pub reserved2: u8,
    pub reserved3: u16,
    pub host_bus_type: [u8; 4],
    pub interface_type: [u8; 8],
    pub interface_path: [u8; 8],
    pub device_path: [u8; 16],
    pub reserved4: u8,
    pub checksum: u8,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ist_info {
    pub signature: u32,
    pub command: u32,
    pub event: u32,
    pub perf_level: u32,
}

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct boot_params {
    pub screen_info: screen_info,
    pub apm_bios_info: apm_bios_info,
    pub _pad2: [u8; 4],
    pub tboot_addr: u64,
    pub ist_info: ist_info,
    pub _pad3: [u8; 16],
    pub hd0_info: [u8; 16],
    pub hd1_info: [u8; 16],
    pub sys_desc_table: sys_desc_table,
    pub olpc_ofw_header: olpc_ofw_header,
    pub ext_ramdisk_image: u32,
    pub ext_ramdisk_size: u32,
    pub ext_cmd_line_ptr: u32,
    pub _pad4: [[u8; 29]; 4],
    pub edid_info: edid_info,
    pub efi_info: efi_info,
    pub alt_mem_k: u32,
    pub scratch: u32,
    pub e820_entries: u8,
    pub eddbuf_entries: u8,
    pub edd_mbr_sig_buf_entries: u8,
    pub kbd_status: u8,
    pub _pad5: [u8; 3],
    pub sentinel: u8,
    pub _pad6: [u8; 1],
    pub hdr: setup_header,
    pub _pad7: [u8; 40],
    pub edd_mbr_sig_buffer: [u32; 16],
    pub e820_map: [e820entry; 128],
    pub _pad8: [u8; 48],
    pub eddbuf: [edd_info; 6],
    pub _pad9: [[u8; 23]; 12],
}

impl Default for boot_params {
    fn default() -> Self {
        FromZeros::new_zeroed()
    }
}

const_assert_eq!(size_of::<boot_params>(), 4096);

// This must be aligned so that it doesn't straddle a page boundary.
#[repr(C, align(16))]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct setup_data {
    pub next: u64,
    pub ty: u32,
    pub len: u32,
}

pub const SETUP_E820_EXT: u32 = 1;
pub const SETUP_DTB: u32 = 2;
pub const SETUP_CC_BLOB: u32 = 7;

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct cc_blob_sev_info {
    pub magic: u32,
    pub version: u16,
    pub _reserved: u16,
    pub secrets_phys: u64,
    pub secrets_len: u32,
    pub _rsvd1: u32,
    pub cpuid_phys: u64,
    pub cpuid_len: u32,
    pub _rsvd2: u32,
}

pub const CC_BLOB_SEV_INFO_MAGIC: u32 = 0x45444d41;

#[repr(C, align(16))]
#[derive(Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct cc_setup_data {
    pub header: setup_data,
    pub cc_blob_address: u32,
    pub _padding: [u32; 3],
}
