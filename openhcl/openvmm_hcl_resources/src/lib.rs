// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(not(target_os = "linux"), expect(missing_docs))]
#![cfg(target_os = "linux")]

//! The [`vm_resource`] resources and [`mesh_worker`] workers that are available
//! in OpenVMM-HCL.

#![forbid(unsafe_code)]

// Resources.
vm_resource::register_static_resolvers! {
    // Chipset devices
    #[cfg(guest_arch = "x86_64")]
    chipset::i8042::resolver::I8042Resolver,
    missing_dev::resolver::MissingDevResolver,
    #[cfg(feature = "tpm")]
    tpm_device::resolver::TpmDeviceResolver,
    chipset_device_worker::resolver::RemoteChipsetDeviceResolver,
    #[cfg(guest_arch = "x86_64")]
    serial_16550::resolver::Serial16550Resolver,
    #[cfg(guest_arch = "aarch64")]
    serial_pl011::resolver::SerialPl011Resolver,
    chipset::battery::resolver::BatteryResolver,

    // Non-volatile stores
    vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreResolver,
    vmgs_broker::resolver::VmgsFileResolver,

    // Serial ports
    serial_core::disconnected::resolver::DisconnectedSerialBackendResolver,
    vmbus_serial_guest::VmbusSerialGuestResolver,

    // Disks.
    //
    // `BlockDevice` and `NvmeDisk` are registered dynamically since they have
    // runtime dependencies.
    disk_striped::StripedDiskResolver,

    // SCSI
    scsidisk::resolver::SimpleScsiResolver,

    // Vmbus devices
    hyperv_ic::resolver::ShutdownIcResolver,
    storvsp::resolver::StorvspResolver,
    #[cfg(feature = "uidevices")]
    uidevices::resolver::VmbusUiResolver,

    // VPCI devices
    #[cfg(feature = "nvme")]
    nvme::resolver::NvmeControllerResolver,
}

// Mesh workers.
mesh_worker::register_workers! {
    #[cfg(feature = "vnc_worker")]
    vnc_worker::VncWorker<vmsocket::VmListener>,
    #[cfg(feature = "debug_worker")]
    debug_worker::DebuggerWorker<vmsocket::VmListener>,
    chipset_device_worker::worker::RemoteChipsetDeviceWorker,
}
