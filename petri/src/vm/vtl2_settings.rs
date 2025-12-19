// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Convenience methods for tests to generate and modify the VTL2 settings of a
//! VM under test.

use guid::Guid;

/// Logical type of the storage controller, used for both devices presented from
/// the host to VTL2, and also from VTL2 to the VTL0 guest.
#[expect(missing_docs)]
#[derive(Debug, PartialEq, Eq)]
pub enum ControllerType {
    Scsi,
    Ide,
    Nvme,
}

/// A builder for a physical device that will back a VTL2 LUN. This is the
/// storage that is presented from the host OS to VTL2. A single VTL2 lun can
/// have zero, one, or multiple backing devices (see [`Vtl2LunBuilder`] for more
/// details on that).
#[derive(Debug, PartialEq, Eq)]
pub struct Vtl2StorageBackingDeviceBuilder {
    device_type: ControllerType,
    device_path: Guid,
    sub_device_path: u32,
}

impl Vtl2StorageBackingDeviceBuilder {
    /// Creates a new physical device builder.
    ///
    /// `device_type` is the type of device presented to VTL2. `device_path` is
    /// the path to the device. Since both SCSI and NVMe are really just VMBUS
    /// devices, this is the VMBUS instance id (a.k.a the `vsid`).
    /// `sub_device_path` is the SCSI target id or NVMe namespace id within that
    /// device.
    ///
    /// IDE is not supported as a VTL2 backing device.
    pub fn new(device_type: ControllerType, device_path: Guid, sub_device_path: u32) -> Self {
        assert_ne!(device_type, ControllerType::Ide); // IDE is not supported as VTL2 backing device
        Self {
            device_type,
            device_path,
            sub_device_path,
        }
    }

    /// Builds the physical device into the protobuf type used by VTL2 settings.
    pub fn build(self) -> vtl2_settings_proto::PhysicalDevice {
        let device_type = match self.device_type {
            ControllerType::Scsi => vtl2_settings_proto::physical_device::DeviceType::Vscsi,
            ControllerType::Nvme => vtl2_settings_proto::physical_device::DeviceType::Nvme,
            ControllerType::Ide => unreachable!(),
        };
        vtl2_settings_proto::PhysicalDevice {
            device_type: device_type.into(),
            device_path: self.device_path.to_string(),
            sub_device_path: self.sub_device_path,
        }
    }
}

/// VTL2 Settings wraps the list of backing devices into a
/// [`PhysicalDevices`](vtl2_settings_proto::PhysicalDevices) type, with
/// subtlely different fields depending on whether there is one or multiple
/// devices. This helper builds that wrapper type.
pub fn build_vtl2_storage_backing_physical_devices(
    mut devices: Vec<Vtl2StorageBackingDeviceBuilder>,
) -> Option<vtl2_settings_proto::PhysicalDevices> {
    match devices.len() {
        0 => None,
        1 => Some(vtl2_settings_proto::PhysicalDevices {
            r#type: vtl2_settings_proto::physical_devices::BackingType::Single.into(),
            device: Some(devices.pop().unwrap().build()),
            devices: Vec::new(),
        }),
        _ => Some(vtl2_settings_proto::PhysicalDevices {
            r#type: vtl2_settings_proto::physical_devices::BackingType::Striped.into(),
            device: None,
            devices: devices.drain(..).map(|d| d.build()).collect(),
        }),
    }
}

/// A builder for a VTL2 LUN, which is a storage device presented from VTL2 to
/// the guest.
///
/// A LUN can be one of two flavors: a disk or a DVD (really a virtual optical
/// device, since it's a CD-ROM when presented over IDE). A DVD can be empty
/// (backed by no physical devices) or it can be backed by one or more physical
/// devices. A disk must be backed by one or more physical devices. (This is not
/// checked, since there may be interesting test cases that need to violate
/// these requirements.)
#[derive(Debug, PartialEq, Eq)]
pub struct Vtl2LunBuilder {
    location: u32,
    device_id: Guid,
    vendor_id: String,
    product_id: String,
    product_revision_level: String,
    serial_number: String,
    model_number: String,
    physical_devices: Vec<Vtl2StorageBackingDeviceBuilder>,
    is_dvd: bool,
    chunk_size_in_kb: u32,
}

impl Vtl2LunBuilder {
    /// Creates a new disk LUN builder with default values. Here "disk" is as
    /// opposed to NOT a DVD.
    pub fn disk() -> Self {
        Self {
            location: 0,
            device_id: Guid::new_random(),
            vendor_id: "OpenVMM".to_string(),
            product_id: "Disk".to_string(),
            product_revision_level: "1.0".to_string(),
            serial_number: "0".to_string(),
            model_number: "1".to_string(),
            physical_devices: Vec::new(),
            is_dvd: false,
            chunk_size_in_kb: 0,
        }
    }

    /// Creates a new dvd LUN builder with default values. Here "dvd" is as
    /// opposed to NOT a disk.
    pub fn dvd() -> Self {
        let mut s = Self::disk();
        s.is_dvd = true;
        s.product_id = "DVD".to_string();
        s
    }

    /// Guest visible location of the device (aka a guest "LUN")
    pub fn with_location(mut self, location: u32) -> Self {
        self.location = location;
        self
    }

    /// The physical devices backing the LUN.
    ///
    /// Overwrites any current physical backing device configuration (one or many).
    pub fn with_physical_devices(
        mut self,
        physical_devices: Vec<Vtl2StorageBackingDeviceBuilder>,
    ) -> Self {
        self.physical_devices = physical_devices;
        self
    }

    /// The single physical device backing the LUN.
    ///
    /// Overwrites any current physical backing device configuration (one or many).
    pub fn with_physical_device(self, physical_device: Vtl2StorageBackingDeviceBuilder) -> Self {
        self.with_physical_devices(vec![physical_device])
    }

    /// For striped devices, the size of the stripe chunk in KB.
    pub fn with_chunk_size_in_kb(mut self, chunk_size_in_kb: u32) -> Self {
        self.chunk_size_in_kb = chunk_size_in_kb;
        self
    }

    /// Builds the LUN into the protobuf type used by VTL2 settings.
    pub fn build(self) -> vtl2_settings_proto::Lun {
        vtl2_settings_proto::Lun {
            location: self.location,
            device_id: self.device_id.to_string(),
            vendor_id: self.vendor_id,
            product_id: self.product_id,
            product_revision_level: self.product_revision_level,
            serial_number: self.serial_number,
            model_number: self.model_number,
            physical_devices: build_vtl2_storage_backing_physical_devices(self.physical_devices),
            is_dvd: self.is_dvd,
            chunk_size_in_kb: self.chunk_size_in_kb,
            ..Default::default()
        }
    }
}

/// A builder for a VTL2 storage controller, which presents one or more LUNs to
/// the guest.
///
/// The controller has a type (SCSI, IDE, or NVMe). For SCSI controllers, the
/// guest identifies the controller by the supplied instance_id (a GUID). A
/// single disk can be located by its LUN on a specific controller.
#[derive(Debug, PartialEq, Eq)]
pub struct Vtl2StorageControllerBuilder {
    instance_id: Guid,
    protocol: ControllerType,
    luns: Vec<Vtl2LunBuilder>,
    io_queue_depth: Option<u32>,
}

impl Vtl2StorageControllerBuilder {
    /// Creates a new storage controller builder with default values
    /// (arbitrary `instance_id` and no disks) and the specified guest-visible
    /// controller type.
    pub fn new(protocol: ControllerType) -> Self {
        Self {
            instance_id: Guid::new_random(),
            protocol,
            luns: Vec::new(),
            io_queue_depth: None,
        }
    }

    /// Set the guest-visible instance GUID.
    pub fn with_instance_id(mut self, instance_id: Guid) -> Self {
        self.instance_id = instance_id;
        self
    }

    /// Add a LUN to the controller.
    pub fn add_lun(mut self, lun: Vtl2LunBuilder) -> Self {
        self.luns.push(lun);
        self
    }

    /// Add a LUN to the controller.
    pub fn add_luns(mut self, luns: Vec<Vtl2LunBuilder>) -> Self {
        self.luns.extend(luns);
        self
    }

    /// Generate the VTL2 settings for the controller.
    pub fn build(mut self) -> vtl2_settings_proto::StorageController {
        let protocol = match self.protocol {
            ControllerType::Scsi => vtl2_settings_proto::storage_controller::StorageProtocol::Scsi,
            ControllerType::Nvme => vtl2_settings_proto::storage_controller::StorageProtocol::Nvme,
            ControllerType::Ide => vtl2_settings_proto::storage_controller::StorageProtocol::Ide,
        };

        vtl2_settings_proto::StorageController {
            instance_id: self.instance_id.to_string(),
            protocol: protocol.into(),
            luns: self.luns.drain(..).map(|l| l.build()).collect(),
            io_queue_depth: self.io_queue_depth,
        }
    }
}
