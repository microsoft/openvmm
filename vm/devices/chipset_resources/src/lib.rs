// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for core chipset devices.

#![forbid(unsafe_code)]

/// The PCI bus name used by the Gen1 (i440BX + PIIX4) chipset.
pub const LEGACY_CHIPSET_PCI_BUS_NAME: &str = "i440bx";

pub mod i8042 {
    //! Resource definitions for the i8042 PS2 keyboard/mouse controller.

    use mesh::MeshPayload;
    use vm_resource::Resource;
    use vm_resource::ResourceId;
    use vm_resource::kind::ChipsetDeviceHandleKind;
    use vm_resource::kind::KeyboardInputHandleKind;

    /// A handle to an i8042 PS2 keyboard/mouse controller controller.
    #[derive(MeshPayload)]
    pub struct I8042DeviceHandle {
        /// The keyboard input.
        pub keyboard_input: Resource<KeyboardInputHandleKind>,
    }

    impl ResourceId<ChipsetDeviceHandleKind> for I8042DeviceHandle {
        const ID: &'static str = "i8042";
    }
}

pub mod pic {
    //! Resource definitions for the PIC (dual 8259 Programmable Interrupt Controller).

    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::ChipsetDeviceHandleKind;

    /// A handle to a dual 8259 PIC (Programmable Interrupt Controller) device.
    #[derive(MeshPayload)]
    pub struct PicDeviceHandle;

    impl ResourceId<ChipsetDeviceHandleKind> for PicDeviceHandle {
        const ID: &'static str = "pic";
    }
}

pub mod pit {
    //! Resource definitions for the PIT (Programmable Interval Timer).

    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::ChipsetDeviceHandleKind;

    /// A handle to a PIT (Intel 8253/8254 Programmable Interval Timer) device.
    #[derive(MeshPayload)]
    pub struct PitDeviceHandle;

    impl ResourceId<ChipsetDeviceHandleKind> for PitDeviceHandle {
        const ID: &'static str = "pit";
    }
}

pub mod battery {
    //! Resource definitions for the battery device

    #[cfg(feature = "arbitrary")]
    use arbitrary::Arbitrary;
    use inspect::Inspect;
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::ChipsetDeviceHandleKind;
    /// A handle to a battery device for x64
    #[derive(MeshPayload)]
    pub struct BatteryDeviceHandleX64 {
        /// Channel to receive updated state
        pub battery_status_recv: mesh::Receiver<HostBatteryUpdate>,
    }

    impl ResourceId<ChipsetDeviceHandleKind> for BatteryDeviceHandleX64 {
        const ID: &'static str = "batteryX64";
    }

    /// A handle to a battery device for aarch64
    #[derive(MeshPayload)]
    pub struct BatteryDeviceHandleAArch64 {
        /// Channel to receive updated state
        pub battery_status_recv: mesh::Receiver<HostBatteryUpdate>,
    }

    impl ResourceId<ChipsetDeviceHandleKind> for BatteryDeviceHandleAArch64 {
        const ID: &'static str = "batteryAArch64";
    }

    /// Updated battery state from the host
    #[derive(Debug, Clone, Copy, Inspect, PartialEq, Eq, MeshPayload, Default)]
    #[cfg_attr(feature = "arbitrary", derive(Arbitrary))]
    pub struct HostBatteryUpdate {
        /// Is the battery present?
        pub battery_present: bool,
        /// Is the battery charging?
        pub charging: bool,
        /// Is the battery discharging?
        pub discharging: bool,
        /// Provides the current rate of drain in milliwatts from the battery.
        pub rate: u32,
        /// Provides the remaining battery capacity in milliwatt-hours.
        pub remaining_capacity: u32,
        /// Provides the max capacity of the battery in `milliwatt-hours`
        pub max_capacity: u32,
        /// Is ac online?
        pub ac_online: bool,
    }

    impl HostBatteryUpdate {
        /// Returns a default `HostBatteryUpdate` with the battery present and charging.
        pub fn default_present() -> Self {
            Self {
                battery_present: true,
                charging: true,
                discharging: false,
                rate: 1,
                remaining_capacity: 950,
                max_capacity: 1000,
                ac_online: true,
            }
        }
    }
}

pub mod piix4_pci_isa_bridge {
    //! Resource definitions for the PIIX4 PCI-ISA bridge device.

    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::ChipsetDeviceHandleKind;

    /// A handle to the PIIX4 PCI-to-ISA bridge (PCI device function 0).
    #[derive(MeshPayload)]
    pub struct Piix4PciIsaBridgeDeviceHandle;

    /// The fixed BDF used by the PIIX4 PCI-ISA bridge in the Gen1 chipset.
    pub const PIIX4_PCI_ISA_BRIDGE_BDF: (u8, u8, u8) = (0, 7, 0);

    impl ResourceId<ChipsetDeviceHandleKind> for Piix4PciIsaBridgeDeviceHandle {
        const ID: &'static str = "piix4PciIsaBridge";
    }
}

pub mod piix4_uhci {
    //! Resource definitions for the PIIX4 USB UHCI stub device.

    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::ChipsetDeviceHandleKind;

    /// A handle to the PIIX4 USB UHCI stub controller.
    #[derive(MeshPayload)]
    pub struct Piix4PciUsbUhciStubDeviceHandle;

    /// The fixed BDF used by the PIIX4 USB UHCI stub in the Gen1 chipset.
    pub const PIIX4_PCI_USB_UHCI_STUB_BDF: (u8, u8, u8) = (0, 7, 2);

    impl ResourceId<ChipsetDeviceHandleKind> for Piix4PciUsbUhciStubDeviceHandle {
        const ID: &'static str = "piix4PciUsbUhciStub";
    }
}

pub mod pm {
    //! Resource definitions for power management devices.

    use mesh::MeshPayload;
    use vm_resource::CanResolveTo;
    use vm_resource::Resource;
    use vm_resource::ResourceId;
    use vm_resource::ResourceKind;
    use vm_resource::kind::ChipsetDeviceHandleKind;

    /// Interface to enable/disable hypervisor PM timer assist.
    pub trait PmTimerAssist: Send + Sync {
        /// Sets the port of the PM timer assist, or disables it if `None`.
        fn set(&self, port: Option<u16>);
    }

    /// Resolved PM timer assist, wrapping a boxed trait object.
    pub struct ResolvedPmTimerAssist(pub Box<dyn PmTimerAssist>);

    /// Resource kind for PM timer assist implementations.
    pub enum PmTimerAssistHandleKind {}

    impl ResourceKind for PmTimerAssistHandleKind {
        const NAME: &'static str = "pm_timer_assist";
    }

    impl CanResolveTo<ResolvedPmTimerAssist> for PmTimerAssistHandleKind {
        type Input<'a> = ();
    }

    /// A handle to the Hyper-V power management device (non-PCI, ACPI/PIO).
    #[derive(MeshPayload)]
    pub struct HyperVPowerManagementDeviceHandle {
        /// IRQ line triggered on ACPI power event.
        pub acpi_irq: u32,
        /// Base port IO address of the device's dynamic register region.
        pub pio_base: u16,
        /// Optional PM timer assist resource.
        pub pm_timer_assist: Option<Resource<PmTimerAssistHandleKind>>,
    }

    impl ResourceId<ChipsetDeviceHandleKind> for HyperVPowerManagementDeviceHandle {
        const ID: &'static str = "hyperv_power_management";
    }

    /// A handle to the PIIX4 power management device (PCI function 3).
    #[derive(MeshPayload)]
    pub struct Piix4PowerManagementDeviceHandle {
        /// Optional PM timer assist resource.
        pub pm_timer_assist: Option<Resource<PmTimerAssistHandleKind>>,
    }

    /// The fixed BDF used by the PIIX4 PM device in the Gen1 chipset.
    pub const PIIX4_PM_BDF: (u8, u8, u8) = (0, 7, 3);

    /// Default PIO base address for the PM dynamic register region.
    ///
    /// This value must match what is reported by the firmware (FADT).
    pub const DEFAULT_PM_PIO_BASE: u16 = 0x400;

    /// Default ACPI IRQ line for the Hyper-V power management device.
    pub const DEFAULT_ACPI_IRQ: u32 = 9;

    impl ResourceId<ChipsetDeviceHandleKind> for Piix4PowerManagementDeviceHandle {
        const ID: &'static str = "piix4_power_management";
    }
}
