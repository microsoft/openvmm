// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Underhill configuration lib
//!
//! The structs and functions for Underhill configuration

#![forbid(unsafe_code)]

use guid::Guid;
use inspect::Inspect;
use mesh::MeshPayload;
use serde::Serialize;

mod errors;
pub mod schema;

// IDE constants
const IDE_NUM_CHANNELS: u8 = 2;
const IDE_MAX_DRIVES_PER_CHANNEL: u8 = 2;

// SCSI constants
const SCSI_CONTROLLER_NUM: usize = 4;
pub const SCSI_LUN_NUM: usize = 64;

#[derive(Debug, Copy, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub enum DeviceType {
    NVMe,
    VScsi,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct PhysicalDevice {
    pub device_type: DeviceType,
    // The associated vmbus device's instance ID, from which Underhill can find
    // the hardware PCI path.
    pub vmbus_instance_id: Guid,
    // The additional sub device path. It's the namespace ID for NVMe devices.
    pub sub_device_path: u32,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
#[inspect(external_tag)]
pub enum PhysicalDevices {
    EmptyDrive,
    Single {
        device: PhysicalDevice,
    },
    Striped {
        #[inspect(iter_by_index)]
        devices: Vec<PhysicalDevice>,
        chunk_size_in_kb: u32,
    },
}

impl PhysicalDevices {
    pub fn is_striping(&self) -> bool {
        matches!(self, PhysicalDevices::Striped { .. })
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, PhysicalDevices::EmptyDrive)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub enum GuestMediaType {
    Hdd,
    DvdLoaded,
    DvdUnloaded,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct DiskParameters {
    pub device_id: String,
    pub vendor_id: String,
    pub product_id: String,
    pub product_revision_level: String,
    pub serial_number: String,
    pub model_number: String,
    pub medium_rotation_rate: u16,
    pub physical_sector_size: Option<u32>,
    pub fua: Option<bool>,
    pub write_cache: Option<bool>,
    pub scsi_disk_size_in_bytes: Option<u64>,
    pub odx: Option<bool>,
    pub unmap: Option<bool>,
    pub max_transfer_length: Option<usize>,
}

#[derive(Debug)]
pub enum StorageDisk {
    Ide(IdeDisk),
    Scsi(ScsiDisk),
}

impl StorageDisk {
    pub fn physical_devices(&self) -> &PhysicalDevices {
        match self {
            StorageDisk::Ide(ide_disk) => &ide_disk.physical_devices,
            StorageDisk::Scsi(scsi_disk) => &scsi_disk.physical_devices,
        }
    }

    pub fn is_dvd(&self) -> bool {
        match self {
            StorageDisk::Ide(ide_disk) => ide_disk.is_dvd,
            StorageDisk::Scsi(scsi_disk) => scsi_disk.is_dvd,
        }
    }
    pub fn ntfs_guid(&self) -> Option<Guid> {
        match self {
            StorageDisk::Ide(ide_disk) => ide_disk.ntfs_guid,
            StorageDisk::Scsi(scsi_disk) => scsi_disk.ntfs_guid,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct IdeDisk {
    pub channel: u8,
    pub location: u8,
    pub disk_params: DiskParameters,
    pub physical_devices: PhysicalDevices,
    pub ntfs_guid: Option<Guid>,
    pub is_dvd: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct IdeController {
    pub instance_id: Guid,
    #[inspect(iter_by_index)]
    pub disks: Vec<IdeDisk>,
    pub io_queue_depth: Option<u32>,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct ScsiDisk {
    pub location: u8,
    pub disk_params: DiskParameters,
    pub physical_devices: PhysicalDevices,
    pub ntfs_guid: Option<Guid>,
    pub is_dvd: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct ScsiController {
    pub instance_id: Guid,
    #[inspect(iter_by_index)]
    pub disks: Vec<ScsiDisk>,
    pub io_queue_depth: Option<u32>,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct NvmeNamespace {
    pub nsid: u32,
    pub disk_params: DiskParameters,
    pub physical_devices: PhysicalDevices,
}

#[derive(Debug, Clone, Eq, PartialEq, MeshPayload, Inspect)]
pub struct NvmeController {
    pub instance_id: Guid,
    #[inspect(iter_by_index)]
    pub namespaces: Vec<NvmeNamespace>,
}

#[derive(Debug, Clone, MeshPayload, Inspect)]
pub struct NicDevice {
    pub instance_id: Guid,
    pub subordinate_instance_id: Option<Guid>,
    pub max_sub_channels: Option<u16>,
}

#[derive(Debug, Clone, MeshPayload, Inspect)]
pub struct Vtl2SettingsFixed {
    /// number of sub-channels for the SCSI controller
    pub scsi_sub_channels: u16,
    /// size of the io-uring submission queues
    pub io_ring_size: u32,
    /// Max bounce buffer pages active per cpu for unaligned IOs
    pub max_bounce_buffer_pages: Option<u32>,
    // DMA bounce buffer pages per queue
    pub dma_bounce_buffer_pages_per_queue: Option<u64>,
    // Threshold of io size in pages to use bounce buffer
    pub dma_bounce_buffer_pages_per_io_threshold: Option<u32>,
    // Max nvme drivers
    pub max_nvme_drivers: Option<u32>,
}

#[derive(Debug, Clone, MeshPayload, Inspect)]
pub struct Vtl2SettingsDynamic {
    /// Primary IDE controller
    pub ide_controller: Option<IdeController>,
    /// SCSI controllers
    #[inspect(iter_by_index)]
    pub scsi_controllers: Vec<ScsiController>,
    /// NIC devices
    #[inspect(iter_by_index)]
    pub nic_devices: Vec<NicDevice>,
    /// NVMe controllers
    #[inspect(iter_by_index)]
    pub nvme_controllers: Vec<NvmeController>,
}

#[derive(Debug, Default, Clone, MeshPayload, Inspect)]
pub struct Vtl2Settings {
    /// Static settings which cannot be updated during runtime
    pub fixed: Vtl2SettingsFixed,
    /// Dynamic settings
    pub dynamic: Vtl2SettingsDynamic,
}

enum Component {
    Underhill,
    Storage,
    Network,
}

enum EscalationCategory {
    Underhill,
    VmCreation,
    Configuration,
}

macro_rules! error_codes {
    {
        $(#[$enum_attr:meta])*
        $vis:vis enum $enum_name:ident {
            $(
                $(#[$attr:meta])*
                $name:ident => ($component:tt, $category:tt),
            )*
        }
    } => {
        $(#[$enum_attr])*
        $vis enum $enum_name {
            $(
                $(#[$attr])*
                $name,
            )*
        }

        impl $enum_name {
            /// Returns the string representation of the error code for sending
            /// ot the host.
            fn name(&self) -> &'static str {
                match self {
                    $(
                        $enum_name::$name => {
                            // Validate the component and category names.
                            let _ = Component::$component;
                            let _ = EscalationCategory::$category;
                            concat!(stringify!($category), ".", stringify!($name))
                        }
                    )*
                }
            }
        }
    };
}

error_codes! {
/// The error codes used for failures when parsing or acting on a VTL2 settings
/// document from the host.
///
/// These are used to provide an identifier that the host can match on, as well
/// as some error categorization. Additional (string) error context can be
/// provided in [`Vtl2SettingsErrorInfo`].
///
/// This table is in the form `name => (component, category)`. `component` and
/// `category` must be elements in the `Component` and `EscalationCategory`
/// enums, respectively.
///
/// Note that the values of name, component, and category are encoded into a
/// form sent to the host, and so changing them for an error may be a breaking
/// change.
///
/// Specifically, they are encoded into a string as `category.name`. The astute
/// reader will note that the `component` is not actually used in the error
/// message, or anywhere. FUTURE: remove the component, or include it as a
/// separate field in the JSON.
///
/// In any case, the Rust-assigned discriminant values are not used in error
/// messages and do not need to be stable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Vtl2SettingsErrorCode {
    /// Underhill internal failure
    InternalFailure => (Underhill, Underhill),
    /// Invalid JSON format
    JsonFormatError => (Underhill, Configuration),
    /// VM Bus server is not configured
    NoVmbusServer => (Underhill, VmCreation),
    /// Unsupported schema version
    UnsupportedSchemaVersion => (Underhill, Configuration),
    /// Invalid vmbus instance ID
    InvalidInstanceId => (Underhill, Configuration),
    /// Invalid protobuf format
    ProtobufFormatError => (Underhill, Configuration),
    /// Unsupported schema namespace
    UnsupportedSchemaNamespace => (Underhill, Configuration),
    /// Empty namespace chunk
    EmptyNamespaceChunk => (Underhill, Configuration),
    /// Change storage controller at runtime
    StorageCannotAddRemoveControllerAtRuntime => (Storage, Configuration),
    /// SCSI LUN exceeds max limits (64)
    StorageLunLocationExceedsMaxLimits => (Storage, Configuration),
    /// SCSI LUN location duplicated in configuration
    StorageLunLocationDuplicated => (Storage, Configuration),
    /// Unsupported device type in configuration
    StorageUnsupportedDeviceType => (Storage, Configuration),
    /// Cannot find NVMe device namespace /dev/nvme*n*
    StorageCannotFindVtl2Device => (Storage, Configuration),
    /// Hard drive cannot be empty
    EmptyDriveNotAllowed => (Storage, Configuration),

    /// Cannot open VTL2 block device
    StorageCannotOpenVtl2Device => (Storage, Underhill),
    /// Cannot find a given SCSI controller
    StorageScsiControllerNotFound => (Storage, Underhill),
    /// Failed to attack a disk to a controller
    StorageAttachDiskFailed => (Storage, Underhill),
    /// Failed to remove a disk from a controller
    StorageRmDiskFailed => (Storage, Underhill),
    /// Storage controller already exists
    StorageControllerGuidAlreadyExists => (Storage, Configuration),
    /// SCSI controller exceeds max limits (4)
    StorageScsiControllerExceedsMaxLimits => (Storage, Configuration),
    /// Invalid vendor ID
    StorageInvalidVendorId => (Storage, Configuration),
    /// Invalid product ID
    StorageInvalidProductId => (Storage, Configuration),
    /// Invalid product revision level
    StorageInvalidProductRevisionLevel => (Storage, Configuration),
    /// IDE channel is not provided
    StorageIdeChannelNotProvided => (Storage, Configuration),
    /// IDE channel exceeds max limits (0 or 1)
    StorageIdeChannelExceedsMaxLimits => (Storage, Configuration),
    /// IDE location exceeds max limits (0 or 1)
    StorageIdeLocationExceedsMaxLimits => (Storage, Configuration),
    /// IDE configuration is invalid
    StorageIdeChannelInvalidConfiguration => (Storage, Configuration),
    /// Cannot change storage controller with striped devices at runtime
    StripedStorageCannotChangeControllerAtRuntime => (Storage, Configuration),
    /// Invalid physical disk count
    StorageInvalidPhysicalDiskCount => (Storage, Configuration),
    /// Cannot modify IDE devices at runtime
    StorageCannotModifyIdeAtRuntime => (Storage, Configuration),
    /// Invalid controller type
    StorageInvalidControllerType => (Storage, Configuration),
    /// Invalid vendor ID
    StorageInvalidDeviceId => (Storage, Configuration),
    /// Failed to change media on a controller
    StorageChangeMediaFailed => (Storage, Underhill),
    /// Invalid NTFS format guid
    StorageInvalidNtfsFormatGuid => (Storage, Configuration),

    /// Failed to modify NIC
    NetworkingModifyNicFailed => (Network, Configuration),
    /// Failed to add NIC
    NetworkingAddNicFailed => (Network, Configuration),
    /// Failed to remove NIC
    NetworkingRemoveNicFailed => (Network, Configuration),
}
}

impl Serialize for Vtl2SettingsErrorCode {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.name())
    }
}

#[derive(Debug, Serialize)]
pub struct Vtl2SettingsErrorInfo {
    error_id: Vtl2SettingsErrorCode,
    message: String,
    file_name: &'static str,
    line: u32,
}

impl Vtl2SettingsErrorInfo {
    #[track_caller]
    pub fn new(code: Vtl2SettingsErrorCode, message: String) -> Self {
        let caller = std::panic::Location::caller();
        Vtl2SettingsErrorInfo {
            error_id: code,
            message,
            file_name: caller.file(),
            line: caller.line(),
        }
    }

    pub fn code(&self) -> Vtl2SettingsErrorCode {
        self.error_id
    }
}

impl std::fmt::Display for Vtl2SettingsErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: use a more standard `Display` impl and make sure anyone who
        // wants JSON requests it explicitly via `serde_json`.
        let json = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        write!(f, "{}", json)
    }
}

impl std::error::Error for Vtl2SettingsErrorInfo {}

#[derive(Debug)]
pub struct Vtl2SettingsErrorInfoVec {
    pub errors: Vec<Vtl2SettingsErrorInfo>,
}

impl std::fmt::Display for Vtl2SettingsErrorInfoVec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for e in &self.errors {
            writeln!(f, "{}", e)?;
        }
        Ok(())
    }
}

impl std::error::Error for Vtl2SettingsErrorInfoVec {}
