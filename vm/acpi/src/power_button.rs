// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared ACPI power-button description for DSDT and SSDT generation.

use crate::aml::*;
use alloc::vec;

pub(crate) fn device(gsi: u32, mmio_base: u32) -> Device {
    let mut button = Device::new(b"PBTN");
    button.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0C0C")));
    button.add_object(&NamedInteger::new(b"_UID", 0));

    let mut event = Method::new(b"_EVT");
    event.set_arg_count(1);
    event.is_serialized = true;
    let mut handle_event = StoreOp {
        source: encode_integer(1),
        destination: encode_name(b"PEND"),
    }
    .to_bytes();
    NotifyOp {
        object: encode_name(b"PBTN"),
        value: encode_integer(0x80),
    }
    .append_to_vec(&mut handle_event);
    event.add_operation(&IfOp {
        predicate: encode_name(b"PEND"),
        body: handle_event,
    });

    let interrupt = Interrupt::new(gsi);
    let mut crs = CurrentResourceSettings::new();
    crs.add_resource(&interrupt);

    // GED _CRS must contain only interrupts; reserve the register separately.
    let mut mmio_crs = CurrentResourceSettings::new();
    mmio_crs.add_resource(&Memory32Fixed::new(mmio_base, 4, true));
    let mut mmio = Device::new(b"MRES");
    mmio.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0C02")));
    mmio.add_object(&mmio_crs);

    // OperationRegion (PREG, SystemMemory, mmio_base, 4)
    let mut region = vec![0x5b, 0x80];
    region.extend_from_slice(&encode_name(b"PREG"));
    region.push(0);
    region.extend_from_slice(&encode_integer(mmio_base.into()));
    region.extend_from_slice(&encode_integer(4));

    let mut ged = Device::new(b"\\_SB.OVPB");
    ged.add_object(&NamedString::new(b"_HID", b"ACPI0013"));
    ged.add_object(&NamedInteger::new(b"_UID", 0));
    ged.add_object(&crs);
    ged.add_object(&mmio);
    ged.add_object(&GenericObject(region));
    // Field (PREG, DWordAcc, NoLock, Preserve) { PEND, 32 }
    ged.add_object(&GenericObject([
        0x5b, 0x81, 0x0b, b'P', b'R', b'E', b'G', 3, b'P', b'E', b'N', b'D', 32,
    ]));
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
        for (gsi, mmio_base) in [(10u32, 0xfed8_0000u32), (39, 0xeffe_9000)] {
            let mut expected = vec![
                0x5b, 0x82, 0x49, 0x0a, b'\\', 0x2e, b'_', b'S', b'B', b'_', b'O', b'V', b'P',
                b'B', 0x08, b'_', b'H', b'I', b'D', 0x0d, b'A', b'C', b'P', b'I', b'0', b'0', b'1',
                b'3', 0, 0x08, b'_', b'U', b'I', b'D', 0, 0x08, b'_', b'C', b'R', b'S', 0x11, 0x0e,
                0x0a, 0x0b, 0x89, 6, 0, 1, 1,
            ];
            expected.extend_from_slice(&gsi.to_le_bytes());
            expected.extend_from_slice(&[
                0x79, 0, 0x5b, 0x82, 0x26, b'M', b'R', b'E', b'S', 0x08, b'_', b'H', b'I', b'D',
                0x0c, 0x41, 0xd0, 0x0c, 2, 0x08, b'_', b'C', b'R', b'S', 0x11, 0x11, 0x0a, 0x0e,
                0x86, 9, 0, 1,
            ]);
            expected.extend_from_slice(&mmio_base.to_le_bytes());
            expected.extend_from_slice(&[
                4, 0, 0, 0, 0x79, 0, 0x5b, 0x80, b'P', b'R', b'E', b'G', 0, 0x0c,
            ]);
            expected.extend_from_slice(&mmio_base.to_le_bytes());
            expected.extend_from_slice(&[
                0x0a, 4, 0x5b, 0x81, 0x0b, b'P', b'R', b'E', b'G', 3, b'P', b'E', b'N', b'D', 32,
                0x5b, 0x82, 0x15, b'P', b'B', b'T', b'N', 0x08, b'_', b'H', b'I', b'D', 0x0c, 0x41,
                0xd0, 0x0c, 0x0c, 0x08, b'_', b'U', b'I', b'D', 0, 0x14, 0x19, b'_', b'E', b'V',
                b'T', 9, 0xa0, 0x12, b'P', b'E', b'N', b'D', 0x70, 1, b'P', b'E', b'N', b'D', 0x86,
                b'P', b'B', b'T', b'N', 0x0a, 0x80,
            ]);
            let mut dsdt = Dsdt::new();
            dsdt.add_power_button(gsi, mmio_base);
            let mut ssdt = Ssdt::new();
            ssdt.add_power_button(gsi, mmio_base);
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
