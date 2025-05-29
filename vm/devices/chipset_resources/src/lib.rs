// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for core chipset devices.

#![forbid(unsafe_code)]

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
