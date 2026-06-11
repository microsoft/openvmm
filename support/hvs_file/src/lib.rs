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
        w.add_array("/test/blob", &data).unwrap();
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
        w.add_array("/savedstate/savedVM/partition_state", &data)
            .unwrap();
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        assert_eq!(
            r.read_array("/savedstate/savedVM/partition_state").unwrap(),
            data
        );
    }

    #[test]
    fn round_trip_multiple_keys() {
        let blob = vec![0xAA; 100];
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_uint("/savedstate/VmVersion", 0x0A00);
        w.add_string("/savedstate/type", "Normal");
        w.add_array("/savedstate/savedVM/partition_state", &blob)
            .unwrap();
        w.add_bool("/savedstate/compressed", false);
        w.add_int("/savedstate/vpcount", 4);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_uint("/savedstate/VmVersion").unwrap(), 0x0A00);
        assert_eq!(r.read_string("/savedstate/type").unwrap(), "Normal");
        assert_eq!(
            r.read_array("/savedstate/savedVM/partition_state").unwrap(),
            blob
        );
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

    /// Verify that key table entries exactly fill each table.
    ///
    /// Per FORMAT.md ("Key table filling"), entries must exactly fill
    /// the data area. Free entries must be > 21 bytes; gaps of 1–21
    /// bytes must be absorbed into the preceding entry's SizeInBytes.
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
            w.add_array(&name, &data).unwrap();
        }
        let buf = w.finish().unwrap();
        let data = buf.into_inner();

        // Parse the file and verify every key table's entries.
        let obj_count = u32::from_le_bytes(data[8196..8200].try_into().unwrap()) as usize;

        for i in 0..obj_count {
            let base = 8200 + i * 18;
            if data[base] != 2 {
                continue; // not a KeyTable
            }
            let kt_off = u64::from_le_bytes(data[base + 5..base + 13].try_into().unwrap()) as usize;

            let mut offset = size_of::<KeyTableHeader>();
            let data_end = DEFAULT_KEY_TABLE_SIZE as usize;
            while offset + entry_hdr <= data_end {
                let pos = kt_off + offset;
                let entry_size =
                    u32::from_le_bytes(data[pos + 2..pos + 6].try_into().unwrap()) as usize;
                assert_ne!(
                    entry_size, 0,
                    "zero-size entry in key table at offset {offset}"
                );
                assert!(
                    offset + entry_size <= data_end,
                    "entry at offset {offset} (size {entry_size}) overflows key table \
                     (offset + size = {} > data_end = {data_end})",
                    offset + entry_size,
                );
                offset += entry_size;
            }
            assert_eq!(
                offset,
                data_end,
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
        // 500 file objects requires 3 chained object tables.
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        for i in 0..500u32 {
            let mut block = vec![0u8; 2048];
            block[..4].copy_from_slice(&i.to_le_bytes());
            w.add_array(&format!("/data/block{i}"), &block).unwrap();
        }
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        for i in 0..500u32 {
            let actual = r.read_array(&format!("/data/block{i}")).unwrap();
            assert_eq!(actual.len(), 2048, "block{i} wrong size");
            let stamp = u32::from_le_bytes(actual[..4].try_into().unwrap());
            assert_eq!(stamp, i, "block{i} data mismatch");
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
