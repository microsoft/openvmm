// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for flushing shared file mappings.

use crate::SparseMapping;
use crate::new_mappable_from_file;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;

#[test]
fn test_flush_shared_file_mapping() {
    let page_size = SparseMapping::page_size();
    let mut file = tempfile::tempfile().unwrap();
    file.set_len(page_size as u64).unwrap();
    let mappable = new_mappable_from_file(&file, true, false).unwrap();
    let mapping = SparseMapping::new(page_size).unwrap();
    mapping.map_file(0, page_size, &mappable, 0, true).unwrap();

    mapping.write_at(0, b"flushed").unwrap();
    mapping.flush(0, page_size).unwrap();

    let mut bytes = [0_u8; 7];
    file.seek(std::io::SeekFrom::Start(0)).unwrap();
    file.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"flushed");
}

#[test]
fn test_flush_range_validation() {
    let page_size = SparseMapping::page_size();
    let len = page_size * 2;
    let file = tempfile::tempfile().unwrap();
    file.set_len(len as u64).unwrap();
    let mappable = new_mappable_from_file(&file, true, false).unwrap();
    let mapping = SparseMapping::new(len).unwrap();
    mapping.map_file(0, len, &mappable, 0, true).unwrap();

    for (offset, flush_len) in [
        (1, page_size - 1),
        (0, 1),
        (0, page_size + 1),
        (page_size, len),
        (page_size, usize::MAX),
    ] {
        let err = mapping.flush(offset, flush_len).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::InvalidInput,
            "offset {offset:#x}, len {flush_len:#x}"
        );
    }

    mapping.flush(page_size, page_size).unwrap();
    mapping.flush(0, 0).unwrap();
    mapping.flush(len, 0).unwrap();
}
