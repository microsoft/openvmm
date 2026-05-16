// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for loading Linux x86 bzImage files directly.
//!
//! A bzImage is the standard compressed kernel image format on x86/x86_64.
//! It consists of a real-mode boot sector, setup code, and a compressed
//! payload that the kernel's own startup stub decompresses at boot time.
//! This format is specific to x86; AArch64 uses a different `Image` format.
//!
//! See the Linux kernel documentation for the boot protocol:
//! <https://www.kernel.org/doc/html/latest/arch/x86/boot.html>

use loader_defs::linux as defs;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::mem::size_of;
use thiserror::Error;
use zerocopy::FromBytes;

/// Magic value "HdrS" at offset 0x202 in a bzImage, identifying a valid
/// Linux setup header.
const HDRS_MAGIC: u32 = 0x53726448;

/// Boot flag value at offset 0x1FE.
const BOOT_FLAG: u16 = 0xAA55;

/// Minimum boot protocol version that supports 64-bit boot (version 2.12+).
const MIN_PROTOCOL_VERSION: u16 = 0x020C;

/// Offset of the `setup_header` struct within the boot sector.
const SETUP_HEADER_OFFSET: usize = 0x1F1;

/// Minimum number of bytes we need to read to cover the full setup header.
/// Derived from the struct layout so it stays correct if `setup_header` changes.
const MIN_HEADER_SIZE: usize = SETUP_HEADER_OFFSET + size_of::<defs::setup_header>();

/// The `loadflags` bit indicating the protected-mode code should be loaded high (at 0x100000).
const LOADED_HIGH: u8 = 0x01;

/// The `xloadflags` bit indicating the kernel has a 64-bit entry point.
const XLF_KERNEL_64: u16 = 0x01;

/// Errors that can occur during bzImage detection and parsing.
#[derive(Debug, Error)]
pub enum Error {
    /// An I/O error occurred while reading the bzImage.
    #[error("I/O error reading bzImage")]
    Io(#[source] std::io::Error),
    /// The image is not a valid bzImage (missing boot flag or HdrS magic).
    #[error("not a valid bzImage (missing boot flag or HdrS magic)")]
    NotBzImage,
    /// The bzImage boot protocol version is too old.
    #[error(
        "bzImage boot protocol version {version:#06x} is too old (need >= 2.12 for 64-bit boot)"
    )]
    ProtocolTooOld {
        /// The detected protocol version.
        version: u16,
    },
    /// The kernel does not support being loaded high (at 0x100000).
    #[error("bzImage does not have LOADED_HIGH flag set")]
    NotLoadedHigh,
    /// The kernel does not have a 64-bit entry point.
    #[error("bzImage does not support 64-bit boot (XLF_KERNEL_64 not set in xloadflags)")]
    No64BitEntry,
    /// The bzImage is truncated — the protected-mode code is too small to
    /// contain the 64-bit entry point.
    #[error(
        "bzImage is truncated: protected-mode size ({size}) is too small for entry offset ({entry_offset})"
    )]
    Truncated {
        /// The size of the protected-mode code.
        size: u64,
        /// The required entry point offset.
        entry_offset: u64,
    },
}

/// Information parsed from a bzImage setup header, needed for loading.
#[derive(Debug, Clone)]
pub struct BzImageInfo {
    /// The setup header to copy into the zero page's `hdr` field.
    pub setup_header: defs::setup_header,
    /// Number of setup sectors (determines where protected-mode code starts).
    /// The protected-mode code begins at offset `(setup_sects + 1) * 512` in the file.
    pub setup_sects: u8,
    /// The total size in bytes of the protected-mode code (everything after the setup).
    pub protected_mode_size: u64,
    /// The 64-bit entry point offset relative to the start of the protected-mode code.
    /// For protocol >= 2.12 with XLF_KERNEL_64, this is at offset 0x200 from
    /// the start of the protected-mode code.
    pub entry_offset: u64,
    /// The `init_size` field — the amount of linear contiguous memory the
    /// kernel needs starting at the load address for initialization.
    pub init_size: u32,
}

/// Attempt to detect whether `kernel_image` is a bzImage.
///
/// Returns `true` if the image has a valid Linux setup header with the
/// "HdrS" magic. The file position is always restored to the beginning
/// before returning.
pub fn is_bzimage(kernel_image: &mut (impl Read + Seek)) -> Result<bool, Error> {
    kernel_image.seek(SeekFrom::Start(0)).map_err(Error::Io)?;

    let mut buf = [0u8; MIN_HEADER_SIZE];
    let result = kernel_image.read_exact(&mut buf);

    // Always restore position before checking the read result.
    kernel_image.seek(SeekFrom::Start(0)).map_err(Error::Io)?;

    match result {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(Error::Io(e)),
        Ok(()) => {}
    }

    let boot_flag = u16::from_le_bytes([buf[0x1fe], buf[0x1ff]]);
    let header_magic = u32::from_le_bytes([buf[0x202], buf[0x203], buf[0x204], buf[0x205]]);

    Ok(boot_flag == BOOT_FLAG && header_magic == HDRS_MAGIC)
}

/// Parse the bzImage setup header and return information needed for loading.
///
/// The file position of `kernel_image` is restored to the beginning on
/// success. On error, restoration is best-effort (the seek-back is
/// attempted but its failure is ignored in favor of the parse error).
pub fn parse_bzimage(kernel_image: &mut (impl Read + Seek)) -> Result<BzImageInfo, Error> {
    kernel_image.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    let result = parse_bzimage_inner(kernel_image);
    let _ = kernel_image.seek(SeekFrom::Start(0));
    result
}

fn parse_bzimage_inner(kernel_image: &mut (impl Read + Seek)) -> Result<BzImageInfo, Error> {
    let mut buf = [0u8; MIN_HEADER_SIZE];
    kernel_image.read_exact(&mut buf).map_err(Error::Io)?;

    // Validate the bzImage identifying markers before parsing fields.
    let boot_flag = u16::from_le_bytes([buf[0x1fe], buf[0x1ff]]);
    let header_magic = u32::from_le_bytes([buf[0x202], buf[0x203], buf[0x204], buf[0x205]]);
    if boot_flag != BOOT_FLAG || header_magic != HDRS_MAGIC {
        return Err(Error::NotBzImage);
    }

    // The setup_header in boot_params starts at offset 0x1F1 relative to
    // the start of the boot sector.
    let hdr = defs::setup_header::read_from_bytes(
        &buf[SETUP_HEADER_OFFSET..SETUP_HEADER_OFFSET + size_of::<defs::setup_header>()],
    )
    .expect("buf is large enough: MIN_HEADER_SIZE is derived from setup_header size");

    let version: u16 = hdr.version.into();
    if version < MIN_PROTOCOL_VERSION {
        return Err(Error::ProtocolTooOld { version });
    }

    let loadflags: u8 = hdr.loadflags;
    if loadflags & LOADED_HIGH == 0 {
        return Err(Error::NotLoadedHigh);
    }

    let xloadflags: u16 = hdr.xloadflags.into();
    if xloadflags & XLF_KERNEL_64 == 0 {
        return Err(Error::No64BitEntry);
    }

    let setup_sects = if hdr.setup_sects == 0 {
        4
    } else {
        hdr.setup_sects
    };
    let protected_mode_offset = (setup_sects as u64 + 1) * 512;

    // Use the syssize field from the header (size in 16-byte paragraphs)
    // for a precise payload size. Fall back to file size if syssize is zero.
    let syssize: u32 = hdr.syssize.into();
    let protected_mode_size = if syssize != 0 {
        syssize as u64 * 16
    } else {
        let file_size = kernel_image.seek(SeekFrom::End(0)).map_err(Error::Io)?;
        file_size.saturating_sub(protected_mode_offset)
    };

    // For 64-bit boot protocol, the 64-bit entry point is at offset 0x200
    // from the beginning of the protected-mode code.
    let entry_offset: u64 = 0x200;
    if protected_mode_size <= entry_offset {
        return Err(Error::Truncated {
            size: protected_mode_size,
            entry_offset,
        });
    }

    let init_size: u32 = hdr.init_size.into();

    tracing::debug!(
        version = format_args!("{:#06x}", version),
        setup_sects,
        protected_mode_offset,
        protected_mode_size,
        init_size,
        "parsed bzImage setup header"
    );

    Ok(BzImageInfo {
        setup_header: hdr,
        setup_sects,
        protected_mode_size,
        entry_offset,
        init_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Build a minimal synthetic bzImage for testing.
    fn make_test_bzimage() -> Vec<u8> {
        let setup_sects: u8 = 1;
        let protected_mode_offset = (setup_sects as u32 + 1) * 512;
        // Some fake protected-mode code (1024 bytes).
        let pm_code = vec![0xCC; 1024];

        let total_size = protected_mode_offset as usize + pm_code.len();
        let mut image = vec![0u8; total_size];

        // setup_sects at 0x1f1
        image[0x1f1] = setup_sects;
        // boot_flag at 0x1fe = 0xAA55
        image[0x1fe..0x200].copy_from_slice(&BOOT_FLAG.to_le_bytes());
        // header magic "HdrS" at 0x202
        image[0x202..0x206].copy_from_slice(&HDRS_MAGIC.to_le_bytes());
        // version at 0x206 = 0x020f (protocol 2.15)
        image[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        // loadflags at 0x211: LOADED_HIGH
        image[0x211] = LOADED_HIGH;
        // xloadflags at 0x236: XLF_KERNEL_64
        image[0x236..0x238].copy_from_slice(&XLF_KERNEL_64.to_le_bytes());
        // pref_address at 0x258 = 0x1000000 (16MB)
        image[0x258..0x260].copy_from_slice(&0x1000000u64.to_le_bytes());
        // syssize at 0x1f4 = pm_code size in 16-byte paragraphs
        let syssize = (pm_code.len() as u32) / 16;
        image[0x1f4..0x1f8].copy_from_slice(&syssize.to_le_bytes());
        // init_size at 0x260 = 0x1000000 (16MB)
        image[0x260..0x264].copy_from_slice(&0x1000000u32.to_le_bytes());

        // Write the protected-mode code.
        image[protected_mode_offset as usize..].copy_from_slice(&pm_code);

        image
    }

    #[test]
    fn test_is_bzimage_positive() {
        let bzimage = make_test_bzimage();
        let mut cursor = Cursor::new(bzimage);
        assert!(is_bzimage(&mut cursor).unwrap());
    }

    #[test]
    fn test_is_bzimage_negative_elf() {
        let mut elf = vec![0u8; 0x1000];
        elf[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        let mut cursor = Cursor::new(elf);
        assert!(!is_bzimage(&mut cursor).unwrap());
    }

    #[test]
    fn test_parse_bzimage() {
        let bzimage = make_test_bzimage();
        let mut cursor = Cursor::new(bzimage);

        let info = parse_bzimage(&mut cursor).unwrap();
        assert_eq!(info.setup_sects, 1);
        assert_eq!(info.protected_mode_size, 1024);
        assert_eq!(info.entry_offset, 0x200);
        assert_eq!(info.init_size, 0x1000000);
    }

    #[test]
    fn test_not_bzimage_returns_false() {
        let mut elf = vec![0u8; 0x1000];
        elf[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        let mut cursor = Cursor::new(elf);
        assert!(!is_bzimage(&mut cursor).unwrap());
    }
}
