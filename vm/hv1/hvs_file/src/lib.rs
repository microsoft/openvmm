// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HyperV Storage file format reader and writer.
//!
//! This crate implements the binary key-value file format used by Hyper-V
//! for `.vmrs` (saved state), `.vmcx` (configuration), and `.vsv` files.

#![expect(missing_docs)]
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
//! let r = HvsFileReader::open(buf).unwrap();
//! assert_eq!(r.read_uint("/savedstate/VmVersion").unwrap(), 0x0A00);
//! ```

pub mod defs;
pub mod reader;
pub mod writer;

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
        w.add_array("/test/blob", data.clone());
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_array("/test/blob").unwrap(), data);
    }

    #[test]
    fn round_trip_file_object() {
        // Create a large-ish blob that would normally be stored as a file object
        let data: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_file_object("/savedstate/savedVM/partition_state", &data).unwrap();
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let mut r = HvsFileReader::open(buf).unwrap();
        assert_eq!(r.read_file_object("/savedstate/savedVM/partition_state").unwrap(), data);
    }

    #[test]
    fn round_trip_multiple_keys() {
        let blob = vec![0xAA; 100];
        let buf = Cursor::new(Vec::new());
        let mut w = HvsFileWriter::new(buf).unwrap();
        w.add_uint("/savedstate/VmVersion", 0x0A00);
        w.add_string("/savedstate/type", "Normal");
        w.add_array("/savedstate/savedVM/partition_state", blob.clone());
        w.add_bool("/savedstate/compressed", false);
        w.add_int("/savedstate/vpcount", 4);
        let mut buf = w.finish().unwrap();

        buf.set_position(0);
        let r = HvsFileReader::open(buf).unwrap();
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
