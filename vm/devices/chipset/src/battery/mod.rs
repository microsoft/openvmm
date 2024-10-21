// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtual battery device.
//!
//! This virtual battery device simulates the host's battery state for the
//! guest. Rather than modeling a real device, it's a Hyper-V specific design,
//! tailored to meet the
//! [ACPI Battery and Power Subsystem Firmware Implementation][acpi] and the
//! additional requirements imposed by the
//! [Windows Hardware Design Guidelines][whdg].
//!
//! This device was implemented alongside the ACPI code in Hyper-V UEFI. For
//! more information, refer to the DSDT in the UEFI codebase: [mu_msvm][uefi].
//!
//! For historical context, this device was originally designed for x86 and was
//! later adapted for ARM64.
//!
//! This device uses `LineInterrupts` to signal state changes to interested
//! parties (e.g: the PM device on x86, a system IRQ on Aarch64). A mesh channel
//! is used to receive battery state updates from the host platform.
//!
//! Refer to the following resources for more context:
//! - [ACPI Battery and Power Subsystem Firmware Implementation][acpi]
//! - [UEFI codebase (mu_msvm)][uefi]
//! - [Windows Hardware Design Guidelines][whdg]
//!
//! [acpi]: https://uefi.org/htmlspecs/ACPI_Spec_6_4_html/10_Power_Source_and_Power_Meter_Devices/Power_Source_and_Power_Meter_Devices.html#control-method-batteries
//! [uefi]: https://github.com/Microsoft/mu_msvm
//! [whdg]: https://docs.microsoft.com/en-us/windows-hardware/design/component-guidelines/acpi-battery-and-power-subsystem-firmware-implementation

pub mod resolver;

use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use chipset_resources::battery::HostBatteryUpdate;
use futures::StreamExt;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use std::ops::RangeInclusive;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;

// Battery MMIO constants
pub const BATTERY_MMIO_REGION_BASE_ADDRESS_X64: u64 = 0xfed3f000;
pub const BATTERY_MMIO_REGION_BASE_ADDRESS_ARM: u64 = 0xEFFEA000;
pub const BATTERY_DEVICE_MMIO_REGION_SIZE: u64 = 0x20;
pub const BATTERY_DEVICE_MMIO_REGION_MASK: u64 = BATTERY_DEVICE_MMIO_REGION_SIZE - 1;

// Battery interrupt lines. For x64, use GPE0 bit 9.
// For ARM64, use IRQ 4 [derived from 4 + 32 (SPI range start) = 36].
pub const BATTERY_STATUS_GPE0_LINE: u32 = 9;
pub const BATTERY_STATUS_IRQ_NO: u32 = 4;

// Bits to set for notifications.
//
// NOTE: ACPI_DEVICE_NOTIFY_BST_CHANGED is deprecated and should not be used.
// It is kept here for reference.
//
// No functionality is lost by not using this constant, but we prefer
// ACPI_DEVICE_NOTIFY_BIX_CHANGED because it tells the guest to check for both
// the BST and BIX registers.
#[allow(unused)]
pub const ACPI_DEVICE_NOTIFY_BST_CHANGED: u32 = 0x1;
pub const ACPI_DEVICE_NOTIFY_BIX_CHANGED: u32 = 0x2;

// Defines what bits are allowed to be set for notifications
pub const ACPI_DEVICE_NOTIFY_VALID_BITS: u32 = 0x3;

// Virtual battery capacity, based on the UEFI DSDT's design capacity
pub const VIRTUAL_BATTERY_CAPACITY: u32 = 5000;

// Battery register offsets
open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
    pub enum RegisterOffset: u64 {
        STA_BATTERY_STATUS = 0x0,
        BST_BATTERY_STATE = 0x4,
        BST_BATTERY_PRESENT_RATE = 0x8,
        BST_BATTERY_REMAINING_CAPACITY = 0xc,
        PSR_AC_POWER_STATUS = 0x10,
        BATTERY_ACPI_NOTIFY_STATUS = 0x14,
        BATTERY_ACPI_NOTIFY_CLEAR = 0x18,
    }
}

/// Various runtime objects used by the BatteryDevice
pub struct BatteryRuntimeDeps {
    pub battery_status_recv: mesh::Receiver<HostBatteryUpdate>,
    pub notify_interrupt: LineInterrupt,
}

/// Virtual battery device.
#[derive(InspectMut)]
pub struct BatteryDevice {
    // Runtime glue
    #[inspect(skip)]
    rt: BatteryRuntimeDeps,

    // Static configuration
    #[inspect(skip)]
    mmio_region: (&'static str, RangeInclusive<u64>),
    base_addr: u64,

    // Volatile state
    notify_bits: u32,
    state: HostBatteryUpdate,
}

impl BatteryDevice {
    /// Create a new battery device
    pub fn new(platform: BatteryRuntimeDeps, base_addr: u64) -> Self {
        BatteryDevice {
            rt: platform,
            mmio_region: (
                "battery",
                base_addr..=base_addr + (BATTERY_DEVICE_MMIO_REGION_SIZE - 1),
            ),
            base_addr,
            state: HostBatteryUpdate::default(),
            notify_bits: 0,
        }
    }

    fn read_register(&self, offset: RegisterOffset) -> u32 {
        match offset {
            RegisterOffset::STA_BATTERY_STATUS => {
                if self.state.battery_present {
                    0x1F
                } else {
                    0xF
                }
            }
            RegisterOffset::BST_BATTERY_STATE => {
                if !self.state.battery_present {
                    0
                } else if self.state.charging {
                    0x2
                } else if self.state.discharging {
                    0x1
                } else {
                    // Somehow, we got in some weird state, return default 0.
                    tracelimit::warn_ratelimited!(
                        "BST_BATTERY_STATE encountered a weird state, defaulting to 0"
                    );
                    0
                }
            }
            RegisterOffset::BST_BATTERY_PRESENT_RATE => {
                if self.state.battery_present && self.state.max_capacity != 0 {
                    // Normalize the rate to the virtual battery's capacity.
                    (self.state.rate.saturating_mul(VIRTUAL_BATTERY_CAPACITY))
                        / self.state.max_capacity
                } else {
                    // Unknown rate.
                    0xFFFFFFFF
                }
            }
            RegisterOffset::BST_BATTERY_REMAINING_CAPACITY => {
                if self.state.battery_present && self.state.max_capacity != 0 {
                    // Normalize the remaining capacity to the virtual battery's capacity.
                    (self
                        .state
                        .remaining_capacity
                        .saturating_mul(VIRTUAL_BATTERY_CAPACITY))
                        / self.state.max_capacity
                } else {
                    // Unknown capacity.
                    0xFFFFFFFF
                }
            }
            RegisterOffset::PSR_AC_POWER_STATUS => {
                if self.state.ac_online {
                    0x1
                } else {
                    0x0
                }
            }
            RegisterOffset::BATTERY_ACPI_NOTIFY_STATUS => self.notify_bits,
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: RegisterOffset, value: u32) {
        if offset != RegisterOffset::BATTERY_ACPI_NOTIFY_CLEAR {
            // Only writes allowed are to clear notification bits.
            tracelimit::warn_ratelimited!("Invalid write to battery device at offset {:?}", offset);
            return;
        }

        // Clear any bits that were set to 1
        self.notify_bits &= !value;
        self.notify_bits &= ACPI_DEVICE_NOTIFY_VALID_BITS;

        // Re-evaluate interrupt for any pending bits left
        self.check_interrupt_assertion();
    }

    /// evaluates whether the battery's interrupt should be
    /// asserted or de-asserted
    fn check_interrupt_assertion(&self) {
        self.rt.notify_interrupt.set_level(self.notify_bits != 0)
    }
}

impl ChangeDeviceState for BatteryDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let Self {
            rt,
            mmio_region: _,
            base_addr: _,
            notify_bits,
            state,
        } = self;
        *state = HostBatteryUpdate::default();
        rt.notify_interrupt.set_level(false);
        *notify_bits = 0;
    }
}

impl ChipsetDevice for BatteryDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl MmioIntercept for BatteryDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        assert_eq!(address & !BATTERY_DEVICE_MMIO_REGION_MASK, self.base_addr);
        if data.len() == size_of::<u32>() {
            let value =
                self.read_register(RegisterOffset(address & BATTERY_DEVICE_MMIO_REGION_MASK));
            data.copy_from_slice(&value.to_ne_bytes());
            IoResult::Ok
        } else {
            IoResult::Err(IoError::InvalidAccessSize)
        }
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        assert_eq!(address & !BATTERY_DEVICE_MMIO_REGION_MASK, self.base_addr);
        if let Ok(x) = data.try_into().map(u32::from_ne_bytes) {
            self.write_register(RegisterOffset(address & BATTERY_DEVICE_MMIO_REGION_MASK), x);
            IoResult::Ok
        } else {
            IoResult::Err(IoError::InvalidAccessSize)
        }
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.mmio_region)
    }
}

impl PollDevice for BatteryDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        while let std::task::Poll::Ready(Some(update)) =
            self.rt.battery_status_recv.poll_next_unpin(cx)
        {
            self.state = update;
            if self.state.battery_present && self.state.max_capacity == 0 {
                // This configuration makes no sense. Just report no battery,
                // and set AC power accordingly.
                tracelimit::warn_ratelimited!("BATTERY: Invalid state: max_capacity is 0 but battery is present. Marking battery as not present.");
                self.state.battery_present = false;
            }
            // Add the corresponding status bit to notification status
            self.notify_bits |= ACPI_DEVICE_NOTIFY_BIX_CHANGED;
            self.check_interrupt_assertion();
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.battery")]
        pub struct SavedState {
            #[mesh(1)]
            pub notify_bits: u32,
            #[mesh(2)]
            pub battery_state: BatterySavedState,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.battery")]
        pub struct BatterySavedState {
            #[mesh(1)]
            pub battery_present: bool,
            #[mesh(2)]
            pub charging: bool,
            #[mesh(3)]
            pub discharging: bool,
            #[mesh(4)]
            pub rate: u32,
            #[mesh(5)]
            pub remaining_capacity: u32,
            #[mesh(6)]
            pub max_capacity: u32,
            #[mesh(7)]
            pub ac_online: bool,
        }
    }

    impl SaveRestore for BatteryDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Self {
                rt: _,
                mmio_region: _,
                base_addr: _,
                notify_bits,
                state:
                    HostBatteryUpdate {
                        battery_present,
                        charging,
                        discharging,
                        rate,
                        remaining_capacity,
                        max_capacity,
                        ac_online,
                    },
            } = *self;

            let saved_state = state::SavedState {
                notify_bits,
                battery_state: state::BatterySavedState {
                    battery_present,
                    charging,
                    discharging,
                    rate,
                    remaining_capacity,
                    max_capacity,
                    ac_online,
                },
            };

            Ok(saved_state)
        }

        fn restore(&mut self, saved_state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                notify_bits,
                battery_state:
                    state::BatterySavedState {
                        battery_present,
                        charging,
                        discharging,
                        rate,
                        remaining_capacity,
                        max_capacity,
                        ac_online,
                    },
            } = saved_state;

            self.notify_bits = notify_bits;
            self.state = HostBatteryUpdate {
                battery_present,
                charging,
                discharging,
                rate,
                remaining_capacity,
                max_capacity,
                ac_online,
            };

            self.check_interrupt_assertion();

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::Context;
    use vmcore::line_interrupt::LineInterrupt;

    fn create_test_platform() -> (BatteryDevice, mesh::Sender<HostBatteryUpdate>) {
        let (tx, rx) = mesh::channel::<HostBatteryUpdate>();
        let battery = BatteryDevice::new(
            BatteryRuntimeDeps {
                battery_status_recv: {
                    tx.send(HostBatteryUpdate::default());
                    rx
                },
                notify_interrupt: LineInterrupt::detached(),
            },
            BATTERY_MMIO_REGION_BASE_ADDRESS_X64,
        );
        (battery, tx)
    }

    fn mmio_read_helper(battery: &mut BatteryDevice, offset: u64) -> [u8; 4] {
        let mut bytes = [0; 4];
        battery
            .mmio_read(battery.base_addr + offset, &mut bytes)
            .unwrap();
        bytes
    }

    fn check_mmio_read(battery: &mut BatteryDevice, offset: u64, expected_value: u32) {
        assert_eq!(
            u32::from_ne_bytes(mmio_read_helper(battery, offset)),
            expected_value
        );
        assert_eq!(
            battery.read_register(RegisterOffset(offset)),
            expected_value
        );
    }

    fn send_update(
        battery: &mut BatteryDevice,
        update: HostBatteryUpdate,
        sender: &mesh::Sender<HostBatteryUpdate>,
    ) {
        sender.send(update);
        battery.poll_device(&mut Context::from_waker(futures::task::noop_waker_ref()));
    }

    /// Test basic battery mmio behavior
    #[test]
    fn test_basic_battery_mmio() {
        // create battery, send channel, and a blank battery state
        let (mut battery, tx) = create_test_platform();
        let mut state = HostBatteryUpdate::default();

        // test uninitialized state.
        check_mmio_read(&mut battery, RegisterOffset::STA_BATTERY_STATUS.0, 0xF);
        check_mmio_read(&mut battery, RegisterOffset::BST_BATTERY_STATE.0, 0);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_PRESENT_RATE.0,
            0xFFFFFFFF,
        );
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_REMAINING_CAPACITY.0,
            0xFFFFFFFF,
        );
        check_mmio_read(&mut battery, RegisterOffset::PSR_AC_POWER_STATUS.0, 0);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BATTERY_ACPI_NOTIFY_STATUS.0,
            0,
        );

        // set battery to be present, with AC power and full capacity
        state.battery_present = true;
        state.remaining_capacity = 100;
        state.max_capacity = 100;
        state.ac_online = true;
        send_update(&mut battery, state, &tx);

        check_mmio_read(&mut battery, RegisterOffset::STA_BATTERY_STATUS.0, 0x1F);
        check_mmio_read(&mut battery, RegisterOffset::BST_BATTERY_STATE.0, 0);
        check_mmio_read(&mut battery, RegisterOffset::BST_BATTERY_PRESENT_RATE.0, 0);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_REMAINING_CAPACITY.0,
            (100 * VIRTUAL_BATTERY_CAPACITY) / 100,
        );
        check_mmio_read(&mut battery, RegisterOffset::PSR_AC_POWER_STATUS.0, 0x1);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BATTERY_ACPI_NOTIFY_STATUS.0,
            2,
        );

        // set battery to be charging, with 50% capacity
        state.charging = true;
        state.remaining_capacity = 50;
        state.rate = 40;
        send_update(&mut battery, state, &tx);

        check_mmio_read(&mut battery, RegisterOffset::STA_BATTERY_STATUS.0, 0x1F);
        check_mmio_read(&mut battery, RegisterOffset::BST_BATTERY_STATE.0, 0x2);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_PRESENT_RATE.0,
            (40 * VIRTUAL_BATTERY_CAPACITY) / 100,
        );
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_REMAINING_CAPACITY.0,
            (50 * VIRTUAL_BATTERY_CAPACITY) / 100,
        );
        check_mmio_read(&mut battery, RegisterOffset::PSR_AC_POWER_STATUS.0, 0x1);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BATTERY_ACPI_NOTIFY_STATUS.0,
            2,
        );

        // set battery to be discharging, no ac, 50% capacity
        state.ac_online = false;
        state.charging = false;
        state.discharging = true;
        state.rate = 45;
        send_update(&mut battery, state, &tx);
        check_mmio_read(&mut battery, RegisterOffset::STA_BATTERY_STATUS.0, 0x1F);
        check_mmio_read(&mut battery, RegisterOffset::BST_BATTERY_STATE.0, 0x1);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_PRESENT_RATE.0,
            (45 * VIRTUAL_BATTERY_CAPACITY) / 100,
        );
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_REMAINING_CAPACITY.0,
            (50 * VIRTUAL_BATTERY_CAPACITY) / 100,
        );
        check_mmio_read(&mut battery, RegisterOffset::PSR_AC_POWER_STATUS.0, 0x0);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BATTERY_ACPI_NOTIFY_STATUS.0,
            2,
        );

        // ensure that mmio_write clears the notify_bits
        let data: u32 = 0x2;
        let _ = battery.mmio_write(
            battery.base_addr + RegisterOffset::BATTERY_ACPI_NOTIFY_CLEAR.0,
            &data.to_ne_bytes(),
        );
        assert_eq!(battery.notify_bits, 0);
    }

    /// Test values when battery is not present
    #[test]
    fn test_battery_not_present() {
        // create battery and send channel
        let (mut battery, tx) = create_test_platform();

        // set ac power online, no battery present
        let state = HostBatteryUpdate {
            ac_online: true,
            ..HostBatteryUpdate::default()
        };
        send_update(&mut battery, state, &tx);
        check_mmio_read(&mut battery, RegisterOffset::STA_BATTERY_STATUS.0, 0xF);
        check_mmio_read(&mut battery, RegisterOffset::BST_BATTERY_STATE.0, 0);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_PRESENT_RATE.0,
            0xFFFFFFFF,
        );
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_REMAINING_CAPACITY.0,
            0xFFFFFFFF,
        );
        check_mmio_read(&mut battery, RegisterOffset::PSR_AC_POWER_STATUS.0, 0x1);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BATTERY_ACPI_NOTIFY_STATUS.0,
            0x2,
        );
    }

    /// Test bad register read/write operations and ensure they error
    #[test]
    fn test_bad_register_read_writes() {
        // create battery and send channel
        let (mut battery, _) = create_test_platform();

        // Test reading with a data size not equal to size_of::<u32>()
        let mut data = vec![0; size_of::<u32>() + 1];
        match battery.mmio_read(battery.base_addr, &mut data) {
            IoResult::Err(e) => assert!(matches!(e, IoError::InvalidAccessSize)),
            _ => panic!("Expected error, but got Ok"),
        }

        // Test writing with a data size not equal to size_of::<u32>()
        let data = vec![0; size_of::<u32>() + 1];
        match battery.mmio_write(battery.base_addr, &data) {
            IoResult::Err(e) => assert!(matches!(e, IoError::InvalidAccessSize)),
            _ => panic!("Expected error, but got Ok"),
        }

        // Test writing with data that cannot be converted into a `u32`
        let data = vec![0; size_of::<u32>() - 1];
        match battery.mmio_write(battery.base_addr, &data) {
            IoResult::Err(e) => assert!(matches!(e, IoError::InvalidAccessSize)),
            _ => panic!("Expected error, but got Ok"),
        }
    }

    /// Test bad capacity on a battery
    #[test]
    fn test_bad_battery_capacity() {
        // create battery and send channel
        let (mut battery, tx) = create_test_platform();

        // set battery present with 50% capacity and 0 max capacity
        let state = HostBatteryUpdate {
            battery_present: true,
            remaining_capacity: 50,
            max_capacity: 0,
            ac_online: true,
            ..HostBatteryUpdate::default()
        };
        send_update(&mut battery, state, &tx);
        check_mmio_read(&mut battery, RegisterOffset::STA_BATTERY_STATUS.0, 0xF);
        check_mmio_read(&mut battery, RegisterOffset::BST_BATTERY_STATE.0, 0x0);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_PRESENT_RATE.0,
            0xFFFFFFFF,
        );
        check_mmio_read(
            &mut battery,
            RegisterOffset::BST_BATTERY_REMAINING_CAPACITY.0,
            0xFFFFFFFF,
        );
        check_mmio_read(&mut battery, RegisterOffset::PSR_AC_POWER_STATUS.0, 0x1);
        check_mmio_read(
            &mut battery,
            RegisterOffset::BATTERY_ACPI_NOTIFY_STATUS.0,
            0x2,
        );
    }
}
