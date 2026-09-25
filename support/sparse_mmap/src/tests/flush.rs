// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for flushing shared file mappings.

use crate::SparseMapping;
use crate::new_mappable_from_file;
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
