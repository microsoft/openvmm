// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenHCL configuration schema V1
//!
//! The schema defined in this file is for VTL2 settings. This schema is a protocol between
//! OpenHCL and client like Azure agent, which is opaque to WMI, so there is no corresponding
//! definitions in MARS files.

use super::ParseSchemaExt;
use crate::errors::ErrorContext;
use crate::errors::ParseErrors;
use crate::schema::ParseResultExt;
use crate::schema::ParseSchema;
use crate::schema::ParsingStopped;
use crate::Vtl2SettingsErrorCode;
use crate::Vtl2SettingsErrorInfo;
use guid::Guid;
use physical_device::DeviceType;
use std::error::Error as _;
use std::fmt::Write;
use storage_controller::StorageProtocol;
use thiserror::Error;
use vtl2_settings_proto::*;

pub(crate) const NAMESPACE_BASE: &str = "Base";
pub(crate) const NAMESPACE_NETWORK_DEVICE: &str = "NetworkDevice";
pub(crate) const NAMESPACE_NETWORK_ACCELERATION: &str = "NetworkAcceleration";

#[derive(Error, Debug)]
pub(crate) enum Error<'a> {
    #[error("unsupported schema version {0:#x}")]
    UnsupportedSchemaVersion(u32),
    #[error("unsupported schema namespace {0}")]
    UnsupportedSchemaNamespace(&'a str),
    #[error("empty namespace settings chunk {0}")]
    EmptyNamespaceChunk(&'a str),
    #[error("invalid instance ID '{0}'")]
    InvalidInstanceId(&'a str, #[source] guid::ParseError),
    #[error("invalid ntfs guid '{0}'")]
    InvalidNtfsGuid(&'a str, #[source] guid::ParseError),
    #[error("controller already exists")]
    StorageControllerGuidAlreadyExists,
    #[error("disk location exceeds limits {limits:?}")]
    StorageLunLocationExceedsMaxLimits { limits: u32 },
    #[error("exceeded 4 max SCSI controllers")]
    StorageScsiControllerExceedsMaxLimits,
    #[error("disk location is duplicated")]
    StorageLunLocationDuplicated,
    #[error("NVMe namespace id is invalid")]
    StorageLunNvmeNsidInvalid,
    #[error("NVMe namespace can't be a DVD drive")]
    StorageLunNvmeDvdUnsupported,
    #[error("build-to-build schema compat error: {0}")]
    StorageSchemaVersionMismatch(&'a str),
    #[error("invalid physical disk count: physical_disk_count = {physical_disk_count}")]
    StorageInvalidPhysicalDiskCount { physical_disk_count: usize },
    #[error("ide controller channel not provided")]
    StorageIdeChannelNotProvided,
    #[error("ide controller channel exceeds 2 max channels")]
    StorageIdeChannelExceedsMaxLimits,
    #[error("ide controller channel exceeds 1 max controller")]
    StorageIdeControllerExceedsMaxLimits,
    #[error("ide controller location exceeds 2 allowed drives per channel")]
    StorageIdeLocationExceedsMaxLimits,
    #[error("ide controller has invalid configuration")]
    StorageIdeChannelInvalidConfiguration,
    #[error("controller has unknown storage protocol")]
    StorageProtocolUnknown,
    #[error("invalid device type")]
    StorageInvalidDeviceType,
}

impl Error<'_> {
    fn code(&self) -> Vtl2SettingsErrorCode {
        match self {
            Error::UnsupportedSchemaVersion(_) => Vtl2SettingsErrorCode::UnsupportedSchemaVersion,
            Error::UnsupportedSchemaNamespace(_) => {
                Vtl2SettingsErrorCode::UnsupportedSchemaNamespace
            }
            Error::EmptyNamespaceChunk(_) => Vtl2SettingsErrorCode::EmptyNamespaceChunk,
            Error::InvalidInstanceId { .. } => Vtl2SettingsErrorCode::InvalidInstanceId,
            Error::InvalidNtfsGuid(_, _) => Vtl2SettingsErrorCode::StorageInvalidNtfsFormatGuid,
            Error::StorageControllerGuidAlreadyExists => {
                Vtl2SettingsErrorCode::StorageControllerGuidAlreadyExists
            }
            Error::StorageLunLocationExceedsMaxLimits { .. } => {
                Vtl2SettingsErrorCode::StorageLunLocationExceedsMaxLimits
            }
            Error::StorageScsiControllerExceedsMaxLimits => {
                Vtl2SettingsErrorCode::StorageScsiControllerExceedsMaxLimits
            }
            Error::StorageLunLocationDuplicated { .. } => {
                Vtl2SettingsErrorCode::StorageLunLocationDuplicated
            }
            Error::StorageLunNvmeNsidInvalid { .. } => {
                Vtl2SettingsErrorCode::StorageLunLocationExceedsMaxLimits
            }
            Error::StorageLunNvmeDvdUnsupported { .. } => {
                Vtl2SettingsErrorCode::StorageUnsupportedDeviceType
            }
            Error::StorageSchemaVersionMismatch(_) => Vtl2SettingsErrorCode::JsonFormatError,
            Error::StorageInvalidPhysicalDiskCount { .. } => {
                Vtl2SettingsErrorCode::StorageInvalidPhysicalDiskCount
            }
            Error::StorageIdeChannelNotProvided => {
                Vtl2SettingsErrorCode::StorageIdeChannelNotProvided
            }
            Error::StorageIdeChannelExceedsMaxLimits => {
                Vtl2SettingsErrorCode::StorageIdeChannelExceedsMaxLimits
            }
            Error::StorageIdeControllerExceedsMaxLimits => {
                Vtl2SettingsErrorCode::StorageIdeChannelExceedsMaxLimits
            }
            Error::StorageIdeLocationExceedsMaxLimits => {
                Vtl2SettingsErrorCode::StorageIdeChannelExceedsMaxLimits
            }
            Error::StorageIdeChannelInvalidConfiguration => {
                Vtl2SettingsErrorCode::StorageIdeChannelInvalidConfiguration
            }
            Error::StorageProtocolUnknown => Vtl2SettingsErrorCode::StorageInvalidControllerType,
            Error::StorageInvalidDeviceType => Vtl2SettingsErrorCode::StorageUnsupportedDeviceType,
        }
    }
}

impl From<Error<'_>> for Vtl2SettingsErrorInfo {
    #[track_caller]
    fn from(e: Error<'_>) -> Vtl2SettingsErrorInfo {
        // Format the message manually to get the full error string (including
        // error sources).
        let mut message = e.to_string();
        let mut source = e.source();
        while let Some(inner) = source {
            write!(&mut message, ": {}", inner).unwrap();
            source = inner.source();
        }

        Vtl2SettingsErrorInfo::new(e.code(), message)
    }
}

pub(crate) fn validate_version(
    version: i32,
    errors: &mut ParseErrors<'_>,
) -> Result<(), ParsingStopped> {
    match vtl2_settings_base::Version::from_i32(version)
        .unwrap_or(vtl2_settings_base::Version::Unknown)
    {
        vtl2_settings_base::Version::Unknown => {
            errors.push(Error::UnsupportedSchemaVersion(version as u32));
        }
        vtl2_settings_base::Version::V1 => {}
    }
    Ok(())
}

fn parse_instance_id(instance_id: &str) -> Result<Guid, Error<'_>> {
    instance_id
        .parse()
        .map_err(|err| Error::InvalidInstanceId(instance_id, err))
}

fn parse_ntfs_guid(ntfs_guid: Option<&str>) -> Result<Option<Guid>, Error<'_>> {
    ntfs_guid
        .map(|guid| {
            guid.parse()
                .map_err(|err| Error::InvalidNtfsGuid(guid, err))
        })
        .transpose()
}

fn check_dups(errors: &mut ParseErrors<'_>, iter: impl IntoIterator<Item = u32>) {
    let mut v: Vec<_> = iter.into_iter().collect();
    v.sort();
    for (a, b) in v.iter().zip(v.iter().skip(1)) {
        if a == b {
            errors.push(Error::StorageLunLocationDuplicated);
        }
    }
}

impl ParseSchema<crate::DeviceType> for DeviceType {
    fn parse_schema(
        &self,
        _errors: &mut ParseErrors<'_>,
    ) -> Result<crate::DeviceType, ParsingStopped> {
        match self {
            DeviceType::Nvme => Ok(crate::DeviceType::NVMe),
            DeviceType::Vscsi => Ok(crate::DeviceType::VScsi),
            DeviceType::Unknown => Err(Error::StorageInvalidDeviceType.into()),
        }
    }
}

impl ParseSchema<crate::PhysicalDevice> for PhysicalDevice {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::PhysicalDevice, ParsingStopped> {
        Ok(crate::PhysicalDevice {
            device_type: self.device_type().parse(errors)?,
            vmbus_instance_id: parse_instance_id(&self.device_path)?,
            sub_device_path: self.sub_device_path,
        })
    }
}

impl ParseSchema<crate::DiskParameters> for Lun {
    fn parse_schema(
        &self,
        _errors: &mut ParseErrors<'_>,
    ) -> Result<crate::DiskParameters, ParsingStopped> {
        Ok(crate::DiskParameters {
            device_id: self.device_id.clone(),
            vendor_id: self.vendor_id.clone(),
            product_id: self.product_id.clone(),
            product_revision_level: self.product_revision_level.clone(),
            serial_number: self.serial_number.clone(),
            model_number: self.model_number.clone(),
            medium_rotation_rate: self.medium_rotation_rate.map(|x| x as u16).unwrap_or(0),
            physical_sector_size: self.physical_sector_size,
            fua: self.fua,
            write_cache: self.write_cache,
            scsi_disk_size_in_bytes: self.scsi_disk_size_in_bytes,
            odx: self.odx,
            unmap: self.disable_thin_provisioning.map(|disable| !disable),
            max_transfer_length: self.max_transfer_length.map(|x| x as usize),
        })
    }
}

impl ParseSchema<crate::PhysicalDevices> for Lun {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::PhysicalDevices, ParsingStopped> {
        #[allow(deprecated)]
        if (self.is_dvd || self.physical_devices.is_some())
            && (self.device_type.is_some()
                || self.device_path.is_some()
                || self.sub_device_path.is_some())
        {
            errors.push(Error::StorageSchemaVersionMismatch(
                "cannot mix old/new physical device schema declarations",
            ));
        }

        let v = if let Some(physical_devices) = &self.physical_devices {
            let invalid_disk_count = || Error::StorageInvalidPhysicalDiskCount {
                physical_disk_count: physical_devices.device.is_some() as usize
                    + physical_devices.devices.len(),
            };

            match physical_devices.r#type() {
                physical_devices::BackingType::Single => {
                    let device = physical_devices
                        .device
                        .as_ref()
                        .ok_or_else(invalid_disk_count)?;
                    if !physical_devices.devices.is_empty() {
                        errors.push(invalid_disk_count());
                    }
                    crate::PhysicalDevices::Single {
                        device: device.parse(errors)?,
                    }
                }
                physical_devices::BackingType::Striped => {
                    if physical_devices.devices.len() < 2 || physical_devices.device.is_some() {
                        errors.push(invalid_disk_count());
                    }
                    crate::PhysicalDevices::Striped {
                        devices: physical_devices
                            .devices
                            .iter()
                            .flat_map(|v| v.parse(errors).collect_error(errors))
                            .collect(),
                        chunk_size_in_kb: self.chunk_size_in_kb,
                    }
                }
                physical_devices::BackingType::Unknown => {
                    return Err(Error::StorageInvalidDeviceType.into());
                }
            }
        } else if self.is_dvd {
            crate::PhysicalDevices::EmptyDrive
        } else {
            // Legacy compat path.
            #[allow(deprecated)]
            let (device_path, sub_device_path) =
                self.device_path.as_ref().zip(self.sub_device_path).ok_or(
                    Error::StorageSchemaVersionMismatch("could not find any physical devices"),
                )?;

            let device_type = self.device_type().parse(errors)?;
            let vmbus_instance_id = parse_instance_id(device_path)?;
            crate::PhysicalDevices::Single {
                device: crate::PhysicalDevice {
                    device_type,
                    vmbus_instance_id,
                    sub_device_path,
                },
            }
        };
        Ok(v)
    }
}

impl ParseSchema<crate::IdeDisk> for Lun {
    fn parse_schema(&self, errors: &mut ParseErrors<'_>) -> Result<crate::IdeDisk, ParsingStopped> {
        let channel = self.channel.ok_or(Error::StorageIdeChannelNotProvided)?;
        errors.with_context(ErrorContext::Ide(channel, self.location), |errors| {
            if channel >= crate::IDE_NUM_CHANNELS.into() {
                return Err(Error::StorageIdeChannelExceedsMaxLimits.into());
            }

            if self.location >= crate::IDE_MAX_DRIVES_PER_CHANNEL.into() {
                return Err(Error::StorageIdeLocationExceedsMaxLimits.into());
            }

            Ok(crate::IdeDisk {
                channel: channel as u8,
                location: self.location as u8,
                disk_params: self.parse(errors)?,
                physical_devices: self.parse(errors)?,
                ntfs_guid: parse_ntfs_guid(self.ntfs_guid.as_deref())?,
                is_dvd: self.is_dvd,
            })
        })
    }
}

impl ParseSchema<crate::ScsiDisk> for Lun {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::ScsiDisk, ParsingStopped> {
        errors.with_context(ErrorContext::Scsi(self.location), |errors| {
            if self.location as usize >= crate::SCSI_LUN_NUM {
                errors.push(Error::StorageLunLocationExceedsMaxLimits {
                    limits: crate::SCSI_LUN_NUM as u32,
                });
            }
            Ok(crate::ScsiDisk {
                location: self.location as u8,
                disk_params: self.parse(errors)?,
                physical_devices: self.parse(errors)?,
                ntfs_guid: parse_ntfs_guid(self.ntfs_guid.as_deref())?,
                is_dvd: self.is_dvd,
            })
        })
    }
}

impl ParseSchema<crate::NvmeNamespace> for Lun {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::NvmeNamespace, ParsingStopped> {
        errors.with_context(ErrorContext::Nvme(self.location), |errors| {
            if self.location == 0 || self.location == !0 {
                errors.push(Error::StorageLunNvmeNsidInvalid);
            }
            if self.is_dvd {
                errors.push(Error::StorageLunNvmeDvdUnsupported);
            }

            Ok(crate::NvmeNamespace {
                nsid: self.location,
                disk_params: self.parse(errors)?,
                physical_devices: self.parse(errors)?,
            })
        })
    }
}

impl ParseSchema<crate::IdeController> for StorageController {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::IdeController, ParsingStopped> {
        let instance_id = parse_instance_id(&self.instance_id)?;

        errors.with_context(ErrorContext::InstanceId(instance_id), |errors| {
            let mut disks = self
                .luns
                .iter()
                .flat_map(|lun| lun.parse(errors).collect_error(errors))
                .collect::<Vec<crate::IdeDisk>>();

            disks.sort_by(|a, b| (a.location).cmp(&b.location));

            for (a, b) in disks.iter().zip(disks.iter().skip(1)) {
                if a.channel == b.channel && a.location == b.location {
                    // Only 1 disk can be attached to a single slot.
                    errors.push_with_context(
                        ErrorContext::Ide(a.channel.into(), a.location.into()),
                        Error::StorageIdeChannelInvalidConfiguration,
                    );
                }
            }

            Ok(crate::IdeController {
                instance_id,
                disks,
                io_queue_depth: self.io_queue_depth,
            })
        })
    }
}

impl ParseSchema<crate::ScsiController> for StorageController {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::ScsiController, ParsingStopped> {
        let instance_id = parse_instance_id(&self.instance_id)?;

        errors.with_context(ErrorContext::InstanceId(instance_id), |errors| {
            let disks = self
                .luns
                .iter()
                .flat_map(|lun| lun.parse(errors).collect_error(errors))
                .collect::<Vec<crate::ScsiDisk>>();

            check_dups(errors, disks.iter().map(|disk| disk.location.into()));

            Ok(crate::ScsiController {
                instance_id,
                disks,
                io_queue_depth: self.io_queue_depth,
            })
        })
    }
}

impl ParseSchema<crate::NvmeController> for StorageController {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::NvmeController, ParsingStopped> {
        assert!(matches!(self.protocol(), StorageProtocol::Nvme));

        let instance_id = parse_instance_id(&self.instance_id)?;

        errors.with_context(ErrorContext::InstanceId(instance_id), |errors| {
            let namespaces = self
                .luns
                .iter()
                .flat_map(|lun| lun.parse(errors).collect_error(errors))
                .collect::<Vec<crate::NvmeNamespace>>();

            check_dups(errors, namespaces.iter().map(|ns| ns.nsid));

            Ok(crate::NvmeController {
                instance_id,
                namespaces,
            })
        })
    }
}

impl ParseSchema<crate::NicDevice> for NicDeviceLegacy {
    fn parse_schema(
        &self,
        _errors: &mut ParseErrors<'_>,
    ) -> Result<crate::NicDevice, ParsingStopped> {
        let instance_id = parse_instance_id(&self.instance_id)?;

        let mut subordinate_instance_id = self
            .subordinate_instance_id
            .as_ref()
            .map(|id| parse_instance_id(id))
            .transpose()?;

        if subordinate_instance_id == Some(Guid::ZERO) {
            subordinate_instance_id = None;
        }

        Ok(crate::NicDevice {
            instance_id,
            subordinate_instance_id,
            max_sub_channels: self.max_sub_channels.map(|val| val as u16),
        })
    }
}

impl ParseSchema<crate::NicDevice> for NicDevice {
    fn parse_schema(
        &self,
        _errors: &mut ParseErrors<'_>,
    ) -> Result<crate::NicDevice, ParsingStopped> {
        let instance_id = parse_instance_id(&self.instance_id)?;

        Ok(crate::NicDevice {
            instance_id,
            subordinate_instance_id: None,
            max_sub_channels: self.max_sub_channels.map(|val| val as u16),
        })
    }
}

impl ParseSchema<crate::NicDevice> for NicAcceleration {
    fn parse_schema(
        &self,
        _errors: &mut ParseErrors<'_>,
    ) -> Result<crate::NicDevice, ParsingStopped> {
        let instance_id = parse_instance_id(&self.instance_id)?;

        let subordinate_instance_id = parse_instance_id(&self.subordinate_instance_id)?;
        let subordinate_instance_id =
            (subordinate_instance_id != Guid::ZERO).then_some(subordinate_instance_id);

        Ok(crate::NicDevice {
            instance_id,
            subordinate_instance_id,
            max_sub_channels: None,
        })
    }
}

impl ParseSchema<crate::Vtl2SettingsFixed> for Vtl2SettingsFixed {
    fn parse_schema(
        &self,
        _errors: &mut ParseErrors<'_>,
    ) -> Result<crate::Vtl2SettingsFixed, ParsingStopped> {
        Ok(crate::Vtl2SettingsFixed {
            scsi_sub_channels: self.scsi_sub_channels.map_or(0, |x| x as u16),
            io_ring_size: self.io_ring_size.unwrap_or(256),
            max_bounce_buffer_pages: self.max_bounce_buffer_pages,
        })
    }
}

impl ParseSchema<crate::Vtl2SettingsDynamic> for Vtl2SettingsDynamic {
    fn parse_schema(
        &self,
        errors: &mut ParseErrors<'_>,
    ) -> Result<crate::Vtl2SettingsDynamic, ParsingStopped> {
        let mut ide_controller = None;
        let mut scsi_controllers = Vec::new();
        let mut nvme_controllers = Vec::new();

        for controller in &self.storage_controllers {
            match controller.protocol() {
                StorageProtocol::Ide => {
                    if let Some(c) = controller
                        .parse::<crate::IdeController>(errors)
                        .collect_error(errors)
                    {
                        if ide_controller.is_some() {
                            errors.push_with_context(
                                ErrorContext::InstanceId(c.instance_id),
                                Error::StorageIdeControllerExceedsMaxLimits,
                            );
                        }
                        ide_controller = Some(c);
                    }
                }
                StorageProtocol::Scsi => {
                    if let Some(c) = controller
                        .parse::<crate::ScsiController>(errors)
                        .collect_error(errors)
                    {
                        if scsi_controllers.len() >= crate::SCSI_CONTROLLER_NUM {
                            errors.push_with_context(
                                ErrorContext::InstanceId(c.instance_id),
                                Error::StorageScsiControllerExceedsMaxLimits,
                            );
                        }
                        scsi_controllers.push(c);
                    }
                }
                StorageProtocol::Nvme => {
                    if let Some(c) = controller
                        .parse::<crate::NvmeController>(errors)
                        .collect_error(errors)
                    {
                        nvme_controllers.push(c);
                    }
                }
                StorageProtocol::Unknown => {
                    let instance_id = parse_instance_id(&controller.instance_id)?;
                    errors.push_with_context(
                        ErrorContext::InstanceId(instance_id),
                        Error::StorageProtocolUnknown,
                    );
                }
            }
        }

        let mut instance_ids = scsi_controllers
            .iter()
            .map(|c| c.instance_id)
            .chain(nvme_controllers.iter().map(|c| c.instance_id))
            .chain(ide_controller.iter().map(|c| c.instance_id))
            .collect::<Vec<_>>();

        instance_ids.sort();
        for (a, b) in instance_ids.iter().zip(instance_ids.iter().skip(1)) {
            if a == b {
                errors.push_with_context(
                    ErrorContext::InstanceId(*a),
                    Error::StorageControllerGuidAlreadyExists,
                );
            }
        }

        let nic_devices = self
            .nic_devices
            .iter()
            .flat_map(|nic| nic.parse(errors).collect_error(errors))
            .collect();

        Ok(crate::Vtl2SettingsDynamic {
            ide_controller,
            scsi_controllers,
            nvme_controllers,
            nic_devices,
        })
    }
}
