// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HyperV Storage file format reader and writer.
//!
//! This crate implements the binary key-value file format used by Hyper-V
//! for `.vmrs` (saved state), `.vmcx` (configuration), and `.vsv` files.
//!
//! # Usage
//!
//! ```rust,no_run
//! use hvs_file::writer::HvsFileWriter;
//! use hvs_file::reader::HvsFileReader;
//! use std::io::Cursor;
//!
//! // Write a file
//! let buf = Cursor::new(Vec::new());
//! let mut w = HvsFileWriter::new(buf).unwrap();
//! w.add_uint("/savedstate/VmVersion", 0x0A00);
//! let mut buf = w.finish().unwrap();
//!
//! // Read it back
//! buf.set_position(0);
//! let mut r = HvsFileReader::open(buf).unwrap();
//! assert_eq!(r.read_uint("/savedstate/VmVersion").unwrap(), 0x0A00);
//! ```

pub(crate) mod defs;
pub mod reader;
pub mod writer;

/// Computes CRC-32 (ISO 3309) over a byte slice.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Computes the checksum for a structure, skipping the checksum field.
///
/// Hashes the bytes before `checksum_offset`, then 4 zero bytes, then
/// the bytes after — without mutating or copying the input.
pub(crate) fn struct_checksum(bytes: &[u8], checksum_offset: usize) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&bytes[..checksum_offset]);
    hasher.update(&[0u8; 4]);
    hasher.update(&bytes[checksum_offset + 4..]);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use crate::reader::HvsFileReader;
    use crate::writer::HvsFileWriter;
    use std::io::Cursor;

    #[test]
    fn roundtrip_real_vmrs_through_writer() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("E7E9D405-022F-4D55-9B8C-C777CC321051.VMRS");
        if !path.exists() {
            eprintln!("SKIP: real saved state VMRS not found");
            return;
        }
        
        // Read the real file
        let file = std::fs::File::open(&path).unwrap();
        let mut reader = HvsFileReader::open(file).unwrap();
        let version = reader.read_int("/savedstate/VmVersion").unwrap();
        let ps = reader.read_array("/savedstate/savedVM/partition_state").unwrap();
        
        // Write a minimal version with just the required keys
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_int("/savedstate/VmVersion", version);
        w.add_array("/savedstate/savedVM/partition_state", ps).unwrap();
        // Add a minimal RamMemoryBlock0
        let mut ram_meta = vec![0u8; 40];
        ram_meta[0..4].copy_from_slice(&3u32.to_le_bytes());
        ram_meta[16..24].copy_from_slice(&1u64.to_le_bytes());
        w.add_array("/savedstate/RamMemoryBlock0", ram_meta).unwrap();
        // One empty RAM block
        w.add_array("/savedstate/RamBlock0", vec![0u8; 4096]).unwrap();
        
        let buf = w.finish().unwrap();
        let out_path = std::env::temp_dir().join("hvs_roundtrip_test.vmrs");
        std::fs::write(&out_path, buf.into_inner()).unwrap();
        eprintln!("Wrote round-tripped file to {}", out_path.display());
    }

    #[test]
    #[ignore] // run manually: cargo test -p hvs_file --lib -- dump_real_keys --ignored --nocapture
    fn dump_real_keys() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("E7E9D405-022F-4D55-9B8C-C777CC321051.VMRS");
        if !path.exists() {
            eprintln!("SKIP: real saved state VMRS not found");
            return;
        }
        let file = std::fs::File::open(&path).unwrap();
        let reader = HvsFileReader::open(file).unwrap();
        let keys: Vec<&str> = reader.keys().collect();
        for key in &keys {
            let type_tag = match reader.value_type(key).unwrap() {
                crate::reader::ValueType::Int => "INT",
                crate::reader::ValueType::UInt => "UINT",
                crate::reader::ValueType::String => "STRING",
                crate::reader::ValueType::Array => "ARRAY",
                crate::reader::ValueType::Bool => "BOOL",
            };
            eprintln!("{type_tag} {key}");
        }
        eprintln!("\nTotal: {} keys", keys.len());
    }

    #[test]
    fn read_real_saved_state() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("..")
            .join("E7E9D405-022F-4D55-9B8C-C777CC321051.VMRS");
        if !path.exists() {
            eprintln!("SKIP: real saved state VMRS not found");
            return;
        }
        let file = std::fs::File::open(&path).unwrap();
        let mut reader = HvsFileReader::open(file).unwrap();
        
        // Check that we can find the savedstate keys
        assert!(reader.contains_key("/savedstate/VmVersion"), "VmVersion not found");
        let version = reader.read_int("/savedstate/VmVersion").unwrap();
        eprintln!("VmVersion: 0x{version:X}");
        assert!(version > 0);
        
        // Check partition state exists
        assert!(reader.contains_key("/savedstate/savedVM/partition_state"), "partition_state not found");
        let ps = reader.read_array("/savedstate/savedVM/partition_state").unwrap();
        eprintln!("partition_state size: {} bytes", ps.len());
        assert!(!ps.is_empty());
    }

    #[test]
    fn write_debug_vmrs() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_int("/savedstate/VmVersion", 0x0A00);
        w.add_array("/savedstate/RamMemoryBlock0", vec![0u8; 40]).unwrap();
        w.add_array("/savedstate/savedVM/partition_state", vec![0u8; 100]).unwrap();
        let buf = w.finish().unwrap();
        let data = buf.into_inner();

        // Verify header bytes
        let sig = u32::from_le_bytes(data[0..4].try_into().unwrap());
        assert_eq!(sig, 0x01282014, "bad signature");

        let cksum = u32::from_le_bytes(data[4..8].try_into().unwrap());

        // Verify CRC
        let mut header_copy = data[..46].to_vec();
        header_copy[4..8].fill(0);
        let computed = crate::crc32(&header_copy);
        assert_eq!(computed, cksum, "header CRC mismatch");

        // Dump object table entries
        let obj_sig = u32::from_le_bytes(data[8192..8196].try_into().unwrap());
        let obj_count = u32::from_le_bytes(data[8196..8200].try_into().unwrap());
        eprintln!("Object table: sig=0x{obj_sig:08X} count={obj_count}");

        for i in 0..obj_count as usize {
            let base = 8200 + i * 18;
            let obj_type = data[base];
            let offset = u64::from_le_bytes(data[base + 5..base + 13].try_into().unwrap());
            let size = u32::from_le_bytes(data[base + 13..base + 17].try_into().unwrap());
            let flags = data[base + 17];
            let type_name = match obj_type {
                0 => "Empty",
                1 => "ObjTbl",
                2 => "KeyTbl",
                3 => "FileObj",
                4 => "Free",
                _ => "???",
            };
            eprintln!("  [{i}] {type_name:7} off=0x{offset:06X} sz={size:6} flg=0x{flags:02X}");
            // Verify offset + size doesn't exceed file
            if obj_type != 0 {
                assert!(
                    (offset as usize + size as usize) <= data.len(),
                    "entry {i}: offset 0x{offset:X} + size {size} > file size {}",
                    data.len()
                );
            }
        }

        // Dump key table header at the first KeyTable entry
        for i in 0..obj_count as usize {
            let base = 8200 + i * 18;
            if data[base] == 2 {
                // KeyTable
                let offset = u64::from_le_bytes(data[base + 5..base + 13].try_into().unwrap()) as usize;
                let kt_sig = u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
                let kt_idx = u16::from_le_bytes(data[offset + 2..offset + 4].try_into().unwrap());
                let kt_seq = u16::from_le_bytes(data[offset + 4..offset + 6].try_into().unwrap());
                let kt_cksum = u32::from_le_bytes(data[offset + 6..offset + 10].try_into().unwrap());
                eprintln!("  KeyTable[{i}] at 0x{offset:X}: sig=0x{kt_sig:04X} idx={kt_idx} seq={kt_seq} cksum=0x{kt_cksum:08X}");

                // Verify key table checksum
                let mut kt_header = data[offset..offset + 10].to_vec();
                kt_header[6..10].fill(0);
                let kt_computed = crate::crc32(&kt_header);
                assert_eq!(kt_computed, kt_cksum, "key table {i} CRC mismatch");

                // Dump first few entries
                let mut pos = offset + 10;
                let end = offset + 4096;
                let mut entry_idx = 0;
                while pos + 21 <= end && entry_idx < 10 {
                    let entry_type = data[pos];
                    let entry_size = u32::from_le_bytes(data[pos + 2..pos + 6].try_into().unwrap());
                    if entry_size == 0 {
                        break;
                    }
                    let parent_table = u16::from_le_bytes(data[pos + 6..pos + 8].try_into().unwrap());
                    let parent_off = u32::from_le_bytes(data[pos + 8..pos + 12].try_into().unwrap());
                    let name_len = data[pos + 20];
                    let name = if name_len > 0 && pos + 21 + name_len as usize <= end {
                        String::from_utf8_lossy(&data[pos + 21..pos + 21 + name_len as usize - 1]).to_string()
                    } else {
                        "(none)".to_string()
                    };
                    let type_name = match entry_type {
                        1 => "Free",
                        3 => "Int",
                        4 => "UInt",
                        7 => "Array",
                        8 => "Bool",
                        9 => "Node",
                        _ => "?",
                    };
                    eprintln!("    entry[{entry_idx}] {type_name:5} sz={entry_size:3} parent=({parent_table},{parent_off:3}) name_len={name_len} name=\"{name}\"");
                    pos += entry_size as usize;
                    entry_idx += 1;
                }
            }
        }

        // Write to temp
        let path = std::env::temp_dir().join("hvs_debug.vmrs");
        std::fs::write(&path, &data).unwrap();
        eprintln!("Wrote {} bytes to {}", data.len(), path.display());
    }

    #[test]
    fn round_trip_uint() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_uint("/savedstate/VmVersion", 0x0A00);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_uint("/savedstate/VmVersion").unwrap(), 0x0A00);
    }

    #[test]
    fn round_trip_int() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_int("/test/negative", -42);
        w.add_int("/test/positive", 999);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_int("/test/negative").unwrap(), -42);
        assert_eq!(r.read_int("/test/positive").unwrap(), 999);
    }

    #[test]
    fn round_trip_string() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_string("/savedstate/type", "Normal");
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_string("/savedstate/type").unwrap(), "Normal");
    }

    #[test]
    fn round_trip_bool() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_bool("/test/flag_true", true);
        w.add_bool("/test/flag_false", false);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert!(r.read_bool("/test/flag_true").unwrap());
        assert!(!r.read_bool("/test/flag_false").unwrap());
    }

    #[test]
    fn round_trip_array() {
        let data = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03];
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_array("/test/blob", data.clone()).unwrap();
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_array("/test/blob").unwrap(), data);
    }

    #[test]
    fn round_trip_file_object() {
        // Create a large blob that triggers file object storage (>= 2048)
        let data: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_array("/savedstate/savedVM/partition_state", data.clone()).unwrap();
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_array("/savedstate/savedVM/partition_state").unwrap(), data);
    }

    #[test]
    fn round_trip_multiple_keys() {
        let blob = vec![0xAA; 100];
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_uint("/savedstate/VmVersion", 0x0A00);
        w.add_string("/savedstate/type", "Normal");
        w.add_array("/savedstate/savedVM/partition_state", blob.clone()).unwrap();
        w.add_bool("/savedstate/compressed", false);
        w.add_int("/savedstate/vpcount", 4);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_uint("/savedstate/VmVersion").unwrap(), 0x0A00);
        assert_eq!(r.read_string("/savedstate/type").unwrap(), "Normal");
        assert_eq!(r.read_array("/savedstate/savedVM/partition_state").unwrap(), blob);
        assert!(!r.read_bool("/savedstate/compressed").unwrap());
        assert_eq!(r.read_int("/savedstate/vpcount").unwrap(), 4);
    }

    #[test]
    fn round_trip_deep_paths() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_uint("/a/b/c/d/value", 42);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_uint("/a/b/c/d/value").unwrap(), 42);
    }

    #[test]
    fn key_not_found() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_uint("/exists", 1);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert!(r.read_uint("/does_not_exist").is_err());
    }

    /// Verify that key table entries never overflow the table boundary.
    ///
    /// The DLL's key table verifier walks entries with:
    ///   `while (offset + sizeof(EntryHeader) < dataEnd)`
    /// Using strict `<`, it cannot reach an entry starting at exactly
    /// `dataEnd - sizeof(EntryHeader)` (i.e., 21 bytes from the end).
    /// If such an entry exists, the verifier stops early and reports
    /// `offset != dataEnd` → ERROR_FILE_CORRUPT.
    ///
    /// The writer must ensure no entry (including Free gap-fillers)
    /// starts at that position by moving the preceding real entry to
    /// the next key table and absorbing the slack into an earlier entry.
    #[test]
    fn no_entry_at_key_table_boundary() {
        use crate::defs::*;
        use core::mem::size_of;

        // Strategy: add enough keys under a common parent so that the
        // node + leaf entries almost fill a key table, then check that
        // no entry starts at exactly `dataEnd - sizeof(EntryHeader)`.
        //
        // Key table: 4096 bytes total, 10-byte header → 4086 usable.
        // Entry header is 21 bytes. We must verify no entry starts at
        // offset 4075 (= 4096 - 21) within any key table.
        let _usable = DEFAULT_KEY_TABLE_SIZE as usize - size_of::<KeyTableHeader>();
        let entry_hdr = size_of::<KeyTableEntryHeader>();

        // Generate many keys with varying name/data sizes to exercise
        // different gap sizes across multiple key tables.
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        for i in 0..300 {
            let name = format!("/parent/key_{i:04}");
            // Vary data size to hit different table-fill patterns.
            let data = vec![0xABu8; (i * 7) % 50];
            w.add_array(&name, data).unwrap();
        }
        let buf = w.finish().unwrap();
        let data = buf.into_inner();

        // Parse the file and verify every key table's entries.
        let obj_count =
            u32::from_le_bytes(data[8196..8200].try_into().unwrap()) as usize;

        for i in 0..obj_count {
            let base = 8200 + i * 18;
            if data[base] != 2 {
                continue; // not a KeyTable
            }
            let kt_off =
                u64::from_le_bytes(data[base + 5..base + 13].try_into().unwrap()) as usize;

            let mut offset = size_of::<KeyTableHeader>();
            let data_end = DEFAULT_KEY_TABLE_SIZE as usize;
            while offset + entry_hdr <= data_end {
                let pos = kt_off + offset;
                let entry_size = u32::from_le_bytes(
                    data[pos + 2..pos + 6].try_into().unwrap(),
                ) as usize;
                assert_ne!(entry_size, 0, "zero-size entry in key table at offset {offset}");
                assert!(
                    offset + entry_size <= data_end,
                    "entry at offset {offset} (size {entry_size}) overflows key table \
                     (offset + size = {} > data_end = {data_end})",
                    offset + entry_size,
                );
                offset += entry_size;
            }
            assert_eq!(
                offset, data_end,
                "key table entries don't exactly fill the table \
                 (ended at {offset}, expected {data_end}, gap = {})",
                data_end - offset,
            );
        }

        // Also verify we can read every key back.
        let mut reader = HvsFileReader::open(Cursor::new(&data)).unwrap();
        for i in 0..300 {
            let name = format!("/parent/key_{i:04}");
            let expected = vec![0xABu8; (i * 7) % 50];
            let actual = reader.read_array(&name).unwrap();
            assert_eq!(actual, expected, "data mismatch for {name}");
        }
    }

    #[test]
    fn object_table_chaining() {
        // Each file object >= FILE_OBJECT_THRESHOLD gets its own object table
        // entry. One table holds (4096 - 8) / 18 - 1 = 226 usable slots.
        // Writing 250 file objects (each >= 2048 bytes) plus a few key tables
        // should require chaining to a second object table.
        let blob = vec![0xCDu8; 2048];
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        for i in 0..250 {
            w.add_array(&format!("/data/block{i}"), blob.clone()).unwrap();
        }
        let mut buf = w.finish().unwrap();

        // Read it back and verify all 250 keys.
        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        for i in 0..250 {
            let actual = r.read_array(&format!("/data/block{i}")).unwrap();
            assert_eq!(actual.len(), 2048, "block{i} wrong size");
            assert_eq!(actual[0], 0xCD, "block{i} wrong data");
        }
    }

    #[test]
    fn contains_key() {
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_uint("/savedstate/VmVersion", 0x0A00);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert!(r.contains_key("/savedstate/VmVersion"));
        assert!(!r.contains_key("/nonexistent"));
    }
}
