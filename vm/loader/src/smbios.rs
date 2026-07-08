// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arch-neutral SMBIOS 3.x (DMI) table builder.
//!
//! In firmware-less Linux direct boot there is no UEFI/PCAT firmware to
//! synthesize SMBIOS tables, so the loader must build them itself. This module
//! builds a SMBIOS 3.1 entry point (`_SM3_`) and a minimal structure table
//! (Type 0 BIOS, Type 1 System, Type 127 End-of-table). The caller decides
//! where the structure table lives in guest memory and how the entry point is
//! delivered to the guest (x86 F-segment scan vs. aarch64 EFI configuration
//! table).

mod spec;

use spec::Smbios30EntryPoint;
use spec::SmbiosType0;
use spec::SmbiosType1;
use spec::SmbiosType127;
use zerocopy::IntoBytes;
use zerocopy::LE;
use zerocopy::U16;

/// BIOS Information (SMBIOS Type 0).
#[derive(Debug, Copy, Clone)]
pub struct SmbiosBiosInfo<'a> {
    /// BIOS vendor string.
    pub vendor: &'a str,
    /// BIOS version string.
    pub version: &'a str,
    /// BIOS release date string.
    pub release_date: &'a str,
    /// System BIOS Major Release.
    pub major: u8,
    /// System BIOS Minor Release.
    pub minor: u8,
}

/// System Information (SMBIOS Type 1).
#[derive(Debug, Copy, Clone)]
pub struct SmbiosSystemInfo<'a> {
    /// System manufacturer string.
    pub manufacturer: &'a str,
    /// System product name string.
    pub product_name: &'a str,
    /// System version string.
    pub version: &'a str,
    /// System serial number string.
    pub serial_number: &'a str,
    /// System SKU number string.
    pub sku_number: &'a str,
    /// System family string.
    pub family: &'a str,
    /// System UUID, as raw EFI GUID bytes (mixed-endian, as stored by the UEFI
    /// path).
    pub uuid: [u8; 16],
}

/// Aggregate of the SMBIOS structures to build. The caller supplies all of the
/// identity strings and the system UUID.
#[derive(Debug, Copy, Clone)]
pub struct SmbiosTables<'a> {
    /// Type 0 BIOS Information.
    pub bios: SmbiosBiosInfo<'a>,
    /// Type 1 System Information.
    pub system: SmbiosSystemInfo<'a>,
}

/// Size in bytes of the SMBIOS 3.1 entry point (`_SM3_`). Callers that place
/// the entry point and structure table separately (e.g. the aarch64 EFI
/// configuration-table path) use this to reserve space for the entry point
/// before knowing the structure table's address.
pub const ENTRY_POINT_SIZE: usize = size_of::<Smbios30EntryPoint>();

/// The built SMBIOS blobs, ready to be placed in guest memory.
#[derive(Debug, Clone)]
pub struct BuiltSmbios {
    /// The 24-byte `_SM3_` entry point.
    pub entry_point: Vec<u8>,
    /// The structure table (Type 0, Type 1, Type 127, plus string sets).
    pub structure_table: Vec<u8>,
}

/// Accumulates the strings referenced by a single SMBIOS structure and emits
/// the trailing string set.
#[derive(Default)]
struct StringSet {
    strings: Vec<String>,
}

impl StringSet {
    /// Adds a string and returns its 1-based index, or 0 ("no string") for an
    /// empty string.
    ///
    /// SMBIOS strings are NUL-terminated, so a string set cannot contain an
    /// interior NUL. The string is truncated at the first NUL (if any) so that
    /// caller-supplied data cannot corrupt the NUL-separated string set framing
    /// (bytes after an interior NUL would otherwise be parsed as a separate
    /// string, shifting every subsequent string index).
    fn add(&mut self, s: &str) -> u8 {
        let s = s.split('\0').next().unwrap_or("");
        if s.is_empty() {
            return 0;
        }
        self.strings.push(s.to_string());
        self.strings.len().try_into().unwrap()
    }

    /// Appends the NUL-terminated string set to `out`, ending with the extra
    /// NUL that terminates the structure. A structure with no strings emits two
    /// NUL bytes.
    fn write_to(&self, out: &mut Vec<u8>) {
        if self.strings.is_empty() {
            out.extend_from_slice(&[0, 0]);
            return;
        }
        for s in &self.strings {
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        out.push(0);
    }
}

/// Builds the SMBIOS entry point and structure table.
///
/// `table_gpa` is the guest physical address at which the returned
/// `structure_table` will be placed; it is written into the entry point's
/// `table_addr` field.
pub fn build(tables: &SmbiosTables<'_>, table_gpa: u64) -> BuiltSmbios {
    let mut structure_table = Vec::new();

    // Each structure needs a handle that is unique within the table; hand them
    // out sequentially. The values are arbitrary.
    let mut next_handle = 0u16;
    let mut handle = || {
        let h = next_handle;
        next_handle += 1;
        U16::<LE>::new(h)
    };

    // Type 0 — BIOS Information.
    {
        let mut strings = StringSet::default();
        let vendor = strings.add(tables.bios.vendor);
        let bios_version = strings.add(tables.bios.version);
        let bios_release_date = strings.add(tables.bios.release_date);
        let t0 = SmbiosType0 {
            typ: 0,
            length: size_of::<SmbiosType0>() as u8,
            handle: handle(),
            vendor,
            bios_version,
            bios_segment: 0.into(),
            bios_release_date,
            bios_size: 0,
            characteristics: spec::BIOS_CHARACTERISTICS_PCI_SUPPORTED.into(),
            characteristics_ext: [
                spec::BIOS_CHARACTERISTICS_EXT1_ACPI,
                spec::BIOS_CHARACTERISTICS_EXT2_VM,
            ],
            bios_major: tables.bios.major,
            bios_minor: tables.bios.minor,
            ec_major: 0xff,
            ec_minor: 0xff,
            ext_rom_size: 0.into(),
        };
        structure_table.extend_from_slice(t0.as_bytes());
        strings.write_to(&mut structure_table);
    }

    // Type 1 — System Information.
    {
        let mut strings = StringSet::default();
        let manufacturer = strings.add(tables.system.manufacturer);
        let product_name = strings.add(tables.system.product_name);
        let version = strings.add(tables.system.version);
        let serial_number = strings.add(tables.system.serial_number);
        let sku_number = strings.add(tables.system.sku_number);
        let family = strings.add(tables.system.family);
        let t1 = SmbiosType1 {
            typ: 1,
            length: size_of::<SmbiosType1>() as u8,
            handle: handle(),
            manufacturer,
            product_name,
            version,
            serial_number,
            uuid: tables.system.uuid,
            wake_up_type: spec::WAKE_UP_TYPE_POWER_SWITCH,
            sku_number,
            family,
        };
        structure_table.extend_from_slice(t1.as_bytes());
        strings.write_to(&mut structure_table);
    }

    // Type 127 — End of Table.
    {
        let t127 = SmbiosType127 {
            typ: 127,
            length: size_of::<SmbiosType127>() as u8,
            handle: handle(),
        };
        structure_table.extend_from_slice(t127.as_bytes());
        // End-of-table has no strings: emit the double-NUL terminator.
        structure_table.extend_from_slice(&[0, 0]);
    }

    let mut entry_point = Smbios30EntryPoint {
        anchor: *b"_SM3_",
        checksum: 0,
        length: size_of::<Smbios30EntryPoint>() as u8,
        major: 3,
        minor: 1,
        docrev: 0,
        revision: 0x01,
        reserved: 0,
        max_size: u32::try_from(structure_table.len()).unwrap().into(),
        table_addr: table_gpa.into(),
    };
    let sum = entry_point
        .as_bytes()
        .iter()
        .fold(0u8, |acc, b| acc.wrapping_add(*b));
    entry_point.checksum = 0u8.wrapping_sub(sum);

    BuiltSmbios {
        entry_point: entry_point.as_bytes().to_vec(),
        structure_table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_smbios_tables() -> SmbiosTables<'static> {
        SmbiosTables {
            system: SmbiosSystemInfo {
                manufacturer: "Test Manufacturer",
                product_name: "Test Product",
                version: "Test Version",
                serial_number: "",
                sku_number: "Test SKU",
                family: "Test Family",
                uuid: [0; 16],
            },
            bios: SmbiosBiosInfo {
                vendor: "Test BIOS Vendor",
                version: "Test BIOS Version",
                release_date: "Test BIOS Release Date",
                major: 1,
                minor: 2,
            },
        }
    }

    #[test]
    fn entry_point_checksum_is_zero() {
        let built = build(&test_smbios_tables(), 0xf0020);
        let sum = built
            .entry_point
            .iter()
            .fold(0u8, |acc, b| acc.wrapping_add(*b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn entry_point_fields() {
        let table_gpa = 0xf0020;
        let built = build(&test_smbios_tables(), table_gpa);
        assert_eq!(built.entry_point.len(), 0x18);
        assert_eq!(&built.entry_point[0..5], b"_SM3_");
        assert_eq!(built.entry_point[6], 0x18); // length
        assert_eq!(built.entry_point[7], 3); // major
        assert_eq!(built.entry_point[8], 1); // minor

        // max_size (offset 0x0c) == structure table length.
        let max_size = u32::from_le_bytes(built.entry_point[0x0c..0x10].try_into().unwrap());
        assert_eq!(max_size as usize, built.structure_table.len());

        // table_addr (offset 0x10) == the GPA we passed in.
        let addr = u64::from_le_bytes(built.entry_point[0x10..0x18].try_into().unwrap());
        assert_eq!(addr, table_gpa);
    }

    #[test]
    fn structure_table_layout() {
        let built = build(&test_smbios_tables(), 0xf0020);
        let table = &built.structure_table;

        // Type 0 header.
        assert_eq!(table[0], 0); // type
        assert_eq!(table[1], 0x1a); // length

        // Type 0 string indices are assigned in order.
        assert_eq!(table[4], 1); // vendor -> string #1
        assert_eq!(table[5], 2); // bios_version -> string #2

        // The structure table must end with the Type 127 end-of-table marker
        // (type, length, handle) followed by the double-NUL terminator. The
        // handle is the third one allocated (0, 1, 2), i.e. 2 little-endian.
        let n = table.len();
        assert_eq!(&table[n - 6..], &[127, 4, 0x02, 0x00, 0, 0]);
    }

    #[test]
    fn strings_resolve() {
        let built = build(&test_smbios_tables(), 0);
        // The first string in the table (after the 0x1a-byte Type 0 formatted
        // area) is the BIOS vendor.
        let strings_start = 0x1a;
        let nul = built.structure_table[strings_start..]
            .iter()
            .position(|&b| b == 0)
            .unwrap();
        let vendor = &built.structure_table[strings_start..strings_start + nul];
        assert_eq!(vendor, "Test BIOS Vendor".as_bytes());
    }

    #[test]
    fn empty_string_uses_index_zero() {
        let tables = test_smbios_tables();
        let built = build(&tables, 0);
        let t1_off = struct_offset(&built.structure_table, 1).expect("Type 1 present");
        // serial_number is the 4th string field, at offset 7 within the Type 1
        // formatted area (type, length, handle:2, manufacturer, product_name,
        // version, serial_number).
        assert_eq!(built.structure_table[t1_off + 7], 0);
    }

    #[test]
    fn interior_nul_is_truncated() {
        let mut tables = test_smbios_tables();
        // An interior NUL must not corrupt the NUL-separated string set: the
        // string is truncated at the NUL and following bytes are dropped, so
        // string indices are not shifted.
        tables.system.manufacturer = "Mfg\0evil";
        let built = build(&tables, 0);
        let t1_off = struct_offset(&built.structure_table, 1).expect("Type 1 present");
        // manufacturer is still string #1, product_name still #2 (unshifted).
        assert_eq!(built.structure_table[t1_off + 4], 1);
        assert_eq!(built.structure_table[t1_off + 5], 2);
        // The string set holds the truncated manufacturer and none of the bytes
        // after the interior NUL.
        let len = built.structure_table[t1_off + 1] as usize;
        let strings = &built.structure_table[t1_off + len..];
        let first_nul = strings.iter().position(|&b| b == 0).unwrap();
        assert_eq!(&strings[..first_nul], b"Mfg");
        assert!(!strings.windows(4).any(|w| w == b"evil"));
    }

    /// Walks the structure table and returns the byte offset of the first
    /// structure with the given type, or `None`.
    fn struct_offset(table: &[u8], want: u8) -> Option<usize> {
        let mut off = 0;
        while off + 2 <= table.len() {
            let typ = table[off];
            let formatted_len = table[off + 1] as usize;
            if typ == want {
                return Some(off);
            }
            // Skip the formatted area, then scan past the string set, which is
            // terminated by a double-NUL.
            let mut i = off + formatted_len;
            while i + 1 < table.len() && !(table[i] == 0 && table[i + 1] == 0) {
                i += 1;
            }
            off = i + 2;
        }
        None
    }
}
