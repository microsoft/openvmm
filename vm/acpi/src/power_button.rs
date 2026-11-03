// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared ACPI power-button description for DSDT and SSDT generation.

use crate::aml::*;

pub(crate) fn device(gsi: u32) -> Device {
    let mut button = Device::new(b"PBTN");
    button.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0C0C")));
    button.add_object(&NamedInteger::new(b"_UID", 0));

    let mut event = Method::new(b"_EVT");
    event.set_arg_count(1);
    event.add_operation(&NotifyOp {
        object: encode_name(b"PBTN"),
        value: encode_integer(0x80),
    });

    let mut interrupt = Interrupt::new(gsi);
    interrupt.is_edge_triggered = true;
    let mut crs = CurrentResourceSettings::new();
    crs.add_resource(&interrupt);

    let mut ged = Device::new(b"\\_SB.OVPB");
    ged.add_object(&NamedString::new(b"_HID", b"ACPI0013"));
    ged.add_object(&NamedInteger::new(b"_UID", 0));
    ged.add_object(&crs);
    ged.add_object(&button);
    ged.add_object(&event);
    ged
}

#[cfg(test)]
mod tests {
    use crate::aml::test_helpers::verify_expected_bytes;
    use crate::dsdt::Dsdt;
    use crate::ssdt::Ssdt;
    use alloc::vec;
    use test_with_tracing::test;

    #[test]
    fn verify_power_button_tables() {
        for gsi in [10u32, 39] {
            let mut expected = vec![
                0x5b, 0x82, 0x4a, 0x05, b'\\', 0x2e, b'_', b'S', b'B', b'_', b'O', b'V', b'P',
                b'B', 0x08, b'_', b'H', b'I', b'D', 0x0d, b'A', b'C', b'P', b'I', b'0', b'0', b'1',
                b'3', 0, 0x08, b'_', b'U', b'I', b'D', 0, 0x08, b'_', b'C', b'R', b'S', 0x11, 0x0e,
                0x0a, 0x0b, 0x89, 6, 0, 3, 1,
            ];
            expected.extend_from_slice(&gsi.to_le_bytes());
            expected.extend_from_slice(&[
                0x79, 0, 0x5b, 0x82, 0x15, b'P', b'B', b'T', b'N', 0x08, b'_', b'H', b'I', b'D',
                0x0c, 0x41, 0xd0, 0x0c, 0x0c, 0x08, b'_', b'U', b'I', b'D', 0, 0x14, 0x0d, b'_',
                b'E', b'V', b'T', 1, 0x86, b'P', b'B', b'T', b'N', 0x0a, 0x80,
            ]);

            let mut dsdt = Dsdt::new();
            dsdt.add_power_button(gsi);
            let mut ssdt = Ssdt::new();
            ssdt.add_power_button(gsi);
            for table in [dsdt.to_bytes(), ssdt.to_bytes()] {
                verify_expected_bytes(&table[36..], &expected);
                assert_eq!(
                    table.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
                    0
                );
                assert_eq!(
                    u32::from_le_bytes(table[4..8].try_into().unwrap()) as usize,
                    table.len()
                );
            }
        }
    }
}
