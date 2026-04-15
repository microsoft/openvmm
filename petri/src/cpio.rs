// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPIO newc archive builder and merger.
//!
//! Provides tools to inject files into an existing gzip-compressed CPIO
//! initrd. Instead of concatenating separate CPIO archives (which can be
//! unreliable), this module decompresses the original, inserts new entries
//! before the TRAILER!!!, and recompresses as a single archive.

use anyhow::Context;

/// The CPIO newc magic number.
const CPIO_NEWC_MAGIC: &[u8] = b"070701";

/// The CPIO trailer filename.
const CPIO_TRAILER: &[u8] = b"TRAILER!!!";

/// Inject a file into a gzip-compressed CPIO initrd.
///
/// Decompresses the initrd, inserts the file entry before the TRAILER!!!
/// marker, and recompresses as a single gzip stream. Returns the new
/// gzip-compressed initrd bytes.
pub fn inject_into_initrd(
    initrd_gz: &[u8],
    file_name: &str,
    file_data: &[u8],
    mode: u32,
) -> anyhow::Result<Vec<u8>> {
    // Decompress the original initrd
    let decompressed = decompress_gz(initrd_gz).context("failed to decompress initrd")?;

    // Find the TRAILER!!! entry and insert our file before it
    let trailer_offset =
        find_trailer_offset(&decompressed).context("could not find TRAILER!!! in CPIO archive")?;

    let mut merged = Vec::with_capacity(decompressed.len() + file_data.len() + 256);
    // Everything before the trailer
    merged.extend_from_slice(&decompressed[..trailer_offset]);
    // Our new file entry
    write_cpio_entry(&mut merged, file_name, file_data, mode);
    // The trailer and everything after it
    merged.extend_from_slice(&decompressed[trailer_offset..]);

    // Recompress
    compress_gz(&merged).context("failed to recompress initrd")
}

/// Find the byte offset of the TRAILER!!! entry in a raw CPIO newc stream.
fn find_trailer_offset(data: &[u8]) -> Option<usize> {
    // The trailer entry starts with the magic "070701" followed by fixed
    // fields, then the filename "TRAILER!!!\0". We search for the magic
    // followed by the trailer name at the expected offset (110 bytes into
    // the header is where the filename starts).
    let mut pos = 0;
    while pos + 110 + CPIO_TRAILER.len() < data.len() {
        if &data[pos..pos + 6] == CPIO_NEWC_MAGIC {
            // Parse namesize from the header (bytes 94..102, 8 hex chars)
            let namesize_hex = &data[pos + 94..pos + 102];
            if let Ok(namesize_str) = std::str::from_utf8(namesize_hex) {
                if let Ok(namesize) = u32::from_str_radix(namesize_str, 16) {
                    let name_start = pos + 110;
                    let name_end = name_start + namesize as usize - 1; // exclude NUL
                    if name_end <= data.len() && &data[name_start..name_end] == CPIO_TRAILER {
                        return Some(pos);
                    }
                    // Skip past this entry to the next one
                    let filesize_hex = &data[pos + 54..pos + 62];
                    if let Ok(fs_str) = std::str::from_utf8(filesize_hex) {
                        if let Ok(filesize) = u32::from_str_radix(fs_str, 16) {
                            let header_plus_name = 110 + namesize as usize;
                            let name_padded = header_plus_name + (4 - (header_plus_name % 4)) % 4;
                            let data_padded = filesize as usize + (4 - (filesize as usize % 4)) % 4;
                            pos = pos + name_padded + data_padded;
                            continue;
                        }
                    }
                }
            }
            // If parsing failed, advance by 1 and keep searching
            pos += 1;
        } else {
            pos += 1;
        }
    }
    None
}

/// Decompress gzip data.
fn decompress_gz(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let mut decoder = GzDecoder::new(data);
    let mut buf = Vec::new();
    decoder
        .read_to_end(&mut buf)
        .context("gzip decompression failed")?;
    Ok(buf)
}

/// Compress data with gzip.
fn compress_gz(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).context("gzip compression failed")?;
    encoder.finish().context("gzip finish failed")
}

/// Write a single CPIO newc entry (header + filename + data).
fn write_cpio_entry(buf: &mut Vec<u8>, name: &str, data: &[u8], mode: u32) {
    // newc header is exactly 110 bytes of ASCII hex fields
    let name_with_nul = name.len() + 1; // include NUL terminator
    let header = format!(
        "070701\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}\
         {:08X}",
        0u32,                                                          // ino
        mode,                                                          // mode
        0u32,                                                          // uid
        0u32,                                                          // gid
        1u32,                                                          // nlink
        0u32,                                                          // mtime
        u32::try_from(data.len()).expect("file too large for CPIO"),   // filesize
        0u32,                                                          // devmajor
        0u32,                                                          // devminor
        0u32,                                                          // rdevmajor
        0u32,                                                          // rdevminor
        u32::try_from(name_with_nul).expect("name too long for CPIO"), // namesize
        0u32,                                                          // check (unused in newc)
    );

    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(name.as_bytes());
    buf.push(0); // NUL terminator

    // Pad name to 4-byte boundary (header + name must be 4-byte aligned)
    let header_plus_name = 110 + name_with_nul;
    let name_padding = (4 - (header_plus_name % 4)) % 4;
    buf.extend(std::iter::repeat_n(0u8, name_padding));

    // File data
    buf.extend_from_slice(data);

    // Pad data to 4-byte boundary
    let data_padding = (4 - (data.len() % 4)) % 4;
    buf.extend(std::iter::repeat_n(0u8, data_padding));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_trailer_in_simple_archive() {
        let mut buf = Vec::new();
        write_cpio_entry(&mut buf, "hello", b"world", 0o100644);
        let trailer_pos = buf.len();
        write_cpio_entry(&mut buf, "TRAILER!!!", &[], 0);
        assert_eq!(find_trailer_offset(&buf), Some(trailer_pos));
    }

    #[test]
    fn inject_adds_file_before_trailer() {
        // Build a minimal uncompressed CPIO, gzip it, inject, decompress, verify
        let mut original = Vec::new();
        write_cpio_entry(&mut original, "existing", b"data", 0o100644);
        write_cpio_entry(&mut original, "TRAILER!!!", &[], 0);

        let gz = compress_gz(&original).unwrap();
        let merged_gz = inject_into_initrd(&gz, "pipette", b"binary", 0o100755).unwrap();
        let merged = decompress_gz(&merged_gz).unwrap();

        let s = String::from_utf8_lossy(&merged);
        // Both files should be present
        assert!(s.contains("existing"));
        assert!(s.contains("pipette"));
        assert!(s.contains("TRAILER!!!"));

        // pipette should appear before TRAILER
        let pipette_pos = s.find("pipette").unwrap();
        let trailer_pos = s.find("TRAILER!!!").unwrap();
        assert!(pipette_pos < trailer_pos);
    }
}
