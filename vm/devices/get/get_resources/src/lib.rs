// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the GET family of devices.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Guest Emulation Log device resources.
pub mod gel {
    use mesh::MeshPayload;
    use vm_resource::kind::VmbusDeviceHandleKind;
    use vm_resource::ResourceId;

    /// Handle to a guest emulation log device.
    #[derive(MeshPayload)]
    pub struct GuestEmulationLogHandle;

    impl ResourceId<VmbusDeviceHandleKind> for GuestEmulationLogHandle {
        const ID: &'static str = "gel";
    }
}

/// Guest crash device resources.
pub mod crash {
    use mesh::rpc::FailableRpc;
    use mesh::MeshPayload;
    use std::fs::File;
    use vm_resource::kind::VmbusDeviceHandleKind;
    use vm_resource::ResourceId;

    /// Handle to a guest crash dump device.
    #[derive(MeshPayload)]
    pub struct GuestCrashDeviceHandle {
        /// A channel the device can use to get a file to write a dump to.
        pub request_dump: mesh::Sender<FailableRpc<mesh::OneshotReceiver<()>, File>>,
        /// The maximum size of the dump that the device will write.
        pub max_dump_size: u64,
    }

    impl ResourceId<VmbusDeviceHandleKind> for GuestCrashDeviceHandle {
        const ID: &'static str = "guest_crash_device";
    }
}

/// Guest Emulation Device resources.
pub mod ged {
    use mesh::error::RemoteError;
    use mesh::payload::Protobuf;
    use mesh::rpc::Rpc;
    use mesh::MeshPayload;
    use thiserror::Error;
    use vm_resource::kind::DiskHandleKind;
    use vm_resource::kind::FramebufferHandleKind;
    use vm_resource::kind::VmbusDeviceHandleKind;
    use vm_resource::Resource;
    use vm_resource::ResourceId;

    /// A resource handle for a guest emulation device.
    #[derive(MeshPayload)]
    pub struct GuestEmulationDeviceHandle {
        /// The firmware configuration for the guest.
        pub firmware: GuestFirmwareConfig,
        /// Enable COM1 for VTL0 and the VMBUS redirector in VTL2.
        pub com1: bool,
        /// Enable COM2 for VTL0 and the VMBUS redirector in VTL2.
        pub com2: bool,
        /// Enable vmbus redirection.
        pub vmbus_redirection: bool,
        /// Enable the TPM.
        pub enable_tpm: bool,
        /// Encoded VTL2 settings.
        pub vtl2_settings: Option<Vec<u8>>,
        /// The disk to back the GET's VMGS interface.
        ///
        /// If `None`, then VMGS services will not be provided to the guest.
        pub vmgs_disk: Option<Resource<DiskHandleKind>>,
        /// Framebuffer device control.
        pub framebuffer: Option<Resource<FramebufferHandleKind>>,
        /// Access to VTL2 functionality.
        pub guest_request_recv: mesh::Receiver<GuestEmulationRequest>,
        /// Notification of firmware events.
        pub firmware_event_send: Option<mesh::MpscSender<FirmwareEvent>>,
        /// Enable secure boot.
        pub secure_boot_enabled: bool,
        /// The secure boot template type.
        pub secure_boot_template: GuestSecureBootTemplateType,
        /// Enable battery.
        pub enable_battery: bool,
    }

    /// The firmware and chipset configuration for the guest.
    #[derive(MeshPayload)]
    pub enum GuestFirmwareConfig {
        /// Boot from UEFI with Hyper-V generation 2 devices.
        Uefi {
            /// Tell UEFI to consider booting from VPCI.
            enable_vpci_boot: bool,
            /// Enable UEFI firmware debugging for VTL0.
            firmware_debug: bool,
            /// Disable the UEFI frontpage which will cause the VM to shutdown instead when unable to boot.
            disable_frontpage: bool,
            /// Where to send UEFI console output
            console_mode: UefiConsoleMode,
        },
        /// Boot from PC/AT BIOS with Hyper-V generation 1 devices.
        Pcat {
            /// The boot order for the PC/AT firmware.
            boot_order: [PcatBootDevice; 4],
        },
    }

    /// UEFI Console Mode
    #[derive(MeshPayload, Clone, Debug, Copy)]
    pub enum UefiConsoleMode {
        /// video+kbd (having a head)
        Default = 0,
        /// headless with COM1 serial console
        COM1 = 1,
        /// headless with COM2 serial console
        COM2 = 2,
        /// headless
        None = 3,
    }

    /// The guest's secure boot template type to use.
    #[derive(MeshPayload, Clone, Debug, Copy)]
    pub enum GuestSecureBootTemplateType {
        /// No template specified.
        None,
        /// The microsoft windows template.
        MicrosoftWindows,
        /// The Microsoft UEFI certificate authority template.
        MicrosoftUefiCertificateAuthoritiy,
    }

    /// The boot devices for a PC/AT BIOS.
    #[derive(MeshPayload, Debug, Clone, Copy, PartialEq)]
    pub enum PcatBootDevice {
        /// Boot from a floppy disk.
        Floppy,
        /// Boot from a hard drive.
        HardDrive,
        /// Boot from an optical drive.
        Optical,
        /// Boot from the network.
        Network,
    }

    impl ResourceId<VmbusDeviceHandleKind> for GuestEmulationDeviceHandle {
        const ID: &'static str = "ged";
    }

    /// Define servicing behavior.
    #[derive(MeshPayload, Default)]
    pub struct GuestServicingFlags {
        /// Retain memory for DMA-attached devices.
        pub nvme_keepalive: bool,
    }

    /// Actions a client can request that the Guest Emulation
    /// Device perform.
    #[derive(MeshPayload)]
    pub enum GuestEmulationRequest {
        /// Wait for VTL2 to connect to the GET.
        WaitForConnect(Rpc<(), ()>),
        /// Wait for VTL2 to start VTL0.
        WaitForVtl0Start(Rpc<(), Result<(), Vtl0StartError>>),
        /// Save VTL2 state.
        SaveGuestVtl2State(Rpc<GuestServicingFlags, Result<(), SaveRestoreError>>),
        /// Update the VTL2 settings.
        ModifyVtl2Settings(Rpc<Vec<u8>, Result<(), ModifyVtl2SettingsError>>),
    }

    /// An error waiting to start VTL0.
    #[derive(Debug, Error, Clone, MeshPayload)]
    #[error("guest reported VTL0 start error: {0}")]
    pub struct Vtl0StartError(pub String);

    /// The various errors that can occur during a save or restore
    /// operation for guest VTL2 state.
    #[derive(Debug, Error, MeshPayload)]
    #[expect(missing_docs)]
    pub enum SaveRestoreError {
        #[error("an operation is in progress")]
        OperationInProgress,
        #[error("vmbus io error")]
        Io(#[source] RemoteError),
        #[error("guest error")]
        GuestError,
    }

    /// An error that can occur during a VTL2 settings update.
    #[derive(Debug, Error, MeshPayload)]
    #[expect(missing_docs)]
    pub enum ModifyVtl2SettingsError {
        #[error("large settings not supported")]
        LargeSettingsNotSupported,
        #[error("an operation is already in progress")]
        OperationInProgress,
        #[error("guest error: {0}")]
        Guest(String),
    }

    /// Firmware events generated by the guest.
    ///
    /// TODO: For now, these mainly represent UEFI events without the corresponding extra information. This should be
    ///       rethought when HvLite supports Linux Direct, IGVM, and other types.
    #[derive(Debug, Protobuf, PartialEq, Eq, Copy, Clone)]
    pub enum FirmwareEvent {
        /// Boot was successful.
        BootSuccess,
        /// Boot failed.
        BootFailed,
        /// No boot device could be found.
        NoBootDevice,
        /// A boot attempt was made.
        BootAttempt,
    }
}
