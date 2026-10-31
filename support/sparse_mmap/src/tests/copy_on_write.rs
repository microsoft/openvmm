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

/// Maps a zero-filled file of `pages` pages as a writable copy-on-write view
/// and fills the view with `0xa5`, so every page is a private copy.
fn written_copy_on_write_view(pages: usize) -> SparseMapping {
    let len = SparseMapping::page_size() * pages;
    let file = tempfile::tempfile().unwrap();
    file.set_len(len as u64).unwrap();
    let mappable = new_mappable_from_file_copy_on_write(&file).unwrap();
    let mapping = SparseMapping::new(len).unwrap();
    mapping
        .map_file_copy_on_write(0, len, &mappable, 0, true)
        .unwrap();
    mapping.fill_at(0, 0xa5, len).unwrap();
    mapping
}

/// Asserts that the private writes of [`written_copy_on_write_view`] are still
/// visible in the page at `offset`.
fn assert_private_page(mapping: &SparseMapping, offset: usize) {
    let page_size = SparseMapping::page_size();
    let mut bytes = vec![0; page_size];
    mapping.read_at(offset, &mut bytes).unwrap();
    assert_eq!(bytes, vec![0xa5; page_size]);
}

/// Checks the result of unmapping or mapping over part of a written
/// copy-on-write view: Windows cannot unmap part of a view, so it refuses
/// rather than discarding the private pages of the rest of the view.
fn check_partial_result(result: std::io::Result<()>) {
    if cfg!(windows) {
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    } else {
        result.unwrap();
    }
}

#[test]
fn copy_on_write_partial_unmap_keeps_private_writes() {
    let page_size = SparseMapping::page_size();
    let mapping = written_copy_on_write_view(3);

    check_partial_result(mapping.unmap(page_size, page_size));
    assert_private_page(&mapping, 0);
    assert_private_page(&mapping, page_size * 2);

    mapping.unmap(0, page_size * 3).unwrap();
}

#[test]
fn copy_on_write_partial_replace_keeps_private_writes() {
    let page_size = SparseMapping::page_size();
    let mapping = written_copy_on_write_view(2);

    check_partial_result(mapping.alloc(0, page_size));
    assert_private_page(&mapping, page_size);
}

#[cfg(windows)]
#[test]
fn copy_on_write_split_follows_write_copy_protection() {
    use windows_sys::Win32::System::Memory::PAGE_WRITECOPY;

    let page_size = SparseMapping::page_size();
    let file = tempfile::tempfile().unwrap();
    file.set_len(page_size as u64 * 2).unwrap();
    let mappable = new_mappable_from_file_copy_on_write(&file).unwrap();
    let mapping = SparseMapping::new(page_size * 2).unwrap();

    // A read-only view holds no private pages, so it can be split.
    mapping
        .map_file_copy_on_write(0, page_size * 2, &mappable, 0, false)
        .unwrap();
    mapping.unmap(0, page_size).unwrap();

    // Raising part of a view to copy-on-write keeps the whole view together.
    mapping
        .map_file_copy_on_write(0, page_size * 2, &mappable, 0, false)
        .unwrap();
    mapping
        .protect(page_size, page_size, PAGE_WRITECOPY)
        .unwrap();
    let err = mapping.unmap(0, page_size).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
}
