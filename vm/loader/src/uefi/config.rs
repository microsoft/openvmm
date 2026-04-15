// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI specific configuration format and construction.

use bitfield_struct::bitfield;
use core::mem::size_of;
use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

fn align_8(x: usize) -> usize {
    (x + 7) & !7
}

/// A configuration blob builder for passing config information to UEFI.
#[derive(Debug)]
pub struct Blob {
    data: Vec<u8>,
    count: u32,
}

impl Blob {
    /// Creates a new configuration blob with a placeholder StructureCount structure
    pub fn new() -> Self {
        let mut blob = Self {
            data: Vec::new(),
            count: 0,
        };
        blob.add(&StructureCount {
            total_structure_count: 0,
            total_config_blob_size: 0,
        });
        blob
    }

    /// Aligns and appends a sized structure and its appropriate header to the configuration blob.
    pub fn add<T: BlobStructure>(&mut self, data: &T) -> &mut Self {
        self.add_raw(T::STRUCTURE_TYPE, data.as_bytes())
    }

    /// Aligns and appends a null terminated C string and its appropriate header
    /// to the configuration blob.
    ///
    /// If the data is zero-sized, the configuration blob is not updated.
    ///
    /// If the data does not include a null terminator (e.g: because the data
    /// was pulled from a Rust string), a null terminator is appended to the end
    /// of the data.
    pub fn add_cstring(&mut self, structure_type: BlobStructureType, data: &[u8]) -> &mut Self {
        if !data.is_empty() {
            self.add_raw_inner(structure_type, data, !data.ends_with(&[0]))
        } else {
            self
        }
    }

    /// Aligns and appends the raw byte data of a potentially dynamically sized structure
    /// and its appropriate header to the configuration blob.
    pub fn add_raw(&mut self, structure_type: BlobStructureType, data: &[u8]) -> &mut Self {
        self.add_raw_inner(structure_type, data, false)
    }

    fn add_raw_inner(
        &mut self,
        structure_type: BlobStructureType,
        data: &[u8],
        add_null_term: bool,
    ) -> &mut Self {
        // Align up to 8 bytes.
        let aligned_data_len = align_8(data.len() + add_null_term as usize);
        self.data.extend_from_slice(
            Header {
                structure_type: structure_type as u32,
                length: (size_of::<Header>() + aligned_data_len) as u32,
            }
            .as_bytes(),
        );
        self.data.extend_from_slice(data);
        if add_null_term {
            self.data.push(0);
        }
        // Pad with zeroes.
        self.data
            .extend_from_slice(&[0; 7][..aligned_data_len - (data.len() + add_null_term as usize)]);
        self.count += 1;
        self
    }

    /// Returns a serialized binary format of the whole configuration blob. Done by updating the structure count and
    /// returning the complete binary config blob.
    pub fn complete(mut self) -> Vec<u8> {
        let total_config_blob_size = self.data.len() as u32;
        self.data[size_of::<Header>()..size_of::<Header>() + size_of::<StructureCount>()]
            .copy_from_slice(
                StructureCount {
                    total_structure_count: self.count,
                    total_config_blob_size,
                }
                .as_bytes(),
            );
        self.data
    }
}

impl Default for Blob {
    fn default() -> Self {
        Self::new()
    }
}

pub trait BlobStructure: IntoBytes + FromBytes + Immutable + KnownLayout {
    const STRUCTURE_TYPE: BlobStructureType;
}

macro_rules! blobtypes {
    {
        $($name:ident,)*
    } => {
        $(
            impl BlobStructure for $name {
                const STRUCTURE_TYPE: BlobStructureType = BlobStructureType::$name;
            }
        )*
    }
}

blobtypes! {
    StructureCount,
    BiosInformation,
    Entropy,
    BiosGuid,
    Smbios31ProcessorInformation,
    Flags,
    ProcessorInformation,
    MmioRanges,
    NvdimmCount,
    VpciInstanceFilter,
    Gic,
}

/// Config structure types.
#[repr(u32)]
pub enum BlobStructureType {
    StructureCount = 0x00,
    BiosInformation = 0x01,
    Srat = 0x02,
    MemoryMap = 0x03,
    Entropy = 0x04,
    BiosGuid = 0x05,
    SmbiosSystemSerialNumber = 0x06,
    SmbiosBaseSerialNumber = 0x07,
    SmbiosChassisSerialNumber = 0x08,
    SmbiosChassisAssetTag = 0x09,
    SmbiosBiosLockString = 0x0A,
    Smbios31ProcessorInformation = 0x0B,
    SmbiosSocketDesignation = 0x0C,
    SmbiosProcessorManufacturer = 0x0D,
    SmbiosProcessorVersion = 0x0E,
    SmbiosProcessorSerialNumber = 0x0F,
    SmbiosProcessorAssetTag = 0x10,
    SmbiosProcessorPartNumber = 0x11,
    Flags = 0x12,
    ProcessorInformation = 0x13,
    MmioRanges = 0x14,
    Aarch64Mpidr = 0x15,
    AcpiTable = 0x16,
    NvdimmCount = 0x17,
    Madt = 0x18,
    VpciInstanceFilter = 0x19,
    SmbiosSystemManufacturer = 0x1A,
    SmbiosSystemProductName = 0x1B,
    SmbiosSystemVersion = 0x1C,
    SmbiosSystemSkuNumber = 0x1D,
    SmbiosSystemFamily = 0x1E,
    SmbiosMemoryDeviceSerialNumber = 0x1F,
    Slit = 0x20,
    Aspt = 0x21,
    Pptt = 0x22,
    Gic = 0x23,
    Mcfg = 0x24,
    Ssdt = 0x25,
    Hmat = 0x26,
    Iort = 0x27,
}

//
// Config Structures.
//
// NOTE: All config structures _must_ be aligned to 8 bytes, as AARCH64 does not
// support unaligned accesses. For variable length structures, they must be
// padded appropriately to 8 byte boundaries.
//

//
// Common config header.
//
// NOTE: Length is the length of the overall structure in bytes, including the
// header.
//
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Header {
    pub structure_type: u32,
    pub length: u32,
}

//
// NOTE: TotalStructureCount is the count of all structures in the config blob,
// including this structure.
//
// NOTE: TotalConfigBlobSize is in bytes.
//
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct StructureCount {
    pub total_structure_count: u32,
    pub total_config_blob_size: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct BiosInformation {
    pub bios_size_pages: u32,
    // struct {
    //     UINT32 LegacyMemoryMap : 1;
    //     UINT32 Reserved : 31;
    // } Flags;
    pub flags: u32,
}

//
// Memory map range flags beginning with VDev version 5.
//
// VM_MEMORY_RANGE_FLAG_PLATFORM_RESERVED is mapped to EfiReservedMemoryType.
// This means the memory range is reserved and not regular RAM.
//
// VM_MEMORY_RANGE_FLAG_PERSISTENT is mapped to EfiPersistentMemory.
// This means the memory range is byte-addressable and non-volatile, like PMem.
//
// VM_MEMORY_RANGE_FLAG_SPECIAL_PURPOSE is mapped to EfiConventionalMemory.
// This flag instructs the guest to mark the memory with the EFI_MEMORY_SP bit.
//
pub const VM_MEMORY_RANGE_FLAG_PLATFORM_RESERVED: u32 = 0x1;
pub const VM_MEMORY_RANGE_FLAG_PERSISTENT: u32 = 0x2;
pub const VM_MEMORY_RANGE_FLAG_SPECIFIC_PURPOSE: u32 = 0x4;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MemoryRangeV5 {
    pub base_address: u64,
    pub length: u64,
    pub flags: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Entropy(pub [u8; 64]);

impl Default for Entropy {
    fn default() -> Self {
        Entropy([0; 64])
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct BiosGuid(pub Guid);

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Smbios31ProcessorInformation {
    pub processor_id: u64,
    pub external_clock: u16,
    pub max_speed: u16,
    pub current_speed: u16,
    pub processor_characteristics: u16,
    pub processor_family2: u16,
    pub processor_type: u8,
    pub voltage: u8,
    pub status: u8,
    pub processor_upgrade: u8,
    pub reserved: u16,
}

#[bitfield(u64, debug = false)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Flags {
    pub serial_controllers_enabled: bool,
    pub pause_after_boot_failure: bool,
    pub pxe_ip_v6: bool,
    pub debugger_enabled: bool,
    pub load_oemp_table: bool,
    pub tpm_enabled: bool,
    pub hibernate_enabled: bool,

    #[bits(2)]
    pub console: ConsolePort,

    pub memory_attributes_table_enabled: bool,
    pub virtual_battery_enabled: bool,
    pub sgx_memory_enabled: bool,
    pub is_vmbfs_boot: bool,
    pub measure_additional_pcrs: bool,
    pub disable_frontpage: bool,
    pub default_boot_always_attempt: bool,
    pub low_power_s0_idle_enabled: bool,
    pub vpci_boot_enabled: bool,
    pub proc_idle_enabled: bool,
    pub disable_sha384_pcr: bool,
    pub media_present_enabled_by_default: bool,

    #[bits(2)]
    pub memory_protection: MemoryProtection,

    pub enable_imc_when_isolated: bool,
    pub watchdog_enabled: bool,
    pub tpm_locality_regs_enabled: bool,
    pub dhcp6_link_layer_address: bool,
    pub cxl_memory_enabled: bool,
    pub mtrrs_initialized_at_load: bool,
    pub hv_sint_enabled: bool,

    #[bits(34)]
    _reserved: u64,
}

#[derive(Clone, Copy)]
pub enum ConsolePort {
    Default = 0b00,
    Com1 = 0b01,
    Com2 = 0b10,
    None = 0b11,
}

impl ConsolePort {
    const fn from_bits(bits: u64) -> Self {
        match bits {
            0b00 => Self::Default,
            0b01 => Self::Com1,
            0b10 => Self::Com2,
            0b11 => Self::None,
            _ => unreachable!(),
        }
    }

    const fn into_bits(self) -> u64 {
        self as u64
    }
}

pub enum MemoryProtection {
    Disabled = 0b00,
    Default = 0b01,
    Strict = 0b10,
    Relaxed = 0b11,
}

impl MemoryProtection {
    const fn from_bits(bits: u64) -> Self {
        match bits {
            0b00 => Self::Disabled,
            0b01 => Self::Default,
            0b10 => Self::Strict,
            0b11 => Self::Relaxed,
            _ => unreachable!(),
        }
    }

    const fn into_bits(self) -> u64 {
        self as u64
    }
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct ProcessorInformation {
    pub max_processor_count: u32,
    pub processor_count: u32,
    pub processors_per_virtual_socket: u32,
    pub threads_per_processor: u32,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes, Copy, Clone)]
pub struct Mmio {
    pub mmio_page_number_start: u64,
    pub mmio_size_in_pages: u64,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MmioRanges(pub [Mmio; 2]);

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct NvdimmCount {
    pub count: u16,
    pub padding: [u16; 3],
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct VpciInstanceFilter {
    pub instance_guid: Guid,
}

#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Gic {
    pub gic_distributor_base: u64,
    pub gic_redistributors_base: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read<T>(bytes: &[u8]) -> T
    where
        T: FromBytes + Immutable + KnownLayout,
    {
        T::read_from_prefix(bytes)
            .expect("byte slice should always be big enough")
            .0
    }

    fn add_one_dynamic(length: usize) {
        let padded_length = align_8(length);
        let madt = vec![0xCC; length];

        let data = {
            let mut blob = Blob::new();
            blob.add_raw(BlobStructureType::Madt, &madt);
            blob.complete()
        };

        assert_eq!(data.len() % 8, 0);

        let header: Header = read(&data[..]);
        let structure: StructureCount = read(&data[size_of::<Header>()..]);

        let header_exp = Header {
            structure_type: 0x00,
            length: (size_of::<Header>() + size_of::<StructureCount>()) as u32,
        };
        let structure_exp = StructureCount {
            total_structure_count: 2,
            total_config_blob_size: (2 * size_of::<Header>()
                + size_of::<StructureCount>()
                + padded_length) as u32,
        };

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(
            structure.total_structure_count,
            structure_exp.total_structure_count
        );
        assert_eq!(
            structure.total_config_blob_size,
            structure_exp.total_config_blob_size
        );

        let header: Header = read(&data[size_of::<Header>() + size_of::<StructureCount>()..]);
        let structure = &data[2 * size_of::<Header>() + size_of::<StructureCount>()..][..length];

        let header_exp = Header {
            structure_type: 0x18,
            length: (size_of::<Header>() + padded_length) as u32,
        };
        let structure_exp = &madt[..];

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(structure, structure_exp);
    }

    #[test]
    fn add_none() {
        let data = {
            let blob = Blob::new();
            blob.complete()
        };

        assert_eq!(data.len() % 8, 0);

        let header: Header = read(&data[..]);
        let structure: StructureCount = read(&data[size_of::<Header>()..]);

        let header_exp = Header {
            structure_type: 0x00,
            length: (size_of::<Header>() + size_of::<StructureCount>()) as u32,
        };
        let structure_exp = StructureCount {
            total_structure_count: 1,
            total_config_blob_size: (size_of::<Header>() + size_of::<StructureCount>()) as u32,
        };

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(
            structure.total_structure_count,
            structure_exp.total_structure_count
        );
        assert_eq!(
            structure.total_config_blob_size,
            structure_exp.total_config_blob_size
        );
    }

    #[test]
    fn add_one_fixed() {
        let biosinfo = BiosInformation {
            bios_size_pages: 12345678,
            flags: 1 << 31,
        };

        let data = {
            let mut blob = Blob::new();
            blob.add(&BiosInformation {
                bios_size_pages: 12345678,
                flags: 1 << 31,
            });
            blob.complete()
        };

        assert_eq!(data.len() % 8, 0);

        let header: Header = read(&data[..]);
        let structure: StructureCount = read(&data[size_of::<Header>()..]);

        let header_exp = Header {
            structure_type: 0x00,
            length: (size_of::<Header>() + size_of::<StructureCount>()) as u32,
        };
        let structure_exp = StructureCount {
            total_structure_count: 2,
            total_config_blob_size: (2 * size_of::<Header>()
                + size_of::<StructureCount>()
                + size_of::<BiosInformation>()) as u32,
        };

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(
            structure.total_structure_count,
            structure_exp.total_structure_count
        );
        assert_eq!(
            structure.total_config_blob_size,
            structure_exp.total_config_blob_size
        );

        let header: Header = read(&data[size_of::<Header>() + size_of::<StructureCount>()..]);
        let structure: BiosInformation =
            read(&data[2 * size_of::<Header>() + size_of::<StructureCount>()..]);

        let header_exp = Header {
            structure_type: 0x01,
            length: (size_of::<Header>() + size_of::<BiosInformation>()) as u32,
        };

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(structure.bios_size_pages, biosinfo.bios_size_pages);
        assert_eq!(structure.flags, biosinfo.flags);
    }

    #[test]
    fn add_one_dynamic_misaligned() {
        add_one_dynamic(43);
    }

    #[test]
    fn add_one_dynamic_aligned() {
        add_one_dynamic(40);
    }

    #[test]
    fn add_two() {
        const LENGTH: usize = 93;
        const PADDED_LENGTH: usize = 96;
        let madt = vec![0xCC; LENGTH];
        let procinfo = ProcessorInformation {
            max_processor_count: 4,
            processor_count: 3,
            processors_per_virtual_socket: 2,
            threads_per_processor: 1,
        };

        let data = {
            let mut blob = Blob::new();
            blob.add_raw(BlobStructureType::Madt, &madt).add(&procinfo);
            blob.complete()
        };

        assert_eq!(data.len() % 8, 0);

        let header: Header = read(&data[..]);
        let structure: StructureCount = read(&data[size_of::<Header>()..]);

        let header_exp = Header {
            structure_type: 0x00,
            length: (size_of::<Header>() + size_of::<StructureCount>()) as u32,
        };
        let structure_exp = StructureCount {
            total_structure_count: 3,
            total_config_blob_size: (3 * size_of::<Header>()
                + size_of::<StructureCount>()
                + PADDED_LENGTH
                + size_of::<ProcessorInformation>()) as u32,
        };

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(
            structure.total_structure_count,
            structure_exp.total_structure_count
        );
        assert_eq!(
            structure.total_config_blob_size,
            structure_exp.total_config_blob_size
        );

        let header: Header = read(&data[size_of::<Header>() + size_of::<StructureCount>()..]);
        let structure = &data[2 * size_of::<Header>() + size_of::<StructureCount>()..][..LENGTH];
        let padding = &data[2 * size_of::<Header>() + size_of::<StructureCount>() + LENGTH..]
            [..PADDED_LENGTH - LENGTH];

        let header_exp = Header {
            structure_type: 0x18,
            length: (size_of::<Header>() + PADDED_LENGTH) as u32,
        };
        let structure_exp = &madt[..];

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(structure.as_bytes(), structure_exp.as_bytes());
        assert_eq!(padding, &[0; PADDED_LENGTH - LENGTH]);

        let header: Header =
            read(&data[2 * size_of::<Header>() + size_of::<StructureCount>() + PADDED_LENGTH..]);
        let structure: ProcessorInformation =
            read(&data[3 * size_of::<Header>() + size_of::<StructureCount>() + PADDED_LENGTH..]);

        let header_exp = Header {
            structure_type: 0x13,
            length: (size_of::<Header>() + size_of::<ProcessorInformation>()) as u32,
        };

        assert_eq!(header.structure_type, header_exp.structure_type);
        assert_eq!(header.length, header_exp.length);
        assert_eq!(structure.max_processor_count, procinfo.max_processor_count);
        assert_eq!(structure.processor_count, procinfo.processor_count);
        assert_eq!(
            structure.processors_per_virtual_socket,
            procinfo.processors_per_virtual_socket
        );
        assert_eq!(
            structure.threads_per_processor,
            procinfo.threads_per_processor
        );
    }
}
