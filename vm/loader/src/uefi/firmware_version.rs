// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSVM firmware build and interface version parsing.

use core::mem::size_of;
use thiserror::Error;
use uefi_specs::hyperv::firmware_version;
use uefi_specs::uefi::firmware_volume;
use zerocopy::FromBytes;

const FILE_ALIGNMENT: usize = 8;
const SECTION_ALIGNMENT: usize = 4;
const ERASED_BYTE: u8 = 0xff;

/// A parsed view of an MSVM firmware version record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsvmFirmwareVersion<'a> {
    pub struct_version: u16,
    pub header_size: u16,
    pub flags: u32,
    pub interface_version_major: u16,
    pub interface_version_minor: u16,
    pub base_version: &'a str,
    pub git_commit: &'a str,
}

/// A malformed MSVM firmware version file or record.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    #[error("invalid firmware volume header")]
    VolumeHeader,
    #[error("invalid firmware file")]
    File,
    #[error("invalid firmware section")]
    Section,
    #[error("firmware version file has no RAW section")]
    MissingRawSection,
    #[error("firmware version record is truncated")]
    TruncatedRecord,
    #[error("invalid firmware version record signature")]
    InvalidSignature,
    #[error("unsupported firmware version structure version {0}")]
    UnsupportedStructVersion(u16),
    #[error("invalid firmware version record header size {0}")]
    InvalidHeaderSize(u16),
    #[error("firmware version field {0} is not NUL-terminated ASCII")]
    InvalidString(&'static str),
}

fn size24(value: [u8; 3]) -> usize {
    usize::from(value[0]) | usize::from(value[1]) << 8 | usize::from(value[2]) << 16
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn find_file(fv: &[u8], name: guid::Guid) -> Result<Option<&[u8]>, Error> {
    let header = firmware_volume::FirmwareVolumeHeader::read_from_prefix(fv)
        .map_err(|_| Error::VolumeHeader)?
        .0;
    if header.signature != firmware_volume::FV_SIGNATURE {
        return Err(Error::VolumeHeader);
    }
    let length = usize::try_from(header.fv_length)
        .ok()
        .filter(|&length| length <= fv.len())
        .ok_or(Error::VolumeHeader)?;
    let mut offset = align_up(usize::from(header.header_length), FILE_ALIGNMENT)
        .filter(|&offset| offset <= length)
        .ok_or(Error::VolumeHeader)?;

    while offset
        .checked_add(size_of::<firmware_volume::FfsFileHeader>())
        .is_some_and(|end| end <= length)
    {
        let header_bytes = &fv[offset..offset + size_of::<firmware_volume::FfsFileHeader>()];
        if header_bytes.iter().all(|&byte| byte == ERASED_BYTE) {
            return Ok(None);
        }
        let header = firmware_volume::FfsFileHeader::read_from_prefix(header_bytes)
            .map_err(|_| Error::File)?
            .0;
        let file_size = size24(header.size);
        if file_size < size_of::<firmware_volume::FfsFileHeader>() {
            return Err(Error::File);
        }
        let end = offset
            .checked_add(file_size)
            .filter(|&end| end <= length)
            .ok_or(Error::File)?;
        if header.name == name {
            return Ok(Some(&fv[offset..end]));
        }
        offset = align_up(end, FILE_ALIGNMENT).ok_or(Error::File)?;
    }
    Ok(None)
}

fn find_unique_section(file: &[u8], section_type: u8) -> Result<Option<&[u8]>, Error> {
    let mut offset = size_of::<firmware_volume::FfsFileHeader>();
    let mut found = None;
    while offset < file.len() {
        offset = align_up(offset, SECTION_ALIGNMENT).ok_or(Error::Section)?;
        let header = firmware_volume::CommonSectionHeader::read_from_prefix(
            file.get(offset..).ok_or(Error::Section)?,
        )
        .map_err(|_| Error::Section)?
        .0;
        let section_size = size24(header.size);
        if section_size < size_of::<firmware_volume::CommonSectionHeader>() {
            return Err(Error::Section);
        }
        let end = offset
            .checked_add(section_size)
            .filter(|&end| end <= file.len())
            .ok_or(Error::Section)?;
        if header.section_type == section_type {
            if found.is_some() {
                return Err(Error::Section);
            }
            found = Some(&file[offset + size_of::<firmware_volume::CommonSectionHeader>()..end]);
        }
        offset = end;
    }
    Ok(found)
}

fn parse_ascii<'a>(value: &'a [u8], field: &'static str) -> Result<&'a str, Error> {
    let nul = value
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(Error::InvalidString(field))?;
    let value = &value[..nul];
    if !value.is_ascii() {
        return Err(Error::InvalidString(field));
    }
    Ok(core::str::from_utf8(value).expect("ASCII is valid UTF-8"))
}

fn parse_record(record: &[u8]) -> Result<MsvmFirmwareVersion<'_>, Error> {
    let info = firmware_version::MsvmFirmwareVersionInfo::ref_from_prefix(record)
        .map_err(|_| Error::TruncatedRecord)?
        .0;
    if &info.signature != firmware_version::SIGNATURE {
        return Err(Error::InvalidSignature);
    }
    let struct_version = u16::from_le_bytes(info.struct_version);
    if struct_version < firmware_version::STRUCT_VERSION {
        return Err(Error::UnsupportedStructVersion(struct_version));
    }
    let header_size = u16::from_le_bytes(info.header_size);
    if usize::from(header_size) < firmware_version::HEADER_SIZE
        || usize::from(header_size) > record.len()
    {
        return Err(Error::InvalidHeaderSize(header_size));
    }
    Ok(MsvmFirmwareVersion {
        struct_version,
        header_size,
        flags: u32::from_le_bytes(info.flags),
        interface_version_major: u16::from_le_bytes(info.interface_version_major),
        interface_version_minor: u16::from_le_bytes(info.interface_version_minor),
        base_version: parse_ascii(&info.base_version, "base version")?,
        git_commit: parse_ascii(&info.git_commit, "git commit")?,
    })
}

/// Finds and parses the version record in a DXE firmware volume.
///
/// `Ok(None)` means the dedicated version FFS file is not present.
pub fn find_in_firmware_volume(fv: &[u8]) -> Result<Option<MsvmFirmwareVersion<'_>>, Error> {
    let Some(file) = find_file(fv, firmware_version::FILE_GUID)? else {
        return Ok(None);
    };
    let record =
        find_unique_section(file, firmware_volume::SECTION_RAW)?.ok_or(Error::MissingRawSection)?;
    parse_record(record).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::IntoBytes;

    fn size24(value: usize) -> [u8; 3] {
        let value = (value as u32).to_le_bytes();
        [value[0], value[1], value[2]]
    }

    fn record(major: u16, minor: u16) -> [u8; firmware_version::HEADER_SIZE] {
        let mut record = [0; firmware_version::HEADER_SIZE];
        record[0..4].copy_from_slice(firmware_version::SIGNATURE);
        record[4..6].copy_from_slice(&firmware_version::STRUCT_VERSION.to_le_bytes());
        record[6..8].copy_from_slice(&(firmware_version::HEADER_SIZE as u16).to_le_bytes());
        record[8..12].copy_from_slice(
            &(firmware_version::FLAG_DIRTY | firmware_version::FLAG_OFFICIAL).to_le_bytes(),
        );
        record[12..14].copy_from_slice(&major.to_le_bytes());
        record[14..16].copy_from_slice(&minor.to_le_bytes());
        record[16..21].copy_from_slice(b"26.0\0");
        record[32..40].copy_from_slice(b"deadbee\0");
        record
    }

    #[test]
    fn parses_record() {
        let record = record(1, 2);
        let version = parse_record(&record).unwrap();
        assert_eq!(version.interface_version_major, 1);
        assert_eq!(version.interface_version_minor, 2);
        assert_eq!(version.base_version, "26.0");
        assert_eq!(version.git_commit, "deadbee");
        assert_eq!(
            version.flags,
            firmware_version::FLAG_DIRTY | firmware_version::FLAG_OFFICIAL
        );
    }

    #[test]
    fn rejects_truncated_record() {
        assert_eq!(parse_record(b"MVFW").unwrap_err(), Error::TruncatedRecord);
    }

    #[test]
    fn rejects_non_ascii_string() {
        let mut record = record(1, 0);
        record[16..19].copy_from_slice(b"\xc3\xa9\0");
        assert_eq!(
            parse_record(&record).unwrap_err(),
            Error::InvalidString("base version")
        );
    }

    #[test]
    fn finds_record_by_ffs_file_guid() {
        let record = record(1, 0);
        let section_size = size_of::<firmware_volume::CommonSectionHeader>() + record.len();
        let file_size = size_of::<firmware_volume::FfsFileHeader>() + section_size;
        let file_offset = size_of::<firmware_volume::FirmwareVolumeHeader>().next_multiple_of(8);
        let mut fv = [0xff; 0x1000];
        let fv_length = fv.len() as u64;
        fv[32..40].copy_from_slice(&fv_length.to_le_bytes());
        fv[40..44].copy_from_slice(b"_FVH");
        fv[48..50].copy_from_slice(
            &(size_of::<firmware_volume::FirmwareVolumeHeader>() as u16).to_le_bytes(),
        );
        fv[file_offset..file_offset + 16].copy_from_slice(firmware_version::FILE_GUID.as_bytes());
        fv[file_offset + 20..file_offset + 23].copy_from_slice(&size24(file_size));
        let section_offset = file_offset + size_of::<firmware_volume::FfsFileHeader>();
        fv[section_offset..section_offset + 3].copy_from_slice(&size24(section_size));
        fv[section_offset + 3] = firmware_volume::SECTION_RAW;
        fv[section_offset + size_of::<firmware_volume::CommonSectionHeader>()
            ..file_offset + file_size]
            .copy_from_slice(&record);

        let version = find_in_firmware_volume(&fv).unwrap().unwrap();
        assert_eq!(version.interface_version_major, 1);
        assert_eq!(version.interface_version_minor, 0);
    }
}
