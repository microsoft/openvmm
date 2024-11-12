// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::config::BootDeviceStatus;

/// Return the BIOS boot order DWORD based on the provided boot devices.
///
/// This implementation assumes that each device kind appears in the boot order
/// exactly once.
///
/// A BIOS boot order consists of a series of nibbles, where each position is
/// associated with a specific boot device category. The lowest order nibble is
/// for floppy devices, then optical, then hard drives, and lastly network boot.
///
/// The value of these nibbles is the order in which they should be attempted.
///
/// However, unattached devices are skipped over, and any devices that would
/// normally occur after them are instead shifted down to take their place. So
/// for example, if a computer has no optical drives attached, the order goes
/// floppy, hard drives, network, with no gaps. The remaining nibbles are filled
/// by counting up. All nibble values from 0 - 7 inclusive must be used exactly
/// once.
///
/// Examples below:
///
/// ```text
/// VM:               Boot Order:           BIOS Boot Order:
///  Has Floppy        Floppy = 0            0x76543210
///  Has CD            CD     = 1
///  Has IDE           IDE    = 2
///  Has Net           Net    = 3
///
///                                    Net     IDE      CD    Floppy
/// |-------|-------|-------|-------|-------|-------|-------|-------|
/// |       |       |       |       |       |       |       |       |
/// |   N   |   N   |   N   |   N   |   3   |   2   |   1   |   0   | <-- BIOS Boot Order
/// |       |       |       |       |       |       |       |       |
/// |-------|-------|-------|-------|-------|-------|-------|-------|
///
///
///
/// VM:               Boot Order:           BIOS Boot Order:
///  Has  Floppy       Net    = 3            0x76543201
///  *No* CD           Floppy = 0
///  *No* IDE          CD     = 1
///  Has  Net          IDE    = 2
///
///                                     X       X      Net    Floppy
/// |-------|-------|-------|-------|-------|-------|-------|-------|
/// |       |       |       |       |       |       |       |       |
/// |   N   |   N   |   N   |   N   |   N   |   N   |   0   |   1   | <-- BIOS Boot Order
/// |       |       |       |       |       |       |       |       |
/// |-------|-------|-------|-------|-------|-------|-------|-------|
///
///
///
/// VM:               Boot Order:          BIOS Boot Order:
///  Has  Floppy       Net    = 3           0x76543021
///  Has  CD           Floppy = 0
///  *No* IDE          CD     = 1
///  Has  Net          IDE    = 2
///
///                                     X      Net     CD    Floppy
/// |-------|-------|-------|-------|-------|-------|-------|-------|
/// |       |       |       |       |       |       |       |       |
/// |   N   |   N   |   N   |   N   |   N   |   0   |   2   |   1   | <-- BIOS Boot Order
/// |       |       |       |       |       |       |       |       |
/// |-------|-------|-------|-------|-------|-------|-------|-------|
/// ```
///
/// reSearch query: `CreateBiosBootOrder`
pub fn bios_boot_order(boot_order: &[BootDeviceStatus]) -> u32 {
    const DEFAULT_BOOT_ORDER: u32 = 0x76543210;
    let mut computed_order = 0;
    let mut mask = 0xFFFFFFFF;

    // Map of BootDeviceKind to final nibble index
    let mut position_mapping = [0, 1, 2, 3];

    for device in boot_order.iter().filter(|d| !d.attached) {
        position_mapping[device.kind as usize..]
            .iter_mut()
            // underflow fine, as it makes the bit-shift below becoming a noop
            .for_each(|m| *m -= 1);
    }

    for (i, device) in boot_order.iter().filter(|d| d.attached).enumerate() {
        computed_order |= (i as u32) << (position_mapping[device.kind as usize] * 4);
        mask <<= 4;
    }

    DEFAULT_BOOT_ORDER & mask | computed_order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BootDevice;

    macro_rules! assert_eq_hex {
        ($left:expr, $right:expr) => {
            assert!($left == $right, "{:#x} != {:#x}", $left, $right)
        };
    }

    #[test]
    fn all_connected() {
        let boot_order = [
            BootDeviceStatus {
                kind: BootDevice::Floppy,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::Optical,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::HardDrive,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::Network,
                attached: true,
            },
        ];
        assert_eq_hex!(bios_boot_order(&boot_order), 0x76543210);
    }

    #[test]
    fn some_missing() {
        let boot_order = [
            BootDeviceStatus {
                kind: BootDevice::Network,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::Floppy,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::Optical,
                attached: false,
            },
            BootDeviceStatus {
                kind: BootDevice::HardDrive,
                attached: false,
            },
        ];
        assert_eq_hex!(bios_boot_order(&boot_order), 0x76543201);
    }

    #[test]
    fn no_hdd() {
        let boot_order = [
            BootDeviceStatus {
                kind: BootDevice::Network,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::Floppy,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::Optical,
                attached: true,
            },
            BootDeviceStatus {
                kind: BootDevice::HardDrive,
                attached: false,
            },
        ];
        assert_eq_hex!(bios_boot_order(&boot_order), 0x76543021);
    }
}
