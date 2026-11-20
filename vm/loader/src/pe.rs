// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Lays out an x86-64 PE/COFF image, such as a Linux bzImage with an EFI
//! stub, at its section addresses.
//!
//! This supports only images that can run at any base address without
//! relocation. The Linux EFI stub is position independent and has an empty
//! relocation directory.

use hvdef::HV_PAGE_SIZE;
use object::LittleEndian as LE;
use object::pe;
use object::read::pe::ImageNtHeaders;
use object::read::pe::ImageOptionalHeader;
use object::read::pe::PeFile64;
use thiserror::Error;

/// An error from laying out a PE/COFF image.
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid PE32+ image")]
    Parse(#[source] object::read::Error),
    #[error("the PE/COFF image is not for x86-64 (machine {0:#x})")]
    WrongMachine(u16),
    #[error("the PE/COFF section alignment {0:#x} is not a power of two of at least 4 KiB")]
    BadSectionAlignment(u32),
    #[error("the PE/COFF headers lie outside the image")]
    BadHeaders,
    #[error("PE/COFF section {0} lies outside the image or the file")]
    BadSection(usize),
    #[error("the PE/COFF entry point lies outside the image")]
    BadEntryPoint,
    #[error("the PE/COFF image needs relocation, which is not supported")]
    NeedsRelocation,
}

/// A PE/COFF image laid out at its section addresses.
#[derive(Debug)]
pub struct PeImage {
    /// The image, `SizeOfImage` bytes long, rounded up to a whole page.
    pub image: Vec<u8>,
    /// The entry point, as an offset from the start of the image.
    pub entry_offset: u64,
    /// The alignment that the image base needs.
    pub alignment: u64,
}

/// Returns `true` if `file` is a PE32+ image.
pub fn is_pe(file: &[u8]) -> bool {
    PeFile64::parse(file).is_ok()
}

/// Lays out `file` at its section addresses.
pub fn layout(file: &[u8]) -> Result<PeImage, Error> {
    let pe_file = PeFile64::parse(file).map_err(Error::Parse)?;
    let headers = pe_file.nt_headers();
    let machine = headers.file_header().machine.get(LE);
    if machine != pe::IMAGE_FILE_MACHINE_AMD64 {
        return Err(Error::WrongMachine(machine));
    }
    let optional = headers.optional_header();
    let section_alignment = optional.section_alignment();
    if !section_alignment.is_power_of_two() || u64::from(section_alignment) < HV_PAGE_SIZE {
        return Err(Error::BadSectionAlignment(section_alignment));
    }

    let size_of_image = optional.size_of_image() as usize;
    let size_of_headers = optional.size_of_headers() as usize;
    let headers = file
        .get(..size_of_headers)
        .filter(|_| size_of_headers <= size_of_image)
        .ok_or(Error::BadHeaders)?;
    let mut image = vec![0; size_of_image.next_multiple_of(HV_PAGE_SIZE as usize)];
    image[..size_of_headers].copy_from_slice(headers);

    let sections = pe_file.section_table();
    for (index, section) in sections.iter().enumerate() {
        // `pe_data` returns the part of the raw data within the virtual size,
        // without file-alignment padding. The rest of the section is zero.
        let data = section
            .pe_data(file)
            .map_err(|_| Error::BadSection(index))?;
        let (start, virtual_size) = section.pe_address_range();
        let start = start as usize;
        if start
            .checked_add((virtual_size as usize).max(data.len()))
            .is_none_or(|end| end > size_of_image)
        {
            return Err(Error::BadSection(index));
        }
        image[start..start + data.len()].copy_from_slice(data);
    }

    let entry_offset = optional.address_of_entry_point() as usize;
    if entry_offset == 0 || entry_offset >= size_of_image {
        return Err(Error::BadEntryPoint);
    }

    // Padding entries have type IMAGE_REL_BASED_ABSOLUTE. Any other entry
    // needs the image to be relocated.
    if let Some(mut blocks) = pe_file
        .data_directories()
        .relocation_blocks(file, &sections)
        .map_err(|_| Error::NeedsRelocation)?
    {
        while let Some(block) = blocks.next().map_err(|_| Error::NeedsRelocation)? {
            if block
                .into_iter()
                .any(|relocation| relocation.typ != pe::IMAGE_REL_BASED_ABSOLUTE)
            {
                return Err(Error::NeedsRelocation);
            }
        }
    }

    Ok(PeImage {
        image,
        entry_offset: entry_offset as u64,
        alignment: section_alignment.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use test_with_tracing::test;

    const PE_OFFSET: usize = 0x40;
    const OPTIONAL_OFFSET: usize = PE_OFFSET + 4 + size_of::<pe::ImageFileHeader>();
    const DIRECTORY_COUNT: usize = pe::IMAGE_DIRECTORY_ENTRY_BASERELOC + 1;
    const SECTIONS_OFFSET: usize = OPTIONAL_OFFSET
        + size_of::<pe::ImageOptionalHeader64>()
        + DIRECTORY_COUNT * size_of::<pe::ImageDataDirectory>();
    const RELOCATION_DIRECTORY: usize = OPTIONAL_OFFSET
        + size_of::<pe::ImageOptionalHeader64>()
        + pe::IMAGE_DIRECTORY_ENTRY_BASERELOC * size_of::<pe::ImageDataDirectory>();

    fn put(file: &mut [u8], offset: usize, bytes: &[u8]) {
        file[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// Builds an image with a header page, a `.text` section whose raw data
    /// is longer than its virtual size, and a `.data` section with a BSS tail.
    fn test_image() -> Vec<u8> {
        let mut file = vec![0; 0x3000];
        put(&mut file, 0, &pe::IMAGE_DOS_SIGNATURE.to_le_bytes());
        put(
            &mut file,
            offset_of!(pe::ImageDosHeader, e_lfanew),
            &(PE_OFFSET as u32).to_le_bytes(),
        );
        put(&mut file, PE_OFFSET, &pe::IMAGE_NT_SIGNATURE.to_le_bytes());
        let coff = PE_OFFSET + 4;
        put(&mut file, coff, &pe::IMAGE_FILE_MACHINE_AMD64.to_le_bytes());
        put(
            &mut file,
            coff + offset_of!(pe::ImageFileHeader, number_of_sections),
            &2u16.to_le_bytes(),
        );
        let optional_size = (SECTIONS_OFFSET - OPTIONAL_OFFSET) as u16;
        put(
            &mut file,
            coff + offset_of!(pe::ImageFileHeader, size_of_optional_header),
            &optional_size.to_le_bytes(),
        );
        let o = OPTIONAL_OFFSET;
        put(
            &mut file,
            o,
            &pe::IMAGE_NT_OPTIONAL_HDR64_MAGIC.to_le_bytes(),
        );
        put(
            &mut file,
            o + offset_of!(pe::ImageOptionalHeader64, address_of_entry_point),
            &0x1010u32.to_le_bytes(),
        );
        put(
            &mut file,
            o + offset_of!(pe::ImageOptionalHeader64, section_alignment),
            &0x1000u32.to_le_bytes(),
        );
        put(
            &mut file,
            o + offset_of!(pe::ImageOptionalHeader64, file_alignment),
            &0x200u32.to_le_bytes(),
        );
        put(
            &mut file,
            o + offset_of!(pe::ImageOptionalHeader64, size_of_image),
            &0x4000u32.to_le_bytes(),
        );
        put(
            &mut file,
            o + offset_of!(pe::ImageOptionalHeader64, size_of_headers),
            &0x1000u32.to_le_bytes(),
        );
        put(
            &mut file,
            o + offset_of!(pe::ImageOptionalHeader64, number_of_rva_and_sizes),
            &(DIRECTORY_COUNT as u32).to_le_bytes(),
        );

        for (index, (va, virtual_size, raw_ptr, raw_size)) in [
            (0x1000u32, 0x10u32, 0x1000u32, 0x200u32),
            (0x2000, 0x1800, 0x2000, 0x1000),
        ]
        .into_iter()
        .enumerate()
        {
            let s = SECTIONS_OFFSET + index * size_of::<pe::ImageSectionHeader>();
            put(
                &mut file,
                s + offset_of!(pe::ImageSectionHeader, virtual_size),
                &virtual_size.to_le_bytes(),
            );
            put(
                &mut file,
                s + offset_of!(pe::ImageSectionHeader, virtual_address),
                &va.to_le_bytes(),
            );
            put(
                &mut file,
                s + offset_of!(pe::ImageSectionHeader, size_of_raw_data),
                &raw_size.to_le_bytes(),
            );
            put(
                &mut file,
                s + offset_of!(pe::ImageSectionHeader, pointer_to_raw_data),
                &raw_ptr.to_le_bytes(),
            );
        }
        file[0x1000..0x1200].fill(0xaa);
        file[0x2000..0x3000].fill(0xbb);
        file
    }

    #[test]
    fn lays_out_sections() {
        let file = test_image();
        assert!(is_pe(&file));
        let pe = layout(&file).unwrap();
        assert_eq!(pe.image.len(), 0x4000);
        assert_eq!(pe.entry_offset, 0x1010);
        assert_eq!(pe.alignment, 0x1000);
        assert_eq!(pe.image[..0x1000], file[..0x1000]);
        // Only the virtual size of `.text` is copied.
        assert!(pe.image[0x1000..0x1010].iter().all(|&b| b == 0xaa));
        assert!(pe.image[0x1010..0x2000].iter().all(|&b| b == 0));
        // The `.data` BSS tail is zero.
        assert!(pe.image[0x2000..0x3000].iter().all(|&b| b == 0xbb));
        assert!(pe.image[0x3000..].iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_non_pe_and_wrong_machine() {
        assert!(!is_pe(&[0; 0x100]));
        assert!(matches!(layout(&[0; 0x100]), Err(Error::Parse(_))));
        let mut file = test_image();
        put(
            &mut file,
            PE_OFFSET + 4,
            &pe::IMAGE_FILE_MACHINE_I386.to_le_bytes(),
        );
        assert!(matches!(
            layout(&file),
            Err(Error::WrongMachine(pe::IMAGE_FILE_MACHINE_I386))
        ));
    }

    #[test]
    fn rejects_section_outside_image() {
        let mut file = test_image();
        put(
            &mut file,
            SECTIONS_OFFSET
                + size_of::<pe::ImageSectionHeader>()
                + offset_of!(pe::ImageSectionHeader, virtual_size),
            &0x3000u32.to_le_bytes(),
        );
        assert!(matches!(layout(&file), Err(Error::BadSection(1))));
    }

    #[test]
    fn accepts_padding_relocations_and_rejects_real_ones() {
        let mut file = test_image();
        put(&mut file, RELOCATION_DIRECTORY, &0x2100u32.to_le_bytes());
        put(&mut file, RELOCATION_DIRECTORY + 4, &12u32.to_le_bytes());
        // One block with two absolute (padding) entries.
        put(&mut file, 0x2100, &0x1000u32.to_le_bytes());
        put(&mut file, 0x2104, &12u32.to_le_bytes());
        put(&mut file, 0x2108, &0x0008u16.to_le_bytes());
        put(&mut file, 0x210a, &0u16.to_le_bytes());
        layout(&file).unwrap();

        // A DIR64 entry needs relocation.
        put(&mut file, 0x2108, &0xa008u16.to_le_bytes());
        assert!(matches!(layout(&file), Err(Error::NeedsRelocation)));
    }
}
