// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the Hyper-V IDE controller device.

use super::DriveMedia;
use super::IdeDevice;
use async_trait::async_trait;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::ide::HyperVIdeDeviceHandle;
use disk_backend::resolve::ResolveDiskParameters;
use ide_resources::GuestMedia;
use scsi_core::ResolveScsiDeviceHandleParams;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::ScsiDeviceHandleKind;

/// A resolver for the Hyper-V IDE controller device.
pub struct HyperVIdeResolver;

declare_static_async_resolver! {
    HyperVIdeResolver,
    (ChipsetDeviceHandleKind, HyperVIdeDeviceHandle),
}

/// Errors that can occur when resolving the IDE controller.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ResolveIdeError {
    #[error("invalid IDE channel {0}")]
    InvalidChannel(u8),
    #[error("invalid IDE drive {0}")]
    InvalidDrive(u8),
    #[error("IDE drive {0}:{1} is already in use")]
    DriveInUse(u8, u8),
    #[error("failed to open IDE disk at {0}/{1}")]
    OpenDisk(u8, u8, #[source] ResolveError),
    #[error("failed to open IDE DVD at {0}/{1}")]
    OpenDvd(u8, u8, #[source] ResolveError),
    #[error("failed to create IDE device")]
    NewDevice(#[source] super::NewDeviceError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, HyperVIdeDeviceHandle> for HyperVIdeResolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveIdeError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: HyperVIdeDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let primary_interrupt = input.configure.new_line(IRQ_LINE_SET, "primary", 14);
        let secondary_interrupt = input.configure.new_line(IRQ_LINE_SET, "secondary", 15);

        let mut drives = [[None, None], [None, None]];

        for disk_cfg in resource.disks {
            let path = disk_cfg.path;

            let channel = drives
                .get_mut(path.channel as usize)
                .ok_or(ResolveIdeError::InvalidChannel(path.channel))?;
            let slot = channel
                .get_mut(path.drive as usize)
                .ok_or(ResolveIdeError::InvalidDrive(path.drive))?;

            if slot.is_some() {
                return Err(ResolveIdeError::DriveInUse(path.channel, path.drive));
            }

            let media = match disk_cfg.guest_media {
                GuestMedia::Dvd(scsi_resource) => {
                    let scsi_device = resolver
                        .resolve::<ScsiDeviceHandleKind, _>(
                            scsi_resource,
                            ResolveScsiDeviceHandleParams {
                                driver_source: input.task_driver_source,
                            },
                        )
                        .await
                        .map_err(|e| ResolveIdeError::OpenDvd(path.channel, path.drive, e))?;

                    DriveMedia::optical_disk(scsi_device.0)
                }
                GuestMedia::Disk {
                    disk_type,
                    read_only,
                    disk_parameters: _,
                } => {
                    let disk = resolver
                        .resolve::<DiskHandleKind, _>(
                            disk_type,
                            ResolveDiskParameters {
                                read_only,
                                driver_source: input.task_driver_source,
                            },
                        )
                        .await
                        .map_err(|e| ResolveIdeError::OpenDisk(path.channel, path.drive, e))?;

                    DriveMedia::hard_disk(disk.0)
                }
            };

            *slot = Some(media);
        }

        let [primary_drives, secondary_drives] = drives;

        let device = IdeDevice::new(
            input.guest_memory.clone(),
            input.register_pio,
            primary_drives,
            secondary_drives,
            primary_interrupt,
            secondary_interrupt,
        )
        .map_err(ResolveIdeError::NewDevice)?;

        Ok(device.into())
    }
}
