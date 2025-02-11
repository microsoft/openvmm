// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to parse bootorder-related nvram variables.

use guid::Guid;
use std::ffi::CStr;
use thiserror::Error;
use ucs2::Ucs2LeSlice;
use uefi_specs::uefi::boot;
use zerocopy::FromBytes;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Data length incorrect or inconsistent with other fields")]
    InvalidLength,
    #[error("Missing null-termination")]
    NullTerminated(#[source] std::ffi::FromBytesUntilNulError),
    #[error("Invalid Ucs2 string")]
    InvalidUcs2(#[source] ucs2::Ucs2ParseError),
    #[error("Invalid UTF-8 string")]
    InvalidUtf8(#[source] std::str::Utf8Error),
    #[error("Device path end structure missing or corrupted")]
    DevicePathEnd,
}

#[derive(Debug, PartialEq)]
pub enum HardwareDevice<'a> {
    MemoryMapped(boot::EfiMemoryMappedDevice),
    Vendor {
        vendor_guid: Guid,
        data: &'a [u8],
    },
    Unknown {
        device_subtype: boot::EfiHardwareDeviceSubType,
        path_data: &'a [u8],
    },
}

#[derive(Debug, PartialEq)]
pub enum AcpiDevice<'a> {
    ExpandedAcpi {
        numeric: boot::EfiExpandedAcpiDevice,
        hidstr: &'a CStr,
        uidstr: &'a CStr,
        cidstr: &'a CStr,
    },
    Unknown {
        device_subtype: boot::EfiAcpiDeviceSubType,
        path_data: &'a [u8],
    },
}

#[derive(Debug, PartialEq)]
pub enum MessagingDevice<'a> {
    Scsi(boot::EfiScsiDevice),
    Unknown {
        device_subtype: boot::EfiMessagingDeviceSubType,
        path_data: &'a [u8],
    },
}

#[derive(Debug, PartialEq)]
pub enum MediaDevice<'a> {
    HardDrive(boot::EfiHardDriveDevice),
    File(&'a Ucs2LeSlice),
    PiwgFirmwareFile(Guid),
    PiwgFirmwareVolume(Guid),
    Unknown {
        device_subtype: boot::EfiMediaDeviceSubType,
        path_data: &'a [u8],
    },
}

#[derive(Debug, PartialEq)]
pub enum EndDevice<'a> {
    Instance,
    Entire,
    Unknown {
        device_subtype: boot::EfiEndDeviceSubType,
        path_data: &'a [u8],
    },
}

#[derive(Debug)]
pub enum EfiDevicePathProtocol<'a> {
    Hardware(HardwareDevice<'a>),
    Acpi(AcpiDevice<'a>),
    Messaging(MessagingDevice<'a>),
    Media(MediaDevice<'a>),
    End(EndDevice<'a>),
    Unknown {
        device_type: boot::EfiDeviceType,
        device_subtype: u8,
        path_data: &'a [u8],
    },
}

impl<'a> EfiDevicePathProtocol<'a> {
    pub fn parse(data: &'a [u8]) -> Result<(Self, &'a [u8]), Error> {
        let (header, path_data) = boot::EfiDevicePathProtocol::read_from_prefix(data)
            .map_err(|_| Error::InvalidLength)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let length = u16::from_le_bytes(header.length) as usize;
        // TODO: Switch to split_at_checked below once stable and remove this check
        if data.len() < length {
            return Err(Error::InvalidLength);
        }

        let (path_data, remaining) = path_data.split_at(
            length
                .checked_sub(size_of::<boot::EfiDevicePathProtocol>())
                .ok_or(Error::InvalidLength)?,
        );

        Ok((
            match header.device_type {
                boot::EfiDeviceType::HARDWARE => EfiDevicePathProtocol::Hardware(
                    match boot::EfiHardwareDeviceSubType(header.sub_type) {
                        boot::EfiHardwareDeviceSubType::MEMORY_MAPPED => {
                            HardwareDevice::MemoryMapped(
                                boot::EfiMemoryMappedDevice::read_from_bytes(path_data)
                                    .map_err(|_| Error::InvalidLength)?, // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                            )
                        }
                        boot::EfiHardwareDeviceSubType::VENDOR => {
                            let (vendor_guid, path_data) = Guid::read_from_prefix(path_data)
                                .map_err(|_| Error::InvalidLength)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                            HardwareDevice::Vendor {
                                vendor_guid,
                                data: path_data,
                            }
                        }
                        device_subtype => HardwareDevice::Unknown {
                            device_subtype,
                            path_data,
                        },
                    },
                ),
                boot::EfiDeviceType::ACPI => {
                    EfiDevicePathProtocol::Acpi(match boot::EfiAcpiDeviceSubType(header.sub_type) {
                        boot::EfiAcpiDeviceSubType::EXPANDED_ACPI => {
                            // minimum length is numeric representation + 3 null terminators from empty strings
                            if path_data.len() < size_of::<boot::EfiExpandedAcpiDevice>() + 3 {
                                return Err(Error::InvalidLength);
                            }
                            let (numeric, path_data) =
                                boot::EfiExpandedAcpiDevice::read_from_prefix(path_data).unwrap(); // TODO: zerocopy: unwrap (https://github.com/microsoft/openvmm/issues/759)
                            let hidstr = CStr::from_bytes_until_nul(path_data)
                                .map_err(Error::NullTerminated)?;
                            let path_data = &path_data[hidstr.to_bytes_with_nul().len()..];
                            let uidstr = CStr::from_bytes_until_nul(path_data)
                                .map_err(Error::NullTerminated)?;
                            let path_data = &path_data[uidstr.to_bytes_with_nul().len()..];
                            let cidstr = CStr::from_bytes_until_nul(path_data)
                                .map_err(Error::NullTerminated)?;
                            if cidstr.to_bytes_with_nul().len() != path_data.len() {
                                return Err(Error::InvalidLength);
                            }
                            AcpiDevice::ExpandedAcpi {
                                numeric,
                                hidstr,
                                uidstr,
                                cidstr,
                            }
                        }
                        device_subtype => AcpiDevice::Unknown {
                            device_subtype,
                            path_data,
                        },
                    })
                }
                boot::EfiDeviceType::MESSAGING => EfiDevicePathProtocol::Messaging(
                    match boot::EfiMessagingDeviceSubType(header.sub_type) {
                        boot::EfiMessagingDeviceSubType::SCSI => MessagingDevice::Scsi(
                            boot::EfiScsiDevice::read_from_bytes(path_data)
                                .map_err(|_| Error::InvalidLength)?, // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                        ),
                        device_subtype => MessagingDevice::Unknown {
                            device_subtype,
                            path_data,
                        },
                    },
                ),
                boot::EfiDeviceType::MEDIA => EfiDevicePathProtocol::Media(
                    match boot::EfiMediaDeviceSubType(header.sub_type) {
                        boot::EfiMediaDeviceSubType::HARD_DRIVE => MediaDevice::HardDrive(
                            boot::EfiHardDriveDevice::read_from_bytes(path_data)
                                .map_err(|_| Error::InvalidLength)?, // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                        ),
                        boot::EfiMediaDeviceSubType::FILE => {
                            let file_name = Ucs2LeSlice::from_slice_with_nul(path_data)
                                .map_err(Error::InvalidUcs2)?;
                            if file_name.as_bytes().len() != path_data.len() {
                                return Err(Error::InvalidLength);
                            }
                            MediaDevice::File(file_name)
                        }
                        boot::EfiMediaDeviceSubType::PIWG_FIRMWARE_FILE => {
                            MediaDevice::PiwgFirmwareFile(
                                Guid::read_from_bytes(path_data)
                                    .map_err(|_| Error::InvalidLength)?, // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                            )
                        }
                        boot::EfiMediaDeviceSubType::PIWG_FIRMWARE_VOLUME => {
                            MediaDevice::PiwgFirmwareVolume(
                                Guid::read_from_bytes(path_data)
                                    .map_err(|_| Error::InvalidLength)?, // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                            )
                        }
                        device_subtype => MediaDevice::Unknown {
                            device_subtype,
                            path_data,
                        },
                    },
                ),
                boot::EfiDeviceType::END => {
                    EfiDevicePathProtocol::End(match boot::EfiEndDeviceSubType(header.sub_type) {
                        boot::EfiEndDeviceSubType::INSTANCE => EndDevice::Instance,
                        boot::EfiEndDeviceSubType::ENTIRE => EndDevice::Entire,
                        device_subtype => EndDevice::Unknown {
                            device_subtype,
                            path_data,
                        },
                    })
                }
                device_type => EfiDevicePathProtocol::Unknown {
                    device_type,
                    device_subtype: header.sub_type,
                    path_data,
                },
            },
            remaining,
        ))
    }
}

#[derive(Debug)]
pub struct EfiLoadOption<'a> {
    pub attributes: u32,
    pub description: &'a Ucs2LeSlice,
    pub device_paths: Vec<EfiDevicePathProtocol<'a>>,
    pub opt: Option<&'a [u8]>,
}

impl<'a> EfiLoadOption<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        let (header, data) =
            boot::EfiLoadOption::read_from_prefix(data).map_err(|_| Error::InvalidLength)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        let description = Ucs2LeSlice::from_slice_with_nul(data).map_err(Error::InvalidUcs2)?;
        let mut data = &data[description.as_bytes().len()..];

        let mut device_paths = Vec::new();
        loop {
            if data.len() < size_of::<boot::EfiDevicePathProtocol>() {
                return Err(Error::DevicePathEnd);
            }
            let path;
            (path, data) = EfiDevicePathProtocol::parse(data)?;
            match path {
                EfiDevicePathProtocol::End(end) => match end {
                    EndDevice::Instance => continue,
                    EndDevice::Entire => break,
                    _ => return Err(Error::DevicePathEnd),
                },
                path => device_paths.push(path),
            }
        }

        let opt = if !data.is_empty() { Some(data) } else { None };

        Ok(EfiLoadOption {
            attributes: header.attributes,
            description,
            device_paths,
            opt,
        })
    }
}

pub fn parse_boot_order(data: &[u8]) -> Result<impl Iterator<Item = u16> + '_, Error> {
    let boot_order_iter = data.chunks_exact(2);
    if !boot_order_iter.remainder().is_empty() {
        return Err(Error::InvalidLength);
    }
    Ok(boot_order_iter.map(|x| u16::from_le_bytes(x.try_into().unwrap())))
}
