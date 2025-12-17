// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The [`vm_resource`] resources and [`mesh_worker`] workers that are available
//! in OpenVMM.

#![forbid(unsafe_code)]

// Resources.
vm_resource::register_static_resolvers! {
    // Chipset devices
    #[cfg(guest_arch = "x86_64")]
    chipset::i8042::resolver::I8042Resolver,
    missing_dev::resolver::MissingDevResolver,
    #[cfg(feature = "tpm")]
    tpm_device::resolver::TpmDeviceResolver,
    #[cfg(guest_arch = "x86_64")]
    serial_16550::resolver::Serial16550Resolver,
    #[cfg(guest_arch = "x86_64")]
    serial_debugcon::resolver::SerialDebugconResolver,
    #[cfg(guest_arch = "aarch64")]
    serial_pl011::resolver::SerialPl011Resolver,
    chipset::battery::resolver::BatteryResolver,

    // Non-volatile stores
    vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreResolver,

    // Serial ports
    serial_core::disconnected::resolver::DisconnectedSerialBackendResolver,
    #[cfg(windows)]
    serial_socket::windows::WindowsPipeSerialResolver,
    serial_socket::net::SocketSerialResolver,

    // Network backends
    net_backend::null::NullResolver,
    #[cfg(feature = "net_consomme")]
    net_consomme::resolver::ConsommeResolver,
    #[cfg(all(feature = "net_tap", target_os = "linux"))]
    net_tap::resolver::TapResolver,
    #[cfg(windows)]
    net_dio::resolver::DioResolver,

    // Disks
    disk_layered::resolver::LayeredDiskResolver,
    #[cfg(feature = "disk_crypt")]
    disk_crypt::resolver::DiskCryptResolver,
    disk_file::FileDiskResolver,
    disk_prwrap::DiskWithReservationsResolver,
    disk_delay::resolver::DelayDiskResolver,
    disk_vhd1::Vhd1Resolver,
    #[cfg(windows)]
    disk_vhdmp::VhdmpDiskResolver,
    #[cfg(feature = "disk_blob")]
    disk_blob::resolver::BlobDiskResolver,

    // Disk Layers
    disklayer_ram::resolver::RamDiskLayerResolver,
    #[cfg(feature = "disklayer_sqlite")]
    disklayer_sqlite::resolver::SqliteDiskLayerResolver,

    // PCI devices
    gdma::resolver::GdmaDeviceResolver,
    nvme::resolver::NvmeControllerResolver,
    nvme_test::resolver::NvmeFaultControllerResolver,
    virtio::resolver::VirtioPciResolver,

    // SCSI
    scsidisk::resolver::SimpleScsiResolver,

    // Virtio devices
    #[cfg(any(windows, target_os = "linux"))]
    virtiofs::resolver::VirtioFsResolver,
    #[cfg(any(windows, target_os = "linux"))]
    virtio_p9::resolver::VirtioPlan9Resolver,
    virtio_net::resolver::VirtioNetResolver,
    virtio_pmem::resolver::VirtioPmemResolver,

    // Vmbus devices
    guest_crash_device::resolver::GuestCrashDeviceResolver,
    guest_emulation_device::resolver::GuestEmulationDeviceResolver,
    guest_emulation_log::resolver::GuestEmulationLogResolver,
    hyperv_ic::resolver::KvpIcResolver,
    hyperv_ic::resolver::ShutdownIcResolver,
    hyperv_ic::resolver::TimesyncIcResolver,
    netvsp::resolver::NetvspResolver,
    storvsp::resolver::StorvspResolver,
    uidevices::resolver::VmbusUiResolver,
    vmbfs::resolver::VmbfsResolver,
    vmbus_serial_host::resolver::VmbusSerialDeviceResolver,
}

// Workers.
mesh_worker::register_workers! {
    openvmm_core::VmWorker,
    vnc_worker::VncWorker<std::net::TcpListener>,

    #[cfg(feature = "gdb")]
    debug_worker::DebuggerWorker<std::net::TcpListener>,
}
