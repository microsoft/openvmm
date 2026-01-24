// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements VTL2 settings worker

use super::LoadedVm;
use crate::nvme_manager::manager::NvmeDiskConfig;
use crate::worker::NicConfig;
use anyhow::Context;
use cvm_tracing::CVM_ALLOWED;
use disk_backend::Disk;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend_resources::AutoFormattedDiskHandle;
use disk_blockdevice::OpenBlockDeviceConfig;
use futures::StreamExt;
use guest_emulation_transport::api::platform_settings::DevicePlatformSettings;
use guid::Guid;
use ide_resources::GuestMedia;
use ide_resources::IdeControllerConfig;
use ide_resources::IdeDeviceConfig;
use ide_resources::IdePath;
use mesh::CancelContext;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use scsidisk_resources::SimpleScsiDvdRequest;
use std::collections::HashMap;
use std::error::Error as _;
use std::fmt::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use storage_string::InvalidAsciiString;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiControllerRequest;
use storvsp_resources::ScsiDeviceAndPath;
use thiserror::Error;
use tracing::Instrument;
use tracing::instrument;
use uevent::UeventListener;
use underhill_config::DiskParameters;
use underhill_config::NicDevice;
use underhill_config::PhysicalDevice;
use underhill_config::PhysicalDevices;
use underhill_config::StorageDisk;
use underhill_config::Vtl2Settings;
use underhill_config::Vtl2SettingsDynamic;
use underhill_config::Vtl2SettingsErrorCode;
use underhill_config::Vtl2SettingsErrorInfo;
use underhill_threadpool::AffinitizedThreadpool;
use vm_resource::IntoResource;
use vm_resource::ResolveError;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmcore::vm_task::VmTaskDriverSource;

#[derive(Error, Debug)]
enum Error<'a> {
    #[error("RPC error")]
    Rpc(#[source] RpcError),
    #[error("cannot add/remove storage controllers at runtime")]
    StorageCannotAddRemoveControllerAtRuntime,
    #[error("Striping devices don't support runtime change")]
    StripStorageCannotChangeControllerAtRuntime,
    #[error("failed to open disk")]
    StorageCannotOpenDisk(#[source] ResolveError),
    #[error("could not disable io scheduling")]
    StorageCannotDisableIoScheduling(#[source] std::io::Error),
    #[error("failed to open {device_type:?} disk {path} at {instance_id}/{sub_device_path}")]
    StorageCannotOpenVtl2Device {
        device_type: underhill_config::DeviceType,
        instance_id: Guid,
        sub_device_path: u32,
        path: &'a PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to find {device_type:?} disk at {instance_id}/{sub_device_path}")]
    StorageCannotFindVtl2Device {
        device_type: underhill_config::DeviceType,
        instance_id: Guid,
        sub_device_path: u32,
        #[source]
        source: anyhow::Error,
    },
    #[error("no SCSI controller {0}")]
    StorageScsiControllerNotFound(Guid),
    #[error("failed to add disk")]
    StorageScsiPathInUse(#[source] anyhow::Error),
    #[error("failed to add disk at lun {0}")]
    StorageAttachDiskFailed(u8, #[source] anyhow::Error),
    #[error("failed to remove disk")]
    StorageScsiPathNotInUse(#[source] anyhow::Error),
    #[error("failed to remove disk at lun {0}")]
    StorageRemoveDiskFailed(u8, #[source] anyhow::Error),
    #[error("failed to change media at lun {0}")]
    StorageChangeMediaFailed(u8, #[source] anyhow::Error),
    #[error("failed to modify networking instance {0}")]
    NetworkingModifyNicFailed(Guid, #[source] anyhow::Error),
    #[error("failed to add network interface {0}")]
    NetworkingAddNicFailed(Guid, #[source] anyhow::Error),
    #[error("failed to remove network interface {0}")]
    NetworkingRemoveNicFailed(Guid, #[source] anyhow::Error),
    #[error("failed to parse Vendor ID: LUN = {lun:?}, vendor_id = {vendor_id:?}")]
    StorageBadVendorId {
        lun: u8,
        vendor_id: &'a str,
        #[source]
        source: InvalidAsciiString,
    },
    #[error("failed to parse Product ID: LUN = {lun:?}, product_id = {product_id:?}")]
    StorageBadProductId {
        lun: u8,
        product_id: &'a str,
        #[source]
        source: InvalidAsciiString,
    },
    #[error("failed to parse Device ID: LUN = {lun:?}, device_id = {device_id:?}")]
    StorageBadDeviceId { lun: u8, device_id: &'a str },
    #[error(
        "failed to parse Product Revision Level: LUN = {lun:?}, product_revision_level = {product_revision_level:?}"
    )]
    StorageBadProductRevisionLevel {
        lun: u8,
        product_revision_level: &'a str,
        #[source]
        source: InvalidAsciiString,
    },
    #[error("cannot modify IDE configuration at runtime")]
    StorageCannotModifyIdeAtRuntime,
}

impl Error<'_> {
    fn code(&self) -> Vtl2SettingsErrorCode {
        match self {
            Error::Rpc(_) => Vtl2SettingsErrorCode::InternalFailure,
            Error::StorageCannotAddRemoveControllerAtRuntime => {
                Vtl2SettingsErrorCode::StorageCannotAddRemoveControllerAtRuntime
            }
            Error::StripStorageCannotChangeControllerAtRuntime => {
                Vtl2SettingsErrorCode::StripedStorageCannotChangeControllerAtRuntime
            }
            Error::StorageCannotOpenDisk(_) => Vtl2SettingsErrorCode::StorageCannotOpenVtl2Device,
            Error::StorageCannotDisableIoScheduling(_) => {
                Vtl2SettingsErrorCode::StorageCannotOpenVtl2Device
            }
            Error::StorageCannotOpenVtl2Device { .. } => {
                Vtl2SettingsErrorCode::StorageCannotOpenVtl2Device
            }
            Error::StorageCannotFindVtl2Device { .. } => {
                Vtl2SettingsErrorCode::StorageCannotFindVtl2Device
            }
            Error::StorageScsiControllerNotFound(_) => {
                Vtl2SettingsErrorCode::StorageScsiControllerNotFound
            }
            Error::StorageScsiPathInUse(_) => Vtl2SettingsErrorCode::StorageAttachDiskFailed,
            Error::StorageAttachDiskFailed(..) => Vtl2SettingsErrorCode::StorageAttachDiskFailed,
            Error::StorageScsiPathNotInUse(_) => Vtl2SettingsErrorCode::StorageRmDiskFailed,
            Error::StorageRemoveDiskFailed(..) => Vtl2SettingsErrorCode::StorageRmDiskFailed,
            Error::NetworkingModifyNicFailed(..) => {
                Vtl2SettingsErrorCode::NetworkingModifyNicFailed
            }
            Error::NetworkingAddNicFailed(..) => Vtl2SettingsErrorCode::NetworkingAddNicFailed,
            Error::NetworkingRemoveNicFailed(..) => {
                Vtl2SettingsErrorCode::NetworkingRemoveNicFailed
            }
            Error::StorageBadVendorId { .. } => Vtl2SettingsErrorCode::StorageInvalidVendorId,
            Error::StorageBadProductId { .. } => Vtl2SettingsErrorCode::StorageInvalidProductId,
            Error::StorageBadProductRevisionLevel { .. } => {
                Vtl2SettingsErrorCode::StorageInvalidProductRevisionLevel
            }
            Error::StorageCannotModifyIdeAtRuntime => {
                Vtl2SettingsErrorCode::StorageCannotModifyIdeAtRuntime
            }
            Error::StorageBadDeviceId { .. } => Vtl2SettingsErrorCode::StorageInvalidDeviceId,
            Error::StorageChangeMediaFailed { .. } => {
                Vtl2SettingsErrorCode::StorageChangeMediaFailed
            }
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

pub enum Vtl2ConfigNicRpc {
    Modify(Rpc<(Guid, Option<Guid>), anyhow::Result<()>>),
    Add(Rpc<(Guid, Option<Guid>, Option<u16>), anyhow::Result<()>>),
    Remove(Rpc<Guid, anyhow::Result<()>>),
}

pub enum Vtl2ConfigAcquireResource {
    AddDisk(Guid, underhill_config::ScsiDisk),
    RmDisk(Guid, underhill_config::ScsiDisk),
    ChangeMedia(Guid, StorageDisk),
    ModifyNic((Guid, Option<Guid>)),
    AddNic((Guid, Option<Guid>, Option<u16>)),
    RemoveNic(Guid),
}

pub enum Vtl2ConfigCommit {
    AddDisk(
        Guid,
        ScsiDeviceAndPath,
        Option<mesh::Sender<SimpleScsiDvdRequest>>,
    ),
    RmDisk(Guid, storvsp_resources::ScsiPath),
    ChangeMedia(Guid, StorageDevicePath, Option<Resource<DiskHandleKind>>),
    ModifyNic((Guid, Option<Guid>)),
    AddNic((Guid, Option<Guid>, Option<u16>)),
    RemoveNic(Guid),
}

/// VTL2 settings worker
pub struct Vtl2SettingsWorker {
    old_settings: Vtl2SettingsDynamic,
    device_config_send: mesh::Sender<Vtl2ConfigNicRpc>,
    get_client: guest_emulation_transport::GuestEmulationTransportClient,
    interfaces: DeviceInterfaces,
    modify_timeout_in_seconds: u64,
}

pub struct DeviceInterfaces {
    scsi_dvds: HashMap<StorageDevicePath, mesh::Sender<SimpleScsiDvdRequest>>,
    scsi_request: HashMap<Guid, mesh::Sender<ScsiControllerRequest>>,
    use_nvme_vfio: bool,
}

impl Vtl2SettingsWorker {
    pub fn new(
        initial_settings: Vtl2SettingsDynamic,
        device_config_send: mesh::Sender<Vtl2ConfigNicRpc>,
        get_client: guest_emulation_transport::GuestEmulationTransportClient,
        interfaces: DeviceInterfaces,
        modify_timeout_in_seconds: u64,
    ) -> Vtl2SettingsWorker {
        Vtl2SettingsWorker {
            old_settings: initial_settings,
            device_config_send,
            get_client,
            interfaces,
            modify_timeout_in_seconds,
        }
    }

    pub async fn run(&mut self, uevent_listener: &UeventListener) {
        let mut settings_recv = self.get_client.take_vtl2_settings_recv().await.unwrap();

        while let Some(req) = settings_recv.next().await {
            req.0
                .handle(async |buf| {
                    self.handle_modify_vtl2_settings(uevent_listener, &{ buf })
                        .await
                })
                .await
        }
    }

    async fn handle_modify_vtl2_settings(
        &mut self,
        uevent_listener: &UeventListener,
        buf: &[u8],
    ) -> Result<(), Vec<Vtl2SettingsErrorInfo>> {
        let mut context =
            CancelContext::new().with_timeout(Duration::from_secs(self.modify_timeout_in_seconds));

        let old_settings = Vtl2Settings {
            fixed: Default::default(),
            dynamic: self.old_settings.clone(),
        };
        let vtl2_settings = Vtl2Settings::read_from(buf, old_settings).map_err(|err| match err {
            underhill_config::schema::ParseError::Json(err) => {
                vec![Vtl2SettingsErrorInfo::new(
                    Vtl2SettingsErrorCode::JsonFormatError,
                    err.to_string(),
                )]
            }
            underhill_config::schema::ParseError::Protobuf(err) => {
                vec![Vtl2SettingsErrorInfo::new(
                    Vtl2SettingsErrorCode::ProtobufFormatError,
                    err.to_string(),
                )]
            }
            underhill_config::schema::ParseError::Validation(err) => err.errors,
        });

        tracing::debug!(
            CVM_ALLOWED,
            ?buf,
            ?vtl2_settings,
            timeout = self.modify_timeout_in_seconds,
            "VTL2 settings received"
        );

        let vtl2_settings = vtl2_settings?;

        let new_settings = vtl2_settings.dynamic;

        tracing::info!(CVM_ALLOWED, ?new_settings, "Received VTL2 settings");

        let mut todos: Vec<Vtl2ConfigAcquireResource> = Vec::new();

        let mut errors = Vec::new();

        modify_storage_configuration(&self.old_settings, &new_settings, &mut todos, &mut errors);
        if let Err(err) =
            modify_network_configuration(&self.old_settings, &new_settings, &mut todos)
        {
            errors.push(err);
        }

        if !errors.is_empty() {
            tracing::warn!(
                CVM_ALLOWED,
                ?errors,
                "Errors preparing VTL2 configuration changes"
            );
            return Err(errors);
        }

        if errors.is_empty() {
            let mut to_commits: Vec<Vtl2ConfigCommit> = Vec::new();
            match self
                .acquire_configuration_resources(
                    &mut context,
                    uevent_listener,
                    todos,
                    &mut to_commits,
                )
                .await
            {
                Err(e) => {
                    tracing::error!(
                        CVM_ALLOWED,
                        ?e,
                        "Error acquiring VTL2 configuration resources"
                    );
                    errors.push(e);
                }
                Ok(()) => {
                    // NOTHING BEYOND CAN FAIL
                    // We assume recover action for commit is to re-create the VM
                    if let Err(e) = self.commit_configuration_changes(to_commits).await {
                        tracing::error!(CVM_ALLOWED, ?e, "Error commit VTL2 configuration changes");
                        errors.push(e);
                    }
                }
            }
        }

        if !errors.is_empty() {
            tracing::warn!(
                CVM_ALLOWED,
                ?errors,
                "Errors applying VTL2 configuration changes"
            );
            return Err(errors);
        }

        tracing::info!(CVM_ALLOWED, "VTL2 settings modified");
        self.old_settings = new_settings;
        Ok(())
    }

    async fn acquire_configuration_resources(
        &mut self,
        ctx: &mut CancelContext,
        uevent_listener: &UeventListener,
        todos: Vec<Vtl2ConfigAcquireResource>,
        to_commits: &mut Vec<Vtl2ConfigCommit>,
    ) -> Result<(), Vtl2SettingsErrorInfo> {
        for todo in todos {
            match todo {
                Vtl2ConfigAcquireResource::AddDisk(guid, disk) => {
                    let (disk_cfg, dvd) = make_scsi_disk_config(
                        ctx,
                        &StorageContext {
                            uevent_listener,
                            use_nvme_vfio: self.interfaces.use_nvme_vfio,
                        },
                        &disk,
                        false,
                    )
                    .await?;
                    to_commits.push(Vtl2ConfigCommit::AddDisk(guid, disk_cfg, dvd));
                }
                Vtl2ConfigAcquireResource::RmDisk(guid, disk) => {
                    let scsi_path = scsi_path_from_config(&disk)?;
                    to_commits.push(Vtl2ConfigCommit::RmDisk(guid, scsi_path));
                }
                Vtl2ConfigAcquireResource::ChangeMedia(guid, disk) => {
                    let path = storage_path_from_config(&disk)?;
                    let disk_type = make_disk_type(
                        ctx,
                        &StorageContext {
                            uevent_listener,
                            use_nvme_vfio: self.interfaces.use_nvme_vfio,
                        },
                        &disk,
                        false,
                    )
                    .await?;
                    to_commits.push(Vtl2ConfigCommit::ChangeMedia(guid, path, disk_type));
                }
                Vtl2ConfigAcquireResource::ModifyNic(nic_settings) => {
                    to_commits.push(Vtl2ConfigCommit::ModifyNic(nic_settings));
                }
                Vtl2ConfigAcquireResource::AddNic(nic_settings) => {
                    to_commits.push(Vtl2ConfigCommit::AddNic(nic_settings));
                }
                Vtl2ConfigAcquireResource::RemoveNic(instance_id) => {
                    to_commits.push(Vtl2ConfigCommit::RemoveNic(instance_id));
                }
            }
        }
        Ok(())
    }

    async fn commit_configuration_changes(
        &mut self,
        to_commits: Vec<Vtl2ConfigCommit>,
    ) -> Result<(), Vtl2SettingsErrorInfo> {
        for commit in to_commits {
            match commit {
                Vtl2ConfigCommit::AddDisk(controller_id, disk_cfg, dvd) => {
                    let scsi_path = disk_cfg.path;
                    self.interfaces
                        .scsi_request
                        .get(&controller_id)
                        .ok_or(Error::StorageScsiControllerNotFound(controller_id))?
                        .call_failable(ScsiControllerRequest::AddDevice, disk_cfg)
                        .await
                        .map_err(|err| {
                            Error::StorageAttachDiskFailed(
                                scsi_path.lun,
                                Error::StorageScsiPathInUse(err.into()).into(),
                            )
                        })?;

                    if let Some(dvd) = dvd {
                        assert!(
                            self.interfaces
                                .scsi_dvds
                                .insert(StorageDevicePath::Scsi(scsi_path), dvd)
                                .is_none()
                        );
                    }
                }
                Vtl2ConfigCommit::RmDisk(controller_id, scsi_path) => {
                    self.interfaces
                        .scsi_request
                        .get(&controller_id)
                        .ok_or(Error::StorageScsiControllerNotFound(controller_id))?
                        .call_failable(ScsiControllerRequest::RemoveDevice, scsi_path)
                        .await
                        .map_err(|err| {
                            Error::StorageRemoveDiskFailed(
                                scsi_path.lun,
                                Error::StorageScsiPathNotInUse(err.into()).into(),
                            )
                        })?;

                    let _ = self
                        .interfaces
                        .scsi_dvds
                        .remove(&StorageDevicePath::Scsi(scsi_path));
                }
                Vtl2ConfigCommit::ChangeMedia(controller_id, path, disk_cfg) => {
                    // TODO: Improve error handling to work with both storage types (IDE/SCSI)
                    let lun = if let StorageDevicePath::Scsi(scsi_path) = path {
                        scsi_path.lun
                    } else {
                        0
                    };

                    async {
                        let target = self
                            .interfaces
                            .scsi_dvds
                            .get(&path)
                            .ok_or(Error::StorageScsiControllerNotFound(controller_id))?;

                        target
                            .call_failable(SimpleScsiDvdRequest::ChangeMedia, disk_cfg)
                            .await?;

                        anyhow::Ok(())
                    }
                    .await
                    .map_err(|e| Error::StorageChangeMediaFailed(lun, e))?;
                }
                Vtl2ConfigCommit::ModifyNic(nic_settings) => {
                    let instance_id = nic_settings.0;
                    self.device_config_send
                        .call(Vtl2ConfigNicRpc::Modify, nic_settings)
                        .await
                        .map_err(Error::Rpc)?
                        .map_err(|e| Error::NetworkingModifyNicFailed(instance_id, e))?;
                }
                Vtl2ConfigCommit::AddNic(nic_settings) => {
                    let instance_id = nic_settings.0;
                    self.device_config_send
                        .call(Vtl2ConfigNicRpc::Add, nic_settings)
                        .await
                        .map_err(Error::Rpc)?
                        .map_err(|e| Error::NetworkingAddNicFailed(instance_id, e))?;
                }
                Vtl2ConfigCommit::RemoveNic(instance_id) => {
                    self.device_config_send
                        .call(Vtl2ConfigNicRpc::Remove, instance_id)
                        .await
                        .map_err(Error::Rpc)?
                        .map_err(|e| Error::NetworkingRemoveNicFailed(instance_id, e))?;
                }
            }
        }

        Ok(())
    }
}

pub(crate) async fn handle_vtl2_config_rpc(
    message: Vtl2ConfigNicRpc,
    vm: &mut LoadedVm,
    threadpool: &AffinitizedThreadpool,
) {
    match message {
        Vtl2ConfigNicRpc::Modify(rpc) => {
            rpc.handle(async |nic_settings| {
                let modify_settings = vm.network_settings.as_mut().map(|settings| {
                    settings.modify_network_settings(nic_settings.0, nic_settings.1)
                });
                modify_settings
                    .context("network modifications not supported for this VM")?
                    .await
            })
            .await
        }
        Vtl2ConfigNicRpc::Add(rpc) => {
            rpc.handle(async |nic_settings| {
                vm.add_vf_manager(threadpool, nic_settings.0, nic_settings.1, nic_settings.2)
                    .await
            })
            .await
        }
        Vtl2ConfigNicRpc::Remove(rpc) => {
            rpc.handle(async |instance_id| vm.remove_vf_manager(instance_id).await)
                .await
        }
    }
}

pub async fn disk_from_disk_type(
    disk_type: Resource<DiskHandleKind>,
    read_only: bool,
    resolver: &ResourceResolver,
    driver_source: &VmTaskDriverSource,
) -> Result<Disk, Vtl2SettingsErrorInfo> {
    let disk = resolver
        .resolve(
            disk_type,
            ResolveDiskParameters {
                read_only,
                driver_source,
            },
        )
        .await
        .map_err(Error::StorageCannotOpenDisk)?;
    Ok(disk.0)
}

fn modify_storage_configuration(
    old_settings: &Vtl2SettingsDynamic,
    new_settings: &Vtl2SettingsDynamic,
    todos: &mut Vec<Vtl2ConfigAcquireResource>,
    errors: &mut Vec<Vtl2SettingsErrorInfo>,
) {
    if let Err(e) = modify_ide_configuration(old_settings, new_settings, todos) {
        errors.push(e);
    }
    if let Err(e) = modify_scsi_configuration(old_settings, new_settings, todos) {
        errors.push(e);
    }
}

fn modify_ide_configuration(
    old_settings: &Vtl2SettingsDynamic,
    new_settings: &Vtl2SettingsDynamic,
    todos: &mut Vec<Vtl2ConfigAcquireResource>,
) -> Result<(), Vtl2SettingsErrorInfo> {
    if old_settings.ide_controller != new_settings.ide_controller {
        if let (Some(old_ide), Some(new_ide)) =
            (&old_settings.ide_controller, &new_settings.ide_controller)
        {
            if old_ide.instance_id == new_ide.instance_id {
                let instance_id = old_ide.instance_id;
                if old_ide.disks.len() != new_ide.disks.len() {
                    return Err(Error::StorageCannotModifyIdeAtRuntime.into());
                }
                for (old_disk, new_disk) in old_ide.disks.iter().zip(new_ide.disks.iter()) {
                    if old_disk.is_dvd
                        && new_disk.is_dvd
                        && old_disk.channel == new_disk.channel
                        && old_disk.location == new_disk.location
                        && old_disk.disk_params == new_disk.disk_params
                        && old_disk.physical_devices != new_disk.physical_devices
                    {
                        match (
                            old_disk.physical_devices.is_empty(),
                            new_disk.physical_devices.is_empty(),
                        ) {
                            (true, true) | (false, false) => {
                                return Err(Error::StorageCannotModifyIdeAtRuntime.into());
                            }
                            (true, false) | (false, true) => {
                                // (true, false) => eject
                                // (false, true) => insert

                                todos.push(Vtl2ConfigAcquireResource::ChangeMedia(
                                    instance_id,
                                    StorageDisk::Ide(new_disk.clone()),
                                ));
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        return Err(Error::StorageCannotModifyIdeAtRuntime.into());
    }

    Ok(())
}

fn modify_scsi_configuration(
    old_settings: &Vtl2SettingsDynamic,
    new_settings: &Vtl2SettingsDynamic,
    todos: &mut Vec<Vtl2ConfigAcquireResource>,
) -> Result<(), Vtl2SettingsErrorInfo> {
    let old_controller_map = create_device_map_from_settings(&old_settings.scsi_controllers);
    let new_controller_map = create_device_map_from_settings(&new_settings.scsi_controllers);

    let (_, controllers_to_remove, controllers_to_add) =
        calculate_device_change_from_map(&old_controller_map, &new_controller_map);

    if !controllers_to_add.is_empty() || !controllers_to_remove.is_empty() {
        return Err(Error::StorageCannotAddRemoveControllerAtRuntime.into());
    }

    modify_scsi_disks_configuration(&old_controller_map, &new_controller_map, todos)?;

    Ok(())
}

fn modify_scsi_disks_configuration(
    old_controller_map: &HashMap<Guid, &underhill_config::ScsiController>,
    new_controller_map: &HashMap<Guid, &underhill_config::ScsiController>,
    todos: &mut Vec<Vtl2ConfigAcquireResource>,
) -> Result<(), Vtl2SettingsErrorInfo> {
    let (modify_disks, mut remove_disks, mut add_disks) =
        calculate_scsi_disks_change(old_controller_map, new_controller_map)?;
    let mut change_media_disks = Vec::new();

    for (old_config, new_config, controller_id) in modify_disks {
        if (old_config.physical_devices.is_striping()) || new_config.physical_devices.is_striping()
        {
            return Err(Error::StripStorageCannotChangeControllerAtRuntime.into());
        }

        match (
            old_config.physical_devices.is_empty(),
            new_config.physical_devices.is_empty(),
        ) {
            (true, true) | (false, false) => {
                remove_disks.push((old_config, controller_id));
                add_disks.push((new_config, controller_id));
            }
            (true, false) | (false, true) => {
                // (true, false) => eject
                // (false, true) => insert

                if old_config.is_dvd && new_config.is_dvd {
                    change_media_disks.push((new_config, controller_id));
                } else {
                    remove_disks.push((old_config, controller_id));
                    add_disks.push((new_config, controller_id));
                }
            }
        }
    }

    // Remove devices
    for (disk, controller_id) in remove_disks {
        if disk.physical_devices.is_striping() {
            return Err(Error::StripStorageCannotChangeControllerAtRuntime.into());
        }
        todos.push(Vtl2ConfigAcquireResource::RmDisk(
            controller_id,
            disk.clone(),
        ));
    }

    // Add devices
    for (disk, controller_id) in add_disks {
        if disk.physical_devices.is_striping() {
            return Err(Error::StripStorageCannotChangeControllerAtRuntime.into());
        }
        todos.push(Vtl2ConfigAcquireResource::AddDisk(
            controller_id,
            disk.clone(),
        ));
    }

    // Change multimedia disks (eject or insert)
    for (disk, controller_id) in change_media_disks {
        todos.push(Vtl2ConfigAcquireResource::ChangeMedia(
            controller_id,
            StorageDisk::Scsi(disk.clone()),
        ))
    }

    Ok(())
}

fn calculate_scsi_disks_change<'a>(
    old_controller_map: &'a HashMap<Guid, &underhill_config::ScsiController>,
    new_controller_map: &'a HashMap<Guid, &underhill_config::ScsiController>,
) -> Result<
    (
        Vec<(
            &'a underhill_config::ScsiDisk,
            &'a underhill_config::ScsiDisk,
            Guid,
        )>,
        Vec<(&'a underhill_config::ScsiDisk, Guid)>,
        Vec<(&'a underhill_config::ScsiDisk, Guid)>,
    ),
    Vtl2SettingsErrorInfo,
> {
    let mut disks_to_modify = Vec::<(
        &underhill_config::ScsiDisk,
        &underhill_config::ScsiDisk,
        Guid,
    )>::new();
    let mut disks_to_remove = Vec::<(&underhill_config::ScsiDisk, Guid)>::new();
    let mut disks_to_add = Vec::<(&underhill_config::ScsiDisk, Guid)>::new();

    for (instance_id, new_controller) in new_controller_map {
        if let Some(old_controller) = old_controller_map.get(instance_id) {
            calculate_disks_change_per_scsi_controller(
                old_controller,
                new_controller,
                &mut disks_to_modify,
                &mut disks_to_remove,
                &mut disks_to_add,
            )?;
        }
    }

    Ok((disks_to_modify, disks_to_remove, disks_to_add))
}

fn calculate_disks_change_per_scsi_controller<'a>(
    old_controller: &'a underhill_config::ScsiController,
    new_controller: &'a underhill_config::ScsiController,
    disks_to_modify: &mut Vec<(
        &'a underhill_config::ScsiDisk,
        &'a underhill_config::ScsiDisk,
        Guid,
    )>,
    disks_to_remove: &mut Vec<(&'a underhill_config::ScsiDisk, Guid)>,
    disks_to_add: &mut Vec<(&'a underhill_config::ScsiDisk, Guid)>,
) -> Result<(), Vtl2SettingsErrorInfo> {
    let old_slots = get_scsi_controller_slots(old_controller)?;
    let new_slots = get_scsi_controller_slots(new_controller)?;

    for i in 0..underhill_config::SCSI_LUN_NUM {
        if old_slots[i] != new_slots[i] {
            match (old_slots[i], new_slots[i]) {
                (Some(old_config), Some(new_config)) => {
                    disks_to_modify.push((old_config, new_config, old_controller.instance_id))
                }
                (Some(old_config), None) => {
                    disks_to_remove.push((old_config, old_controller.instance_id));
                }
                (None, Some(new_config)) => {
                    disks_to_add.push((new_config, new_controller.instance_id));
                }
                (None, None) => continue,
            }
        }
    }

    Ok(())
}

fn get_scsi_controller_slots(
    controller: &underhill_config::ScsiController,
) -> Result<
    [Option<&underhill_config::ScsiDisk>; underhill_config::SCSI_LUN_NUM],
    Vtl2SettingsErrorInfo,
> {
    let mut slots: [Option<&underhill_config::ScsiDisk>; underhill_config::SCSI_LUN_NUM] =
        [None; underhill_config::SCSI_LUN_NUM];

    for disk in &controller.disks {
        slots[disk.location as usize] = Some(disk);
    }

    Ok(slots)
}

fn storage_path_from_config(
    disk: &StorageDisk,
) -> Result<StorageDevicePath, Vtl2SettingsErrorInfo> {
    match disk {
        StorageDisk::Ide(disk) => Ok(StorageDevicePath::Ide(ide_path_from_config(disk)?)),
        StorageDisk::Scsi(disk) => Ok(StorageDevicePath::Scsi(scsi_path_from_config(disk)?)),
    }
}

fn ide_path_from_config(
    disk: &underhill_config::IdeDisk,
) -> Result<IdePath, Vtl2SettingsErrorInfo> {
    Ok(IdePath {
        channel: disk.channel,
        drive: disk.location,
    })
}

fn scsi_path_from_config(
    disk: &underhill_config::ScsiDisk,
) -> Result<storvsp_resources::ScsiPath, Vtl2SettingsErrorInfo> {
    Ok(storvsp_resources::ScsiPath {
        path: 0,
        target: 0,
        lun: disk.location,
    })
}

fn modify_network_configuration(
    old_settings: &Vtl2SettingsDynamic,
    new_settings: &Vtl2SettingsDynamic,
    todos: &mut Vec<Vtl2ConfigAcquireResource>,
) -> Result<(), Vtl2SettingsErrorInfo> {
    let mut old_settings_map = HashMap::new();
    for nic_settings in &old_settings.nic_devices {
        old_settings_map.insert(
            nic_settings.instance_id,
            nic_settings.subordinate_instance_id,
        );
    }
    for nic_settings in &new_settings.nic_devices {
        if let Some(existing_subordinate_id) = old_settings_map.remove(&nic_settings.instance_id) {
            if existing_subordinate_id.is_some() != nic_settings.subordinate_instance_id.is_some() {
                todos.push(Vtl2ConfigAcquireResource::ModifyNic((
                    nic_settings.instance_id,
                    nic_settings.subordinate_instance_id,
                )));
            }
        } else {
            todos.push(Vtl2ConfigAcquireResource::AddNic((
                nic_settings.instance_id,
                nic_settings.subordinate_instance_id,
                nic_settings.max_sub_channels,
            )));
        }
    }
    if !old_settings_map.is_empty() {
        for nic_settings in old_settings_map {
            todos.push(Vtl2ConfigAcquireResource::RemoveNic(nic_settings.0));
        }
    }
    Ok(())
}

pub struct UhIdeControllerConfig {
    pub config: IdeControllerConfig,
    pub dvds: Vec<(IdePath, mesh::Sender<SimpleScsiDvdRequest>)>,
}

pub struct UhScsiControllerConfig {
    pub handle: ScsiControllerHandle,
    pub request: mesh::Sender<ScsiControllerRequest>,
    pub dvds: Vec<(
        storvsp_resources::ScsiPath,
        mesh::Sender<SimpleScsiDvdRequest>,
    )>,
}

#[cfg_attr(not(feature = "vpci"), expect(dead_code))]
pub struct UhVpciDeviceConfig {
    pub instance_id: Guid,
    pub resource: Resource<PciDeviceHandleKind>,
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub enum StorageDevicePath {
    Ide(IdePath),
    Scsi(storvsp_resources::ScsiPath),
}

async fn make_disk_type_from_physical_devices(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    physical_devices: &PhysicalDevices,
    ntfs_guid: Option<Guid>,
    read_only: bool,
    is_restoring: bool,
) -> Result<Resource<DiskHandleKind>, Vtl2SettingsErrorInfo> {
    let disk_type = match *physical_devices {
        PhysicalDevices::Single { ref device } => {
            make_disk_type_from_physical_device(ctx, storage_context, device, read_only).await?
        }
        PhysicalDevices::Striped {
            ref devices,
            chunk_size_in_kb,
        } => {
            let mut physical_disk_configs = Vec::new();
            for single_device in devices {
                let disk = make_disk_type_from_physical_device(
                    ctx,
                    storage_context,
                    single_device,
                    read_only,
                )
                .await?;

                physical_disk_configs.push(disk);
            }
            Resource::new(disk_backend_resources::StripedDiskHandle {
                devices: physical_disk_configs,
                chunk_size_in_bytes: Some(chunk_size_in_kb * 1024),
                logic_sector_count: None,
            })
        }
        PhysicalDevices::EmptyDrive => unreachable!("handled in calling function"),
    };

    if let Some(ntfs_guid) = ntfs_guid {
        if is_restoring {
            tracing::debug!(disk_guid = ?ntfs_guid, "restoring - disk not candidate for auto format");
        } else {
            // DEVNOTE: open-source OpenHCL does not currently have a resolver
            // for `AutoFormattedDiskHandle`.
            return Ok(Resource::new(AutoFormattedDiskHandle {
                disk: disk_type,
                guid: ntfs_guid.into(),
            }));
        }
    }

    Ok(disk_type)
}

struct StorageContext<'a> {
    uevent_listener: &'a UeventListener,
    use_nvme_vfio: bool,
}

#[instrument(skip_all, fields(CVM_ALLOWED))]
async fn make_disk_type_from_physical_device(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    single_device: &PhysicalDevice,
    read_only: bool,
) -> Result<Resource<DiskHandleKind>, Vtl2SettingsErrorInfo> {
    let controller_instance_id = single_device.vmbus_instance_id;
    let sub_device_path = single_device.sub_device_path;

    // Special case for NVMe when using VFIO.
    if storage_context.use_nvme_vfio
        && matches!(
            single_device.device_type,
            underhill_config::DeviceType::NVMe
        )
    {
        // Wait for the NVMe controller to arrive.
        let (pci_id, devpath) = vpci_path(&controller_instance_id);
        async {
            ctx.until_cancelled(storage_context.uevent_listener.wait_for_devpath(&devpath))
                .await??;
            ctx.until_cancelled(wait_for_pci_path(&pci_id)).await?;
            Ok(())
        }
        .await
        .map_err(|err| Error::StorageCannotFindVtl2Device {
            device_type: single_device.device_type,
            instance_id: controller_instance_id,
            sub_device_path,
            source: err,
        })?;

        // We can't validate yet that this namespace actually exists. That will
        // be checked later.
        return Ok(Resource::new(NvmeDiskConfig {
            pci_id,
            nsid: sub_device_path,
        }));
    }

    // Wait for the device to arrive.
    let devname = async {
        let devname = ctx
            .until_cancelled(async {
                match single_device.device_type {
                    underhill_config::DeviceType::NVMe => {
                        get_nvme_namespace_devname(
                            storage_context.uevent_listener,
                            &controller_instance_id,
                            sub_device_path,
                        )
                        .await
                    }
                    underhill_config::DeviceType::VScsi => {
                        get_vscsi_devname(
                            storage_context.uevent_listener,
                            &controller_instance_id,
                            sub_device_path,
                        )
                        .await
                    }
                }
            })
            .instrument(tracing::info_span!("get_devname", CVM_ALLOWED))
            .await??;
        Ok(devname)
    }
    .await
    .map_err(|err| Error::StorageCannotFindVtl2Device {
        device_type: single_device.device_type,
        instance_id: controller_instance_id,
        sub_device_path,
        source: err,
    })?;

    // Wait for bdi, which is the last thing before the block device actually
    // gets enabled. Per the above, the block device should already be ready (we
    // successfully opened it, or we got the uevent that indicates the disk is
    // ready), but it's possible that some times of devices are openable even
    // when the block device infrastructure is not fully ready.
    async {
        ctx.until_cancelled(
            storage_context
                .uevent_listener
                .wait_for_devpath(&PathBuf::from_iter([
                    Path::new("/sys/block"),
                    devname.as_ref(),
                    "bdi".as_ref(),
                ]))
                .instrument(tracing::info_span!("wait_for_bdi", CVM_ALLOWED)),
        )
        .await??;
        Ok(())
    }
    .await
    .map_err(|err| Error::StorageCannotFindVtl2Device {
        device_type: single_device.device_type,
        instance_id: controller_instance_id,
        sub_device_path,
        source: err,
    })?;

    // Disable any IO scheduler for the device, and disable blk layer merging,
    // so that Underhill is a straight passhthrough.
    {
        let path =
            PathBuf::from_iter([Path::new("/sys/block"), devname.as_ref(), "queue".as_ref()]);

        fs_err::write(path.join("scheduler"), "none")
            .map_err(Error::StorageCannotDisableIoScheduling)?;

        // 2 means disable all merges.
        fs_err::write(path.join("nomerges"), "2")
            .map_err(Error::StorageCannotDisableIoScheduling)?;
    }

    let disk_path = Path::new("/dev").join(devname);

    let file = disk_blockdevice::open_file_for_block(&disk_path, read_only).map_err(|e| {
        Error::StorageCannotOpenVtl2Device {
            device_type: single_device.device_type,
            instance_id: controller_instance_id,
            sub_device_path,
            path: &disk_path,
            source: e,
        }
    })?;

    Ok(Resource::new(OpenBlockDeviceConfig { file }))
}

fn make_disk_config_inner(
    location: u8,
    disk_params: &DiskParameters,
) -> Result<scsidisk_resources::DiskParameters, Vtl2SettingsErrorInfo> {
    let vendor_id = disk_params
        .vendor_id
        .parse()
        .map_err(|source| Error::StorageBadVendorId {
            lun: location,
            vendor_id: &disk_params.vendor_id,
            source,
        })?;

    let product_id =
        disk_params
            .product_id
            .parse()
            .map_err(|source| Error::StorageBadProductId {
                lun: location,
                product_id: &disk_params.product_id,
                source,
            })?;

    let product_revision_level = disk_params
        .product_revision_level
        .parse()
        .map_err(|source| Error::StorageBadProductRevisionLevel {
            lun: location,
            product_revision_level: &disk_params.product_revision_level,
            source,
        })?;

    let disk_id = disk_params
        .device_id
        .parse::<Guid>()
        .map_err(|_| Error::StorageBadDeviceId {
            lun: location,
            device_id: &disk_params.device_id,
        })?;

    if disk_id == Guid::ZERO {
        return Err(Error::StorageBadDeviceId {
            lun: location,
            device_id: &disk_params.device_id,
        }
        .into());
    }

    Ok(scsidisk_resources::DiskParameters {
        disk_id: Some(disk_id.into()),
        identity: Some(scsidisk_resources::DiskIdentity {
            vendor_id,
            product_id,
            product_revision_level,
            model_number: disk_params.model_number.clone().into_bytes(),
        }),
        serial_number: disk_params.serial_number.clone().into_bytes(),
        medium_rotation_rate: Some(disk_params.medium_rotation_rate),
        physical_sector_size: disk_params.physical_sector_size,
        fua: disk_params.fua,
        write_cache: disk_params.write_cache,
        scsi_disk_size_in_bytes: disk_params.scsi_disk_size_in_bytes,
        odx: disk_params.odx,
        unmap: disk_params.unmap,
        get_lba_status: true,
        max_transfer_length: disk_params.max_transfer_length,
        optimal_unmap_sectors: None, // TODO
    })
}

async fn make_ide_disk_config(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    disk: &underhill_config::IdeDisk,
    is_restoring: bool,
) -> Result<(IdeDeviceConfig, Option<mesh::Sender<SimpleScsiDvdRequest>>), Vtl2SettingsErrorInfo> {
    let disk_type = make_disk_type(
        ctx,
        storage_context,
        &StorageDisk::Ide(disk.clone()),
        is_restoring,
    )
    .await?;
    if disk.is_dvd {
        let (send, recv) = mesh::channel();
        Ok((
            IdeDeviceConfig {
                path: ide_path_from_config(disk)?,
                guest_media: GuestMedia::Dvd(
                    SimpleScsiDvdHandle {
                        media: disk_type,
                        requests: Some(recv),
                    }
                    .into_resource(),
                ),
            },
            Some(send),
        ))
    } else {
        Ok((
            IdeDeviceConfig {
                path: ide_path_from_config(disk)?,
                guest_media: GuestMedia::Disk {
                    disk_type: disk_type.unwrap(),
                    read_only: false,
                    disk_parameters: Some(make_disk_config_inner(
                        disk.location,
                        &disk.disk_params,
                    )?),
                },
            },
            None,
        ))
    }
}

async fn make_scsi_disk_config(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    disk: &underhill_config::ScsiDisk,
    is_restoring: bool,
) -> Result<
    (
        ScsiDeviceAndPath,
        Option<mesh::Sender<SimpleScsiDvdRequest>>,
    ),
    Vtl2SettingsErrorInfo,
> {
    let path = scsi_path_from_config(disk)?;
    let disk_type = make_disk_type(
        ctx,
        storage_context,
        &StorageDisk::Scsi(disk.clone()),
        is_restoring,
    )
    .await?;
    let (device, dvd) = if disk.is_dvd {
        let (send, recv) = mesh::channel();
        (
            SimpleScsiDvdHandle {
                media: disk_type,
                requests: Some(recv),
            }
            .into_resource(),
            Some(send),
        )
    } else {
        (
            SimpleScsiDiskHandle {
                disk: disk_type.unwrap(),
                read_only: false,
                parameters: make_disk_config_inner(disk.location, &disk.disk_params)?,
            }
            .into_resource(),
            None,
        )
    };
    Ok((ScsiDeviceAndPath { path, device }, dvd))
}

async fn make_disk_type(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    disk: &StorageDisk,
    is_restoring: bool,
) -> Result<Option<Resource<DiskHandleKind>>, Vtl2SettingsErrorInfo> {
    let disk_type = match (disk.is_dvd(), disk.physical_devices()) {
        (true, PhysicalDevices::EmptyDrive) => None,
        (false, PhysicalDevices::EmptyDrive) => unreachable!("schema validated"),
        (false, physical_devices) => Some(
            make_disk_type_from_physical_devices(
                ctx,
                storage_context,
                physical_devices,
                disk.ntfs_guid(),
                false,
                is_restoring,
            )
            .await?,
        ),
        (true, physical_devices) => {
            let disk_type = make_disk_type_from_physical_devices(
                ctx,
                storage_context,
                physical_devices,
                disk.ntfs_guid(),
                true,
                is_restoring,
            )
            .await;
            match disk_type {
                Ok(disk_type) => Some(disk_type),
                Err(err_info) => match err_info.code() {
                    Vtl2SettingsErrorCode::StorageCannotOpenVtl2Device => {
                        tracing::warn!(CVM_ALLOWED, ?err_info, "Check if ISO is present on drive");
                        None
                    }
                    _ => Err(err_info)?,
                },
            }
        }
    };
    Ok(disk_type)
}

async fn make_nvme_disk_config(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    namespace: &underhill_config::NvmeNamespace,
    is_restoring: bool,
) -> Result<NamespaceDefinition, Vtl2SettingsErrorInfo> {
    let disk_type = make_disk_type_from_physical_devices(
        ctx,
        storage_context,
        &namespace.physical_devices,
        None,
        false,
        is_restoring,
    )
    .await?;
    Ok(NamespaceDefinition {
        nsid: namespace.nsid,
        disk: disk_type,
        read_only: false,
    })
}

#[instrument(skip_all, fields(CVM_ALLOWED))]
async fn make_ide_controller_config(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    settings: &Vtl2SettingsDynamic,
    is_restoring: bool,
) -> Result<Option<UhIdeControllerConfig>, Vtl2SettingsErrorInfo> {
    let mut primary_channel_disks = Vec::new();
    let mut secondary_channel_disks = Vec::new();
    let mut io_queue_depth = None;

    let mut dvds = Vec::new();
    if let Some(ide_controller) = &settings.ide_controller {
        io_queue_depth = ide_controller.io_queue_depth;
        for disk in &ide_controller.disks {
            let (config, dvd) =
                make_ide_disk_config(ctx, storage_context, disk, is_restoring).await?;
            if let Some(dvd) = dvd {
                dvds.push((config.path, dvd));
            }
            if disk.channel == 0 {
                primary_channel_disks.push(config);
            } else {
                secondary_channel_disks.push(config);
            }
        }
    }

    if primary_channel_disks.is_empty() && secondary_channel_disks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(UhIdeControllerConfig {
            config: IdeControllerConfig {
                primary_channel_disks,
                secondary_channel_disks,
                io_queue_depth,
            },
            dvds,
        }))
    }
}

#[instrument(skip_all, fields(CVM_ALLOWED))]
async fn make_scsi_controller_config(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    controller: &underhill_config::ScsiController,
    scsi_sub_channels: u16,
    is_restoring: bool,
    default_io_queue_depth: u32,
) -> Result<UhScsiControllerConfig, Vtl2SettingsErrorInfo> {
    let instance_id = controller.instance_id;
    let mut scsi_disks = Vec::new();

    let mut dvds = Vec::new();
    for disk in &controller.disks {
        let (disk_cfg, dvd) =
            make_scsi_disk_config(ctx, storage_context, disk, is_restoring).await?;
        if let Some(dvd) = dvd {
            dvds.push((disk_cfg.path, dvd));
        }
        scsi_disks.push(disk_cfg);
    }

    // Set poll_mode_queue_depth = 4 for remote disk based on experiment results.
    // Azure VMs have the fixed SCSI controller ids. Both host and guest agents
    // rely on the fixed ids. It is assumed that data (remote) disks are attached
    // to SCSI controller with this specific id.
    let poll_mode_queue_depth =
        if instance_id == guid::guid!("f8b3781b-1e82-4818-a1c3-63d806ec15bb") {
            Some(4)
        } else {
            None
        };
    tracing::info!(
        %instance_id,
        poll_mode_queue_depth,
        "poll mode queue depth chosen based on controller instance id",
    );

    let (send, recv) = mesh::channel();
    // The choice of max 256 scsi subchannels is somewhat arbitrary. But
    // for now, it provides a decent trade off between unbounded number of
    // channels that cause perf issues vs delivering some reasonable perf.
    Ok(UhScsiControllerConfig {
        handle: ScsiControllerHandle {
            instance_id,
            max_sub_channel_count: scsi_sub_channels.min(256),
            devices: scsi_disks,
            io_queue_depth: Some(controller.io_queue_depth.unwrap_or(default_io_queue_depth)),
            requests: Some(recv),
            poll_mode_queue_depth,
        },
        request: send,
        dvds,
    })
}

#[instrument(skip_all, fields(CVM_ALLOWED))]
async fn make_nvme_controller_config(
    ctx: &mut CancelContext,
    storage_context: &StorageContext<'_>,
    controller: &underhill_config::NvmeController,
    is_restoring: bool,
) -> Result<UhVpciDeviceConfig, Vtl2SettingsErrorInfo> {
    let mut namespaces = Vec::new();
    for namespace in &controller.namespaces {
        namespaces
            .push(make_nvme_disk_config(ctx, storage_context, namespace, is_restoring).await?);
    }

    Ok(UhVpciDeviceConfig {
        instance_id: controller.instance_id,
        resource: NvmeControllerHandle {
            subsystem_id: controller.instance_id,
            namespaces,
            max_io_queues: 64,
            msix_count: 64,
        }
        .into_resource(),
    })
}

pub async fn create_storage_controllers_from_vtl2_settings(
    ctx: &mut CancelContext,
    uevent_listener: &UeventListener,
    use_nvme_vfio: bool,
    settings: &Vtl2SettingsDynamic,
    sub_channels: u16,
    is_restoring: bool,
    default_io_queue_depth: u32,
) -> Result<
    (
        Option<UhIdeControllerConfig>,
        Vec<UhScsiControllerConfig>,
        Vec<UhVpciDeviceConfig>,
    ),
    Vtl2SettingsErrorInfo,
> {
    let storage_context = StorageContext {
        uevent_listener,
        use_nvme_vfio,
    };
    let ide_controller =
        make_ide_controller_config(ctx, &storage_context, settings, is_restoring).await?;

    let mut scsi_controllers = Vec::new();
    for controller in &settings.scsi_controllers {
        scsi_controllers.push(
            make_scsi_controller_config(
                ctx,
                &storage_context,
                controller,
                sub_channels,
                is_restoring,
                default_io_queue_depth,
            )
            .await?,
        );
    }

    let mut nvme_controllers = Vec::new();
    for controller in &settings.nvme_controllers {
        nvme_controllers.push(
            make_nvme_controller_config(ctx, &storage_context, controller, is_restoring).await?,
        );
    }

    Ok((ide_controller, scsi_controllers, nvme_controllers))
}

fn nsid_matches(devpath: &Path, nsid: u32) -> anyhow::Result<bool> {
    match fs_err::read_to_string(devpath.join("nsid")) {
        Ok(this_nsid) => {
            let this_nsid: u32 = this_nsid.trim().parse().context("failed to parse nsid")?;
            Ok(this_nsid == nsid)
        }
        // If `nsid` is not found, then this sysfs path is not ready yet. Report
        // mismatch; we'll try again when the uevent arrives.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Checks if the block device `/dev/devname` is ready.
///
/// Linux adds the sysfs entry for a block device before the block device can
/// actually be opened; opens in this state will fail with ENXIO. But it delays
/// the uevent to notify the device's arrival until the device is fully ready
/// (and has even scanned the partition table). This function checks to ensure
/// the block device can be opened.
///
/// This should be called only when the block device was discovered by sysfs. If
/// it is not ready, then the caller should wait for a uevent indicating its
/// readiness.
async fn check_block_sysfs_ready(devname: &str) -> bool {
    let devname = devname.to_string();
    let dev_clone = devname.clone();
    match blocking::unblock(move || fs_err::File::open(Path::new("/dev").join(dev_clone))).await {
        Ok(_) => true,
        Err(err) if err.raw_os_error() == Some(libc::ENXIO) => {
            tracing::info!(
                CVM_ALLOWED,
                devname,
                "block device not ready, waiting for uevent"
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                CVM_ALLOWED,
                error = &err as &dyn std::error::Error,
                devname,
                "block device failed to open during scan"
            );
            // The block device is ready but is failing to open. Let this one
            // propagate through so that we don't time out.
            true
        }
    }
}

/// Returns the device path of an NVMe disk given the disk's namespace ID.
async fn get_nvme_namespace_devname(
    uevent_listener: &UeventListener,
    controller_instance_id: &Guid,
    nsid: u32,
) -> anyhow::Result<String> {
    // Wait for the nvme host to show up.
    let nvme_devpath =
        nvme_controller_path_from_vmbus_instance_id(uevent_listener, controller_instance_id)
            .await?;

    // Get a prefix like `nvme0n` to use when evaluating child nodes.
    let prefix = nvme_devpath
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
        + "n";

    // Look for a child node with the correct namespace ID.
    let devname = uevent_listener
        .wait_for_matching_child(&nvme_devpath, async |path, uevent| {
            let name = path.file_name()?.to_str()?;
            if !name.starts_with(&prefix) {
                return None;
            }
            match nsid_matches(&path, nsid) {
                Ok(true) => {
                    if uevent || check_block_sysfs_ready(name).await {
                        Some(Ok(name.to_string()))
                    } else {
                        None
                    }
                }
                Ok(false) => None,
                Err(err) => Some(Err(err)),
            }
        })
        .await??;

    Ok(devname)
}

async fn get_scsi_host_number(
    uevent_listener: &UeventListener,
    instance_id: &Guid,
) -> anyhow::Result<u32> {
    // Wait for a node of the name host<n> to show up.
    let controller_path = PathBuf::from(format!("/sys/bus/vmbus/devices/{instance_id}"));
    let host_number = uevent_listener
        .wait_for_matching_child(&controller_path, async |name, _uevent| {
            name.file_name()?
                .to_str()?
                .strip_prefix("host")?
                .parse()
                .ok()
        })
        .await?;

    Ok(host_number)
}

async fn get_vscsi_devname(
    uevent_listener: &UeventListener,
    controller_instance_id: &Guid,
    lun: u32,
) -> anyhow::Result<String> {
    // Wait for the SCSI host.
    let host_number = get_scsi_host_number(uevent_listener, controller_instance_id).await?;

    // Wait for the block device to show up.
    let block_devpath = PathBuf::from(format!(
        "/sys/bus/scsi/devices/{host_number}:0:0:{lun}/block"
    ));

    uevent_listener
        .wait_for_matching_child(&block_devpath, async |path, uevent| {
            let name = path.file_name()?.to_str()?;
            if uevent || check_block_sysfs_ready(name).await {
                Some(name.to_string())
            } else {
                None
            }
        })
        .await
        .context("failed to wait for block path")
}

/// Returns the device path of an NVMe controller given the instance ID of the VMBUS channel
/// associated to its parent virtual PCI bus.
async fn nvme_controller_path_from_vmbus_instance_id(
    uevent_listener: &UeventListener,
    instance_id: &Guid,
) -> anyhow::Result<PathBuf> {
    let (pci_id, mut devpath) = vpci_path(instance_id);
    devpath.push("nvme");
    let devpath = uevent_listener
        .wait_for_matching_child(&devpath, async |path, _uevent| Some(path))
        .await?;
    wait_for_pci_path(&pci_id).await;
    Ok(devpath)
}

fn vpci_path(instance_id: &Guid) -> (String, PathBuf) {
    // The RID of a VPCI device is derived from its vmbus channel instance ID by
    // using Data2 as the segment ID and setting bus, device and function number to zero.
    let pci_id = format!("{:04x}:00:00.0", instance_id.data2);
    let pci_bus_id = &pci_id[..7];

    // Wait for the vfio driver to bind.
    //
    // Example devpath: /sys/bus/vmbus/devices/bf66a3ce-2c8d-4443-a1c2-59f58e4fcb14/pci2c8d:00/2c8d:00:00.0
    let devpath = PathBuf::from(format!(
        "/sys/bus/vmbus/devices/{instance_id}/pci{pci_bus_id}/{pci_id}"
    ));

    (pci_id, devpath)
}

/// Waits for the PCI path to get populated. The PCI path is just a symlink to the actual
/// device path. This should be called once the device path is available.
pub async fn wait_for_pci_path(pci_id: &String) {
    let pci_path = PathBuf::from(format!("/sys/bus/pci/devices/{pci_id}"));
    loop {
        if pci_path.exists() {
            return;
        }

        let mut context = CancelContext::new().with_timeout(Duration::from_millis(10));
        let _ = context
            .until_cancelled({
                async {
                    futures::pending!();
                }
            })
            .await;
    }
}

pub async fn wait_for_mana(
    uevent_listener: &UeventListener,
    instance_id: &Guid,
) -> anyhow::Result<String> {
    let (pci_id, devpath) = vpci_path(instance_id);

    // Wait for the device to show up.
    uevent_listener.wait_for_devpath(&devpath).await?;
    wait_for_pci_path(&pci_id).await;

    // Validate the device and vendor.
    let vendor = fs_err::read_to_string(devpath.join("vendor"))?;
    let device = fs_err::read_to_string(devpath.join("device"))?;
    if vendor.trim_end() != "0x1414" {
        anyhow::bail!("invalid mana vendor {vendor}");
    }
    if device.trim_end() != "0x00ba" {
        anyhow::bail!("invalid mana device {device}");
    }

    Ok(pci_id)
}

pub async fn get_mana_config_from_vtl2_settings(
    ctx: &mut CancelContext,
    uevent_listener: &UeventListener,
    settings: &Vtl2SettingsDynamic,
) -> anyhow::Result<Vec<NicConfig>> {
    let mut mana = Vec::<NicConfig>::new();
    for config in &settings.nic_devices {
        let pci_id = ctx
            .until_cancelled(wait_for_mana(uevent_listener, &config.instance_id))
            .await
            .context("cancelled waiting for mana devices")??;

        mana.push(NicConfig {
            pci_id,
            instance_id: config.instance_id,
            subordinate_instance_id: config.subordinate_instance_id,
            max_sub_channels: config.max_sub_channels,
        });
    }
    Ok(mana)
}

trait HasInstanceId {
    fn instance_id(&self) -> Guid;
}

impl HasInstanceId for underhill_config::ScsiController {
    fn instance_id(&self) -> Guid {
        self.instance_id
    }
}

impl HasInstanceId for underhill_config::IdeController {
    fn instance_id(&self) -> Guid {
        self.instance_id
    }
}

impl HasInstanceId for NicDevice {
    fn instance_id(&self) -> Guid {
        self.instance_id
    }
}

fn create_device_map_from_settings<T: HasInstanceId>(devices: &Vec<T>) -> HashMap<Guid, &T> {
    let mut map = HashMap::new();

    for device in devices {
        map.insert(device.instance_id(), device);
    }

    map
}

fn calculate_device_change_from_map<'a, T>(
    old_device_map: &'a HashMap<Guid, &T>,
    new_device_map: &'a HashMap<Guid, &T>,
) -> (Vec<(&'a T, &'a T)>, Vec<&'a T>, Vec<&'a T>) {
    let mut devices_to_check = Vec::new();
    let mut devices_to_remove = Vec::new();
    let mut devices_to_add = Vec::new();

    for (instance_id, old_config) in old_device_map.iter() {
        match new_device_map.get(instance_id) {
            Some(new_config) => {
                // add device to check list
                devices_to_check.push((*old_config, *new_config));
            }
            // remove the device
            _ => devices_to_remove.push(*old_config),
        }
    }

    for (instance_id, config) in new_device_map.iter() {
        if old_device_map.get(instance_id).is_none() {
            devices_to_add.push(*config)
        }
    }

    (devices_to_check, devices_to_remove, devices_to_add)
}

pub struct InitialControllers {
    pub ide_controller: Option<IdeControllerConfig>,
    pub vmbus_devices: Vec<Resource<VmbusDeviceHandleKind>>,
    pub vpci_devices: Vec<UhVpciDeviceConfig>,
    pub mana: Vec<NicConfig>,
    pub device_interfaces: DeviceInterfaces,
}

impl InitialControllers {
    /// Construct initial network/storage controllers from initial Vtl2Settings
    pub async fn new(
        uevent_listener: &UeventListener,
        dps: &DevicePlatformSettings,
        use_nvme_vfio: bool,
        is_restoring: bool,
        default_io_queue_depth: u32,
        config_timeout_in_seconds: u64,
    ) -> anyhow::Result<Self> {
        let mut context =
            CancelContext::new().with_timeout(Duration::from_secs(config_timeout_in_seconds));

        let vtl2_settings = dps.general.vtl2_settings.as_ref();

        tracing::info!(
            CVM_ALLOWED,
            ?vtl2_settings,
            config_timeout_in_seconds,
            "Initial VTL2 settings"
        );

        let fixed = vtl2_settings.map_or_else(Default::default, |s| s.fixed.clone());
        let dynamic = vtl2_settings.map(|s| &s.dynamic);

        let (ide_controller, scsi_controllers, vpci_devices) = if let Some(dynamic) = &dynamic {
            create_storage_controllers_from_vtl2_settings(
                &mut context,
                uevent_listener,
                use_nvme_vfio,
                dynamic,
                fixed.scsi_sub_channels,
                is_restoring,
                default_io_queue_depth,
            )
            .await?
        } else {
            (None, Vec::new(), Vec::new())
        };

        let mana = if let Some(dynamic) = &dynamic {
            get_mana_config_from_vtl2_settings(&mut context, uevent_listener, dynamic).await?
        } else {
            Vec::new()
        };

        let mut scsi_dvds = HashMap::new();
        let mut scsi_request = HashMap::new();

        let ide_controller = ide_controller.map(|c| {
            scsi_dvds.extend(
                c.dvds
                    .into_iter()
                    .map(|(path, send)| (StorageDevicePath::Ide(path), send)),
            );
            c.config
        });

        let vmbus_devices = scsi_controllers
            .into_iter()
            .map(|c| {
                scsi_dvds.extend(
                    c.dvds
                        .into_iter()
                        .map(|(path, send)| (StorageDevicePath::Scsi(path), send)),
                );
                scsi_request.insert(c.handle.instance_id, c.request);
                c.handle.into_resource()
            })
            .collect();

        let cfg = InitialControllers {
            ide_controller,
            vmbus_devices,
            mana,
            vpci_devices,
            device_interfaces: DeviceInterfaces {
                scsi_dvds,
                scsi_request,
                use_nvme_vfio,
            },
        };

        Ok(cfg)
    }
}
