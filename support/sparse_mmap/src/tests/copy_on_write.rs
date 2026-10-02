// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for copy-on-write file mappings.

use crate::SparseMapping;
use crate::new_mappable_from_file_copy_on_write;
use std::io::Read;
use std::io::Seek;
use std::io::Write;

#[test]
fn copy_on_write_file_mapping_does_not_modify_file() {
    let page_size = SparseMapping::page_size();
    let mapping_size = page_size * 16;
    let original = (0..mapping_size)
        .map(|offset| (offset / page_size) as u8)
        .collect::<Vec<_>>();
    let mut artifact = tempfile::NamedTempFile::new().unwrap();
    artifact.write_all(&original).unwrap();
    artifact.as_file().sync_all().unwrap();
    let mut file = std::fs::File::open(artifact.path()).unwrap();

    let mappable = new_mappable_from_file_copy_on_write(&file).unwrap();
    let mapping = SparseMapping::new(mapping_size).unwrap();
    mapping
        .map_file_copy_on_write(0, mapping_size, &mappable, 0, true)
        .unwrap();
    mapping.fill_at(0, 0xa5, mapping_size).unwrap();

    let mut mapped_bytes = vec![0; mapping_size];
    mapping.read_at(0, &mut mapped_bytes).unwrap();
    assert_eq!(mapped_bytes, vec![0xa5; mapping_size]);
    drop(mapping);

    file.rewind().unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();
    assert_eq!(file_bytes, original);

    let mappable = new_mappable_from_file_copy_on_write(&file).unwrap();
    let mapping = SparseMapping::new(mapping_size).unwrap();
    mapping
        .map_file_copy_on_write(0, mapping_size, &mappable, 0, true)
        .unwrap();
    let mut remapped_bytes = vec![0; mapping_size];
    mapping.read_at(0, &mut remapped_bytes).unwrap();
    assert_eq!(remapped_bytes, original);
}

#[cfg(unix)]
#[test]
fn copy_on_write_file_mapping_rejects_out_of_range_file_offset() {
    let page_size = SparseMapping::page_size();
    let file = tempfile::tempfile().unwrap();
    file.set_len(page_size as u64).unwrap();

    let mappable = new_mappable_from_file_copy_on_write(&file).unwrap();
    let mapping = SparseMapping::new(page_size).unwrap();
    let err = mapping
        .map_file_copy_on_write(0, page_size, &mappable, i64::MAX as u64 + 1, false)
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}
