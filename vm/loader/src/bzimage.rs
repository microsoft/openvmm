// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for extracting uncompressed ELF kernels from Linux bzImage files.
//!
//! A bzImage is the standard compressed kernel image format on x86. It
//! consists of a real-mode boot sector, setup code, and a compressed
//! payload containing the vmlinux ELF image. This module parses the
//! bzImage setup header to locate the compressed payload and
//! decompresses it so the existing ELF loader can process it.
//!
//! See the Linux kernel documentation for the boot protocol:
//! <https://www.kernel.org/doc/html/latest/arch/x86/boot.html>

use std::io::Cursor;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use thiserror::Error;

/// Magic value "HdrS" at offset 0x202 in a bzImage, identifying a valid
/// Linux setup header.
const HDRS_MAGIC: u32 = 0x53726448;

/// Boot flag value at offset 0x1FE.
const BOOT_FLAG: u16 = 0xAA55;

/// Minimum boot protocol version that includes the `payload_offset` and
/// `payload_length` fields (version 2.08, Linux 2.6.31+).
const MIN_PROTOCOL_VERSION_FOR_PAYLOAD: u16 = 0x0208;

/// Minimum number of bytes we need to read to cover the full setup header
/// through the `payload_length` field at offset 0x24F.
const MIN_HEADER_SIZE: usize = 0x250;

/// ELF magic bytes.
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// Gzip magic bytes (0x1f 0x8b).
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Errors that can occur during bzImage detection and extraction.
#[derive(Debug, Error)]
pub enum Error {
    /// An I/O error occurred while reading the bzImage.
    #[error("I/O error reading bzImage")]
    Io(#[source] std::io::Error),
    /// The bzImage boot protocol version is too old to have payload offset/length fields.
    #[error(
        "bzImage boot protocol version {version:#06x} is too old (need >= 2.08 for payload fields)"
    )]
    ProtocolTooOld {
        /// The detected protocol version.
        version: u16,
    },
    /// The compressed payload uses an unsupported compression format.
    #[error("unsupported compression format in bzImage payload (only gzip is supported)")]
    UnsupportedCompression,
    /// Decompression of the payload failed.
    #[error("failed to decompress bzImage payload")]
    DecompressionFailed(#[source] std::io::Error),
    /// The decompressed payload is not a valid ELF image.
    #[error("decompressed bzImage payload is not a valid ELF image")]
    NotElf,
}

/// Attempt to detect whether `kernel_image` is a bzImage.
///
/// Returns `true` if the image has a valid Linux setup header with the
/// "HdrS" magic. The file position is always restored to the beginning
/// before returning.
pub fn is_bzimage(kernel_image: &mut (impl Read + Seek)) -> Result<bool, Error> {
    kernel_image.seek(SeekFrom::Start(0)).map_err(Error::Io)?;

    let mut buf = [0u8; MIN_HEADER_SIZE];
    let result = kernel_image.read(&mut buf);

    // Always restore position before checking the read result.
    kernel_image.seek(SeekFrom::Start(0)).map_err(Error::Io)?;

    let n = result.map_err(Error::Io)?;
    if n < MIN_HEADER_SIZE {
        return Ok(false);
    }

    let boot_flag = u16::from_le_bytes([buf[0x1fe], buf[0x1ff]]);
    let header_magic = u32::from_le_bytes([buf[0x202], buf[0x203], buf[0x204], buf[0x205]]);

    Ok(boot_flag == BOOT_FLAG && header_magic == HDRS_MAGIC)
}

/// Extract the uncompressed vmlinux ELF image from a bzImage.
///
/// Parses the setup header to locate the compressed payload, decompresses
/// it (gzip), and returns a [`Cursor`] over the resulting ELF data that
/// can be passed directly to the ELF loader.
///
/// The file position of `kernel_image` is restored to the beginning on
/// both success and error.
pub fn extract_vmlinux(kernel_image: &mut (impl Read + Seek)) -> Result<Cursor<Vec<u8>>, Error> {
    kernel_image.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    let result = extract_vmlinux_inner(kernel_image);
    // Always restore file position, even on error.
    let _ = kernel_image.seek(SeekFrom::Start(0));
    result
}

fn extract_vmlinux_inner(kernel_image: &mut (impl Read + Seek)) -> Result<Cursor<Vec<u8>>, Error> {
    let mut buf = [0u8; MIN_HEADER_SIZE];
    kernel_image.read_exact(&mut buf).map_err(Error::Io)?;

    // Parse setup header fields.
    let setup_sects = buf[0x1f1];
    let setup_sects: u32 = if setup_sects == 0 {
        4
    } else {
        setup_sects as u32
    };

    let version = u16::from_le_bytes([buf[0x206], buf[0x207]]);
    if version < MIN_PROTOCOL_VERSION_FOR_PAYLOAD {
        return Err(Error::ProtocolTooOld { version });
    }

    let payload_offset = u32::from_le_bytes([buf[0x248], buf[0x249], buf[0x24a], buf[0x24b]]);
    let payload_length = u32::from_le_bytes([buf[0x24c], buf[0x24d], buf[0x24e], buf[0x24f]]);

    // The protected-mode code starts after (setup_sects + 1) 512-byte sectors.
    let protected_mode_offset = (setup_sects + 1) as u64 * 512;
    let payload_file_offset = protected_mode_offset + payload_offset as u64;

    tracing::debug!(
        version = format_args!("{:#06x}", version),
        setup_sects,
        payload_file_offset,
        payload_length,
        "parsing bzImage"
    );

    // Read the compressed payload.
    kernel_image
        .seek(SeekFrom::Start(payload_file_offset))
        .map_err(Error::Io)?;

    let mut payload = vec![0u8; payload_length as usize];
    kernel_image.read_exact(&mut payload).map_err(Error::Io)?;

    // Decompress.
    let elf_data = decompress_payload(&payload)?;

    // Verify the result is an ELF image.
    if elf_data.len() < 4 || elf_data[0..4] != ELF_MAGIC {
        return Err(Error::NotElf);
    }

    tracing::info!(
        compressed_size = payload.len(),
        decompressed_size = elf_data.len(),
        "extracted vmlinux ELF from bzImage"
    );

    Ok(Cursor::new(elf_data))
}

/// Attempt to decompress the payload, trying supported compression formats.
fn decompress_payload(payload: &[u8]) -> Result<Vec<u8>, Error> {
    if payload.len() >= 2 && payload[0..2] == GZIP_MAGIC {
        return decompress_gzip(payload);
    }

    // The payload doesn't start with a recognized magic. Try scanning for
    // a gzip stream within the first few bytes — some kernel builds may
    // have a small stub before the compressed data.
    let scan_limit = payload.len().min(256);
    for i in 1..scan_limit.saturating_sub(1) {
        if payload[i..i + 2] == GZIP_MAGIC {
            match decompress_gzip(&payload[i..]) {
                Ok(data) if data.len() >= 4 && data[0..4] == ELF_MAGIC => {
                    return Ok(data);
                }
                _ => continue,
            }
        }
    }

    Err(Error::UnsupportedCompression)
}

fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(Error::DecompressionFailed)?;
    Ok(decompressed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;

    /// Build a minimal synthetic bzImage for testing.
    fn make_test_bzimage(elf_payload: &[u8]) -> Vec<u8> {
        // Compress the ELF payload with gzip.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(elf_payload).unwrap();
        let compressed = encoder.finish().unwrap();

        // setup_sects = 1 (minimum), so protected-mode code starts at sector 2 (offset 1024).
        let setup_sects: u8 = 1;
        let protected_mode_offset = (setup_sects as u32 + 1) * 512;
        let payload_offset: u32 = 0; // payload at start of protected-mode code
        let payload_length: u32 = compressed.len() as u32;

        let total_size = protected_mode_offset as usize + compressed.len();
        let mut image = vec![0u8; total_size];

        // setup_sects at 0x1f1
        image[0x1f1] = setup_sects;
        // boot_flag at 0x1fe = 0xAA55
        image[0x1fe..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        // header magic "HdrS" at 0x202
        image[0x202..0x206].copy_from_slice(&HDRS_MAGIC.to_le_bytes());
        // version at 0x206 = 0x020f (protocol 2.15)
        image[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        // payload_offset at 0x248
        image[0x248..0x24c].copy_from_slice(&payload_offset.to_le_bytes());
        // payload_length at 0x24c
        image[0x24c..0x250].copy_from_slice(&payload_length.to_le_bytes());

        // Write the compressed payload.
        image[protected_mode_offset as usize..].copy_from_slice(&compressed);

        image
    }

    #[test]
    fn test_is_bzimage_positive() {
        let fake_elf = {
            let mut v = vec![0u8; 64];
            v[0..4].copy_from_slice(&ELF_MAGIC);
            v
        };
        let bzimage = make_test_bzimage(&fake_elf);
        let mut cursor = Cursor::new(bzimage);
        assert!(is_bzimage(&mut cursor).unwrap());
    }

    #[test]
    fn test_is_bzimage_negative_elf() {
        let mut elf = vec![0u8; 0x1000];
        elf[0..4].copy_from_slice(&ELF_MAGIC);
        let mut cursor = Cursor::new(elf);
        assert!(!is_bzimage(&mut cursor).unwrap());
    }

    #[test]
    fn test_extract_vmlinux() {
        let fake_elf = {
            let mut v = vec![0u8; 256];
            v[0..4].copy_from_slice(&ELF_MAGIC);
            // Put some recognizable data in there.
            v[4..8].copy_from_slice(b"TEST");
            v
        };
        let bzimage = make_test_bzimage(&fake_elf);
        let mut cursor = Cursor::new(bzimage);

        let extracted = extract_vmlinux(&mut cursor).unwrap();
        let data = extracted.into_inner();
        assert_eq!(&data[0..4], &ELF_MAGIC);
        assert_eq!(&data[4..8], b"TEST");
        assert_eq!(data.len(), 256);
    }

    #[test]
    fn test_not_bzimage_returns_none() {
        let mut elf = vec![0u8; 0x1000];
        elf[0..4].copy_from_slice(&ELF_MAGIC);
        let mut cursor = Cursor::new(elf);
        assert!(!is_bzimage(&mut cursor).unwrap());
    }
}
