// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for core chipset devices.

#![forbid(unsafe_code)]

use local_clock::InspectableLocalClock;
use vm_resource::CanResolveTo;
use vm_resource::ResourceKind;

/// The PCI bus name used by the Gen1 (i440BX + PIIX4) chipset.
pub const LEGACY_CHIPSET_PCI_BUS_NAME: &str = "i440bx";

/// Resource kind for CMOS RTC time-source handles.
pub enum CmosRtcTimeSourceHandleKind {}

impl ResourceKind for CmosRtcTimeSourceHandleKind {
    const NAME: &'static str = "cmos_rtc_time_source";
}

/// Resolved runtime time source for CMOS RTC devices.
pub struct ResolvedCmosRtcTimeSource(pub Box<dyn InspectableLocalClock>);

impl CanResolveTo<ResolvedCmosRtcTimeSource> for CmosRtcTimeSourceHandleKind {
    type Input<'a> = ();
}

pub mod cmos_rtc_time_source {
    //! Resource definitions and resolvers for CMOS RTC time sources.

    use super::CmosRtcTimeSourceHandleKind;
    use super::ResolvedCmosRtcTimeSource;
    use local_clock::LocalClockDelta;
    use local_clock::SystemTimeClock;
    use mesh::MeshPayload;
    use vm_resource::ResolveResource;
    use vm_resource::ResourceId;
    use vm_resource::declare_static_resolver;

    /// A time source backed by the host system clock with a configurable
    /// millisecond delta.
    #[derive(MeshPayload)]
    pub struct SystemTimeClockHandle {
        /// Offset from system time in milliseconds.
        pub delta_milliseconds: i64,
    }

    impl ResourceId<CmosRtcTimeSourceHandleKind> for SystemTimeClockHandle {
        const ID: &'static str = "system_time_clock";
    }

    /// Resolver for [`SystemTimeClockHandle`].
    pub struct SystemTimeClockResolver;

    declare_static_resolver! {
        SystemTimeClockResolver,
        (CmosRtcTimeSourceHandleKind, SystemTimeClockHandle),
    }

    impl ResolveResource<CmosRtcTimeSourceHandleKind, SystemTimeClockHandle>
        for SystemTimeClockResolver
    {
        type Output = ResolvedCmosRtcTimeSource;
        type Error = std::convert::Infallible;

        fn resolve(
            &self,
            resource: SystemTimeClockHandle,
            (): (),
        ) -> Result<Self::Output, Self::Error> {
            Ok(ResolvedCmosRtcTimeSource(Box::new(SystemTimeClock::new(
                LocalClockDelta::from_millis(resource.delta_milliseconds),
            ))))
        }
    }
}

pub mod cmos_rtc {
    //! Resource definitions for CMOS RTC devices.

    use super::CmosRtcTimeSourceHandleKind;
    use mesh::MeshPayload;
    use vm_resource::Resource;
    use vm_resource::ResourceId;
    use vm_resource::kind::ChipsetDeviceHandleKind;

    /// Handle for the generic MC146818-compatible CMOS RTC device.
    #[derive(MeshPayload)]
    pub struct GenericCmosRtcDeviceHandle {
        /// IRQ line to signal RTC device events.
        pub irq: u32,
        /// Which CMOS RAM register contains the century register.
        pub century_reg_idx: u8,
        /// Initial state of CMOS RAM.
        pub initial_cmos: Option<[u8; 256]>,
        /// Runtime clock source for this RTC instance.
        pub time_source: Resource<CmosRtcTimeSourceHandleKind>,
    }

    impl ResourceId<ChipsetDeviceHandleKind> for GenericCmosRtcDeviceHandle {
        const ID: &'static str = "genericCmosRtc";
    }

    /// Handle for the PIIX4 CMOS RTC device.
    #[derive(MeshPayload)]
    pub struct Piix4CmosRtcDeviceHandle {
        /// Initial state of CMOS RAM.
        pub initial_cmos: Option<[u8; 256]>,
        /// Whether enlightened interrupts are enabled.
        pub enlightened_interrupts: bool,
        /// Runtime clock source for this RTC instance.
        pub time_source: Resource<CmosRtcTimeSourceHandleKind>,
    }

    impl ResourceId<ChipsetDeviceHandleKind> for Piix4CmosRtcDeviceHandle {
        const ID: &'static str = "piix4CmosRtc";
    }
}

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
