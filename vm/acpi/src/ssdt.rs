// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub use crate::aml::*;
use memory_range::MemoryRange;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[repr(C, packed)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct DescriptionHeader {
    pub signature: u32,
    _length: u32, // placeholder, filled in during serialization to bytes
    pub revision: u8,
    _checksum: u8, // placeholder, filled in during serialization to bytes
    pub oem_id: [u8; 6],
    pub oem_table_id: u64,
    pub oem_revision: u32,
    pub creator_id: u32,
    pub creator_rev: u32,
}

fn encode_pcie_name(mut pcie_index: u32) -> Vec<u8> {
    assert!(pcie_index < 1000);
    let mut temp = "PCI0".as_bytes().to_vec();
    let mut i = temp.len() - 1;
    while pcie_index > 0 {
        temp[i] = b'0' + (pcie_index % 10) as u8;
        pcie_index /= 10;
        i -= 1;
    }
    temp
}

pub struct Ssdt {
    description_header: DescriptionHeader,
    objects: Vec<u8>,
    pcie_ecam_ranges: Vec<MemoryRange>,
}

impl Ssdt {
    pub fn new() -> Self {
        Self {
            description_header: DescriptionHeader {
                signature: u32::from_le_bytes(*b"SSDT"),
                _length: 0,
                revision: 2,
                _checksum: 0,
                oem_id: *b"MSFTVM",
                oem_table_id: 0x313054445353, // b'SSDT01'
                oem_revision: 1,
                creator_id: u32::from_le_bytes(*b"MSFT"),
                creator_rev: 0x01000000,
            },
            objects: vec![],
            pcie_ecam_ranges: vec![],
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut byte_stream = Vec::new();
        byte_stream.extend_from_slice(self.description_header.as_bytes());
        byte_stream.extend_from_slice(&self.objects);

        // N.B. Certain guest OSes will only probe ECAM ranges if they are
        // reserved in the resources of an ACPI motherboard device.
        if !self.pcie_ecam_ranges.is_empty() {
            let mut vmod = Device::new(b"VMOD");
            vmod.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0C02")));

            let mut crs = CurrentResourceSettings::new();
            for ecam_range in &self.pcie_ecam_ranges {
                crs.add_resource(&QwordMemory::new(
                    ecam_range.start(),
                    ecam_range.end() - ecam_range.start(),
                ));
            }
            vmod.add_object(&crs);
            vmod.append_to_vec(&mut byte_stream);
        }

        let length = byte_stream.len();
        byte_stream[4..8].copy_from_slice(&u32::try_from(length).unwrap().to_le_bytes());
        let mut checksum: u8 = 0;
        for byte in &byte_stream {
            checksum = checksum.wrapping_add(*byte);
        }

        byte_stream[9] = (!checksum).wrapping_add(1);
        byte_stream
    }

    pub fn add_object(&mut self, obj: &impl AmlObject) {
        obj.append_to_vec(&mut self.objects);
    }

    /// Adds a PCI Express root complex with the specified bus number and MMIO ranges.
    ///
    /// ```text
    /// Device(\_SB.PCI<N>)
    /// {
    ///     Name(_HID, PNP0A08)
    ///     Name(_CID, PNP0A03)
    ///     Name(_UID, <index>)
    ///     Name(_SEG, <segment>)
    ///     Name(_BBN, <bus number>)
    ///     Name(_CCA, 1)
    ///     Name(_CRS, ResourceTemplate()
    ///     {
    ///         WordBusNumber(...) // Bus number range
    ///         QWordMemory() // Low MMIO
    ///         QWordMemory() // High MMIO
    ///     })
    /// }
    /// ```
    pub fn add_pcie(
        &mut self,
        index: u32,
        segment: u16,
        start_bus: u8,
        end_bus: u8,
        ecam_range: MemoryRange,
        low_mmio: MemoryRange,
        high_mmio: MemoryRange,
    ) {
        let mut pcie = Device::new(encode_pcie_name(index).as_slice());
        pcie.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0A08")));
        pcie.add_object(&NamedObject::new(b"_CID", &EisaId(*b"PNP0A03")));
        pcie.add_object(&NamedInteger::new(b"_UID", index.into()));
        pcie.add_object(&NamedInteger::new(b"_SEG", segment.into()));
        pcie.add_object(&NamedInteger::new(b"_BBN", start_bus.into()));
        pcie.add_object(&NamedInteger::new(b"_CCA", 1));

        // _OSC method: grant native PCIe control to the OS.
        //
        // Per ACPI spec §6.2.11 (_OSC, Operating System Capabilities), the OS
        // calls _OSC to negotiate platform feature control. For PCIe root
        // complexes, the UUID and capability definitions are specified in the
        // PCI Firmware Specification §4.5.1 (_OSC Implementation for PCI Host
        // Bridge Devices).
        //
        // UUID: 33DB4D5B-1FF7-401C-9657-7441C03DD766
        //   (PCI Firmware Spec §4.5.1, Table 4-3)
        //
        // Status DWORD[0] bit 2: "Unrecognized UUID"
        //   (ACPI spec §6.2.11.1, Table 6.15)
        //
        // Method(_OSC, 4) {
        //   CreateDWordField(Arg3, 0, STS0)
        //   If (LEqual(Arg0, ToUUID("33DB4D5B-1FF7-401C-9657-7441C03DD766"))) {
        //     Store(0, STS0)  // clear status — grant everything
        //   } Else {
        //     Or(STS0, 0x04, STS0)  // unrecognized UUID
        //   }
        //   Return(Arg3)
        // }
        let mut osc_method = Method::new(b"_OSC");
        osc_method.set_arg_count(4);

        // CreateDWordField(Arg3, 0, STS0)
        osc_method.add_operation(&CreateDWordFieldOp {
            source_buffer: encode_arg(3),
            byte_index: encode_integer(0),
            field_name: *b"STS0",
        });

        // If (LEqual(Arg0, ToUUID("33DB4D5B-1FF7-401C-9657-7441C03DD766")))
        let pcie_osc_uuid = guid::guid!("33DB4D5B-1FF7-401C-9657-7441C03DD766");
        let uuid_buffer = Buffer(pcie_osc_uuid.as_bytes()).to_bytes();
        let lequal = LEqualOp {
            left: encode_arg(0),
            right: uuid_buffer,
        };

        // Else { Or(STS0, 0x04, STS0) }
        let or_op = OrOp {
            operand1: b"STS0".to_vec(),
            operand2: encode_integer(0x04),
            target_name: b"STS0".to_vec(),
        };
        let else_body = ElseOp {
            body: or_op.to_bytes(),
        };

        // If block: UUID matches — clear status and grant everything
        let store_zero = StoreOp {
            source: encode_integer(0),
            destination: b"STS0".to_vec(),
        };
        let if_op = IfOp {
            predicate: lequal.to_bytes(),
            body: store_zero.to_bytes(),
        };
        osc_method.add_operation(&if_op);
        osc_method.add_operation(&else_body);

        // Return(Arg3)
        osc_method.add_operation(&ReturnOp {
            result: encode_arg(3),
        });

        pcie.add_object(&osc_method);

        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&BusNumber::new(
            start_bus.into(),
            (end_bus as u16) - (start_bus as u16) + 1,
        ));
        crs.add_resource(&QwordMemory::new(
            low_mmio.start(),
            low_mmio.end() - low_mmio.start(),
        ));
        crs.add_resource(&QwordMemory::new(
            high_mmio.start(),
            high_mmio.end() - high_mmio.start(),
        ));
        pcie.add_object(&crs);

        self.add_object(&pcie);
        self.pcie_ecam_ranges.push(ecam_range);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::test_helpers::verify_expected_bytes;

    pub fn verify_header(bytes: &[u8]) {
        assert!(bytes.len() >= 36);

        // signature
        assert_eq!(bytes[0], b'S');
        assert_eq!(bytes[1], b'S');
        assert_eq!(bytes[2], b'D');
        assert_eq!(bytes[3], b'T');

        // length
        let ssdt_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(ssdt_len as usize, bytes.len());

        // revision
        assert_eq!(bytes[8], 2);

        // Validate checksum bytes[9] by verifying content adds to zero.
        let mut checksum: u8 = 0;
        for byte in bytes.iter() {
            checksum = checksum.wrapping_add(*byte);
        }
        assert_eq!(checksum, 0);

        // oem_id
        assert_eq!(bytes[10], b'M');
        assert_eq!(bytes[11], b'S');
        assert_eq!(bytes[12], b'F');
        assert_eq!(bytes[13], b'T');
        assert_eq!(bytes[14], b'V');
        assert_eq!(bytes[15], b'M');

        // oem_table_id
        assert_eq!(bytes[16], b'S');
        assert_eq!(bytes[17], b'S');
        assert_eq!(bytes[18], b'D');
        assert_eq!(bytes[19], b'T');
        assert_eq!(bytes[20], b'0');
        assert_eq!(bytes[21], b'1');
        assert_eq!(bytes[22], 0);
        assert_eq!(bytes[23], 0);

        // oem_revision
        let oem_revision = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        assert_eq!(oem_revision, 1);

        // creator_id
        assert_eq!(bytes[28], b'M');
        assert_eq!(bytes[29], b'S');
        assert_eq!(bytes[30], b'F');
        assert_eq!(bytes[31], b'T');

        // creator_rev
        let creator_rev = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
        assert_eq!(creator_rev, 0x01000000);
    }

    #[test]
    pub fn verify_pcie_name_encoding() {
        assert_eq!(encode_pcie_name(0), b"PCI0".to_vec());
        assert_eq!(encode_pcie_name(1), b"PCI1".to_vec());
        assert_eq!(encode_pcie_name(2), b"PCI2".to_vec());
        assert_eq!(encode_pcie_name(54), b"PC54".to_vec());
        assert_eq!(encode_pcie_name(294), b"P294".to_vec());
    }

    #[test]
    fn verify_simple_table() {
        let mut ssdt = Ssdt::new();
        let nobj = NamedObject::new(b"_S0", &Package(vec![0, 0]));
        ssdt.add_object(&nobj);
        let bytes = ssdt.to_bytes();
        verify_header(&bytes);
        verify_expected_bytes(&bytes[36..], &[8, b'_', b'S', b'0', b'_', 0x12, 4, 2, 0, 0]);
    }

    #[test]
    fn pcie_includes_cca() {
        let mut ssdt = Ssdt::new();
        ssdt.add_pcie(
            0,
            0,
            0,
            255,
            MemoryRange::new(0x1000_0000..0x2000_0000),
            MemoryRange::new(0xdc00_0000..0xe000_0000),
            MemoryRange::new(0x10_0000_0000..0x10_4000_0000),
        );

        let bytes = ssdt.to_bytes();
        verify_header(&bytes);
        assert!(
            bytes
                .windows(6)
                .any(|window| window == [8, b'_', b'C', b'C', b'A', 1,])
        );
    }
}
