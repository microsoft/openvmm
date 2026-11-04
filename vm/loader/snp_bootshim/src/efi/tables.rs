// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The UEFI tables and protocols that the shim gives to the EFI stub.
//!
//! The tables live in one [`Efi`] value at a fixed address. [`Efi::build`]
//! fills them in from that address and the addresses of the service
//! functions, so host tests can build them too.

use guid::Guid;
use memory_range::MemoryRange;
use uefi_specs::uefi::boot::ACPI_20_TABLE_GUID;
use uefi_specs::uefi::boot::EFI_2_70_SYSTEM_TABLE_REVISION;
use uefi_specs::uefi::boot::EFI_BOOT_SERVICES_COUNT;
use uefi_specs::uefi::boot::EFI_BOOT_SERVICES_SIGNATURE;
use uefi_specs::uefi::boot::EFI_CC_BLOB_GUID;
use uefi_specs::uefi::boot::EFI_LOADED_IMAGE_PROTOCOL_REVISION;
use uefi_specs::uefi::boot::EFI_RT_PROPERTIES_TABLE_GUID;
use uefi_specs::uefi::boot::EFI_RUNTIME_SERVICES_COUNT;
use uefi_specs::uefi::boot::EFI_RUNTIME_SERVICES_SIGNATURE;
use uefi_specs::uefi::boot::EFI_SYSTEM_TABLE_SIGNATURE;
use uefi_specs::uefi::boot::EfiConfigurationTable;
use uefi_specs::uefi::boot::EfiDevicePathProtocol;
use uefi_specs::uefi::boot::EfiDeviceType;
use uefi_specs::uefi::boot::EfiEndDeviceSubType;
use uefi_specs::uefi::boot::EfiLoadedImageProtocol;
use uefi_specs::uefi::boot::EfiMediaDeviceSubType;
use uefi_specs::uefi::boot::EfiMemoryType;
use uefi_specs::uefi::boot::EfiRtPropertiesTable;
use uefi_specs::uefi::boot::EfiSimpleTextInputProtocol;
use uefi_specs::uefi::boot::EfiSimpleTextOutputMode;
use uefi_specs::uefi::boot::EfiSimpleTextOutputProtocol;
use uefi_specs::uefi::boot::EfiSystemTable;
use uefi_specs::uefi::boot::EfiTableHeader;
use uefi_specs::uefi::boot::LINUX_EFI_INITRD_MEDIA_GUID;
use uefi_specs::uefi::boot::SMBIOS3_TABLE_GUID;
use uefi_specs::uefi::common::EfiStatus;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// The room for configuration tables, including those that the stub installs.
pub(super) const MAX_CONFIGURATION_TABLES: usize = 32;
/// A vendor media device-path node: the node header, then a GUID.
pub(super) const VENDOR_NODE_SIZE: usize = size_of::<EfiDevicePathProtocol>() + size_of::<Guid>();
/// An end-of-path node is only a node header.
const END_NODE_SIZE: usize = size_of::<EfiDevicePathProtocol>();

/// Computes the CRC-32 that UEFI uses for table headers.
pub(super) fn crc32(data: &[u8]) -> u32 {
    crc32_parts(&[data])
}

/// Computes the CRC-32 of the concatenation of `parts`.
fn crc32_parts(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for &byte in parts.iter().flat_map(|part| part.iter()) {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// EFI_BOOT_SERVICES or EFI_RUNTIME_SERVICES: a table header, then function
/// pointers (UEFI spec 4.4, 4.5).
#[repr(C)]
#[derive(FromBytes)]
pub(super) struct ServiceTable<const N: usize> {
    pub header: EfiTableHeader,
    pub functions: [u64; N],
}

impl<const N: usize> ServiceTable<N> {
    fn new(signature: u64, functions: [u64; N]) -> Self {
        let mut table = Self {
            header: EfiTableHeader {
                signature,
                revision: EFI_2_70_SYSTEM_TABLE_REVISION,
                header_size: size_of::<Self>() as u32,
                crc32: 0,
                reserved: 0,
            },
            functions,
        };
        table.header.crc32 = crc32_parts(&[table.header.as_bytes(), table.functions.as_bytes()]);
        table
    }
}

/// A vendor media node with `LINUX_EFI_INITRD_MEDIA_GUID`, then an end node.
/// The Linux EFI stub finds its initrd through this device path.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoBytes, FromBytes, Immutable, KnownLayout)]
pub(super) struct InitrdDevicePath {
    pub vendor: [u8; VENDOR_NODE_SIZE],
    pub end: [u8; END_NODE_SIZE],
}

impl InitrdDevicePath {
    pub fn new() -> Self {
        let mut vendor = [0; VENDOR_NODE_SIZE];
        vendor[0] = EfiDeviceType::MEDIA.0;
        vendor[1] = EfiMediaDeviceSubType::VENDOR.0;
        vendor[2..4].copy_from_slice(&(VENDOR_NODE_SIZE as u16).to_le_bytes());
        vendor[4..].copy_from_slice(LINUX_EFI_INITRD_MEDIA_GUID.as_bytes());
        Self {
            vendor,
            end: [
                EfiDeviceType::END.0,
                EfiEndDeviceSubType::ENTIRE.0,
                END_NODE_SIZE as u8,
                0,
            ],
        }
    }
}

/// Every table and protocol that the shim gives to the EFI stub. The shim's
/// image is runtime-services memory, so these stay valid after
/// ExitBootServices.
#[repr(C)]
#[derive(FromBytes)]
pub(super) struct Efi {
    pub system_table: EfiSystemTable,
    pub boot_services: ServiceTable<EFI_BOOT_SERVICES_COUNT>,
    pub runtime_services: ServiceTable<EFI_RUNTIME_SERVICES_COUNT>,
    pub configuration_tables: [EfiConfigurationTable; MAX_CONFIGURATION_TABLES],
    pub loaded_image: EfiLoadedImageProtocol,
    /// EFI_LOAD_FILE2_PROTOCOL, which has one function (UEFI spec 13.2).
    pub load_file2: u64,
    pub initrd_path: InitrdDevicePath,
    pub con_out: EfiSimpleTextOutputProtocol,
    pub con_out_mode: EfiSimpleTextOutputMode,
    pub con_in: EfiSimpleTextInputProtocol,
    pub firmware_vendor: [u16; 8],
    pub rt_properties: EfiRtPropertiesTable,
}

/// Returns the address of a field of an [`Efi`] at address `base`.
macro_rules! field_address {
    ($base:expr, $field:ident) => {
        $base + core::mem::offset_of!(Efi, $field) as u64
    };
}
#[cfg(minimal_rt)]
pub(super) use field_address;

/// What the stub needs to boot Linux.
#[cfg_attr(
    all(test, not(minimal_rt)),
    expect(dead_code, reason = "only the guest enters the stub")
)]
pub(crate) struct Boot {
    pub kernel: MemoryRange,
    pub kernel_entry: u64,
    pub initrd: Option<MemoryRange>,
    pub initrd_size: u64,
    pub cmdline_gpa: u64,
    pub cmdline_size: u64,
    pub rsdp_gpa: u64,
    pub smbios_gpa: u64,
    pub cc_blob_gpa: u64,
    /// The top of the stack for the stub. It must be 16-byte aligned.
    pub stack_top: u64,
}

/// The addresses of the shim's service functions.
pub(super) struct Handlers {
    /// The boot-services table, by index.
    pub boot_services: [u64; EFI_BOOT_SERVICES_COUNT],
    /// A function that returns `EFI_UNSUPPORTED`.
    pub unsupported: u64,
    /// A function that returns `EFI_SUCCESS`.
    pub success: u64,
    pub read_key_stroke: u64,
    pub load_file: u64,
}

impl Efi {
    /// Fills in every table and protocol. `base` is the address of `self`.
    pub fn build(&mut self, base: u64, boot: &Boot, handlers: &Handlers) {
        for (index, c) in "OpenVMM\0".encode_utf16().enumerate() {
            self.firmware_vendor[index] = c;
        }

        let (success, unsupported) = (handlers.success, handlers.unsupported);
        self.con_out = EfiSimpleTextOutputProtocol {
            reset: success,
            output_string: success,
            test_string: success,
            query_mode: unsupported,
            set_mode: unsupported,
            set_attribute: success,
            clear_screen: success,
            set_cursor_position: unsupported,
            enable_cursor: unsupported,
            mode: field_address!(base, con_out_mode),
        };
        self.con_in = EfiSimpleTextInputProtocol {
            reset: success,
            read_key_stroke: handlers.read_key_stroke,
            wait_for_key: 0,
        };

        self.boot_services = ServiceTable::new(EFI_BOOT_SERVICES_SIGNATURE, handlers.boot_services);
        self.runtime_services = ServiceTable::new(
            EFI_RUNTIME_SERVICES_SIGNATURE,
            [unsupported; EFI_RUNTIME_SERVICES_COUNT],
        );

        self.rt_properties = EfiRtPropertiesTable::NONE_SUPPORTED;
        let tables = [
            (ACPI_20_TABLE_GUID, boot.rsdp_gpa),
            (SMBIOS3_TABLE_GUID, boot.smbios_gpa),
            (EFI_CC_BLOB_GUID, boot.cc_blob_gpa),
            (
                EFI_RT_PROPERTIES_TABLE_GUID,
                field_address!(base, rt_properties),
            ),
        ];
        for (entry, (vendor_guid, vendor_table)) in self.configuration_tables.iter_mut().zip(tables)
        {
            *entry = EfiConfigurationTable {
                vendor_guid,
                vendor_table,
            };
        }

        self.initrd_path = InitrdDevicePath::new();
        self.load_file2 = handlers.load_file;

        self.loaded_image = EfiLoadedImageProtocol {
            revision: EFI_LOADED_IMAGE_PROTOCOL_REVISION,
            system_table: field_address!(base, system_table),
            load_options_size: boot.cmdline_size as u32,
            load_options: boot.cmdline_gpa,
            image_base: boot.kernel.start(),
            image_size: boot.kernel.len(),
            image_code_type: EfiMemoryType::EFI_LOADER_CODE,
            image_data_type: EfiMemoryType::EFI_LOADER_DATA,
            unload: unsupported,
            ..FromZeros::new_zeroed()
        };

        let con_out = field_address!(base, con_out);
        let con_in = field_address!(base, con_in);
        self.system_table = EfiSystemTable {
            signature: EFI_SYSTEM_TABLE_SIGNATURE,
            revision: EFI_2_70_SYSTEM_TABLE_REVISION,
            header_size: size_of::<EfiSystemTable>() as u32,
            firmware_vendor: field_address!(base, firmware_vendor),
            firmware_revision: 1,
            console_in_handle: con_in,
            con_in,
            console_out_handle: con_out,
            con_out,
            standard_error_handle: con_out,
            std_err: con_out,
            runtime_services: field_address!(base, runtime_services),
            boot_services: field_address!(base, boot_services),
            number_of_table_entries: tables.len() as u64,
            configuration_table: field_address!(base, configuration_tables),
            ..Default::default()
        };
        self.update_system_table_crc();
    }

    fn update_system_table_crc(&mut self) {
        let table = &mut self.system_table;
        table.crc32 = 0;
        table.crc32 = crc32(table.as_bytes());
    }

    /// Adds, replaces, or removes a configuration table, as
    /// `InstallConfigurationTable` does. A `table` of zero removes the entry.
    pub fn install_configuration_table(&mut self, guid: Guid, table: u64) -> EfiStatus {
        let count = self.system_table.number_of_table_entries as usize;
        let tables = &mut self.configuration_tables;
        let index = tables[..count]
            .iter()
            .position(|entry| entry.vendor_guid == guid);
        match (index, table == 0) {
            (Some(index), false) => tables[index].vendor_table = table,
            (Some(index), true) => {
                tables[index..count].rotate_left(1);
                self.system_table.number_of_table_entries -= 1;
            }
            (None, false) => {
                let Some(entry) = tables.get_mut(count) else {
                    return EfiStatus::OUT_OF_RESOURCES;
                };
                *entry = EfiConfigurationTable {
                    vendor_guid: guid,
                    vendor_table: table,
                };
                self.system_table.number_of_table_entries += 1;
            }
            (None, true) => return EfiStatus::NOT_FOUND,
        }
        self.update_system_table_crc();
        EfiStatus::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const MB: u64 = 1 << 20;
    const BASE: u64 = 0x10_0000;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32_parts(&[b"1234", b"", b"56789"]), 0xcbf4_3926);
    }

    fn test_guid(data1: u32) -> Guid {
        Guid {
            data1,
            ..Guid::ZERO
        }
    }

    fn next() -> u32 {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn boot() -> Boot {
        Boot {
            kernel: MemoryRange::new(MB..5 * MB),
            kernel_entry: MB + 0x1000,
            initrd: None,
            initrd_size: 0,
            cmdline_gpa: 6 * MB,
            cmdline_size: 30,
            rsdp_gpa: 0x14000,
            smbios_gpa: 0x13000,
            cc_blob_gpa: 0x12000,
            stack_top: 8 * MB,
        }
    }

    fn handlers() -> Handlers {
        let mut boot_services = [0xdead; EFI_BOOT_SERVICES_COUNT];
        boot_services[2] = 0x1234;
        Handlers {
            boot_services,
            unsupported: 0xdead,
            success: 0x5000,
            read_key_stroke: 0x6000,
            load_file: 0x7000,
        }
    }

    fn built() -> Efi {
        let mut efi = Efi::new_zeroed();
        efi.build(BASE, &boot(), &handlers());
        efi
    }

    fn valid_crc(header_bytes: &[u8], crc_offset: usize) -> bool {
        let mut bytes = header_bytes.to_vec();
        let crc = u32::from_le_bytes(bytes[crc_offset..crc_offset + 4].try_into().unwrap());
        bytes[crc_offset..crc_offset + 4].fill(0);
        crc32(&bytes) == crc
    }

    #[test]
    fn builds_the_system_table() {
        let efi = built();
        let st = &efi.system_table;
        assert_eq!(st.signature, EFI_SYSTEM_TABLE_SIGNATURE);
        assert!(valid_crc(st.as_bytes(), 16));
        assert_eq!(
            st.boot_services,
            BASE + core::mem::offset_of!(Efi, boot_services) as u64
        );
        assert_eq!(
            st.configuration_table,
            BASE + core::mem::offset_of!(Efi, configuration_tables) as u64
        );
        assert_eq!(st.number_of_table_entries, 4);
        assert_eq!(
            &efi.firmware_vendor[..8],
            &"OpenVMM\0".encode_utf16().collect::<Vec<_>>()[..]
        );
        assert_eq!(efi.loaded_image.load_options, 6 * MB);
        assert_eq!(efi.loaded_image.image_base, MB);
        assert_eq!(efi.loaded_image.image_size, 4 * MB);
        assert_eq!(efi.load_file2, 0x7000);
    }

    #[test]
    fn builds_service_tables_with_valid_crcs() {
        let efi = built();
        let bs = &efi.boot_services;
        assert_eq!(bs.header.signature, EFI_BOOT_SERVICES_SIGNATURE);
        assert_eq!(bs.functions[2], 0x1234);
        let bytes = [bs.header.as_bytes(), bs.functions.as_bytes()].concat();
        assert!(valid_crc(&bytes, 16));
        let rs = &efi.runtime_services;
        assert!(rs.functions.iter().all(|&f| f == 0xdead));
        let bytes = [rs.header.as_bytes(), rs.functions.as_bytes()].concat();
        assert!(valid_crc(&bytes, 16));
    }

    #[test]
    fn builds_the_initrd_device_path() {
        let path = InitrdDevicePath::new();
        assert_eq!(&path.vendor[..4], &[4, 3, 20, 0]);
        assert_eq!(&path.vendor[4..], LINUX_EFI_INITRD_MEDIA_GUID.as_bytes());
        assert_eq!(path.end, [0x7f, 0xff, 4, 0]);
    }

    #[test]
    fn installs_replaces_and_removes_configuration_tables() {
        let mut efi = built();
        let guid = test_guid(next());
        assert_eq!(
            efi.install_configuration_table(guid, 0),
            EfiStatus::NOT_FOUND
        );
        assert_eq!(
            efi.install_configuration_table(guid, 0x1000),
            EfiStatus::SUCCESS
        );
        assert_eq!(efi.system_table.number_of_table_entries, 5);
        assert_eq!(efi.configuration_tables[4].vendor_table, 0x1000);
        assert_eq!(
            efi.install_configuration_table(guid, 0x2000),
            EfiStatus::SUCCESS
        );
        assert_eq!(efi.system_table.number_of_table_entries, 5);
        assert_eq!(efi.configuration_tables[4].vendor_table, 0x2000);
        assert!(valid_crc(efi.system_table.as_bytes(), 16));
        // Removing the ACPI table moves the later tables down.
        assert_eq!(
            efi.install_configuration_table(ACPI_20_TABLE_GUID, 0),
            EfiStatus::SUCCESS
        );
        assert_eq!(efi.system_table.number_of_table_entries, 4);
        assert_eq!(efi.configuration_tables[0].vendor_guid, SMBIOS3_TABLE_GUID);
        assert_eq!(efi.configuration_tables[3].vendor_guid, guid);
    }

    #[test]
    fn configuration_table_capacity_is_limited() {
        let mut efi = built();
        for _ in 4..MAX_CONFIGURATION_TABLES {
            assert_eq!(
                efi.install_configuration_table(test_guid(next()), 1),
                EfiStatus::SUCCESS
            );
        }
        assert_eq!(
            efi.install_configuration_table(test_guid(next()), 1),
            EfiStatus::OUT_OF_RESOURCES
        );
    }
}
