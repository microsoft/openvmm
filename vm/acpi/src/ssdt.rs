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

/// Parameters for adding a PCIe host bridge to the SSDT.
pub struct PcieHostBridgeEntry {
    /// Unique index of this host bridge.
    pub index: u32,
    /// PCIe segment number.
    pub segment: u16,
    /// Lowest valid bus number.
    pub start_bus: u8,
    /// Highest valid bus number.
    pub end_bus: u8,
    /// Memory range for ECAM configuration space access.
    pub ecam_range: MemoryRange,
    /// Memory range for low MMIO.
    pub low_mmio: MemoryRange,
    /// Memory range for high MMIO.
    pub high_mmio: MemoryRange,
    /// Whether this host bridge supports CXL.
    pub cxl: bool,
    /// NUMA proximity domain.
    pub vnode: Option<u32>,
    /// When true, emit a `_DSM` method instructing the OS to preserve
    /// firmware-assigned BAR values. Used for P2P DMA with GPA = HPA.
    pub preserve_bars: bool,
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
    pub fn add_pcie(&mut self, entry: PcieHostBridgeEntry) {
        let PcieHostBridgeEntry {
            index,
            segment,
            start_bus,
            end_bus,
            ecam_range,
            low_mmio,
            high_mmio,
            cxl,
            vnode,
            preserve_bars,
        } = entry;
        let mut pcie = Device::new(encode_pcie_name(index).as_slice());
        if cxl {
            // As recommended by the CXL specification, describe CXL host bridges with _HID "ACPI0016"
            // and both PNP0A03 and PNP0A08 in _CID to maximize compatibility with different OSes.
            pcie.add_object(&NamedString::new(b"_HID", b"ACPI0016"));
            let mut cid_data = Vec::new();
            cid_data.extend_from_slice(&EisaId(*b"PNP0A03").to_bytes());
            cid_data.extend_from_slice(&EisaId(*b"PNP0A08").to_bytes());
            pcie.add_object(&NamedObject::new(
                b"_CID",
                &StructuredPackage {
                    elem_count: 2,
                    elem_data: cid_data,
                },
            ));
        } else {
            pcie.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0A08")));
            pcie.add_object(&NamedObject::new(b"_CID", &EisaId(*b"PNP0A03")));
        }
        pcie.add_object(&NamedInteger::new(b"_UID", index.into()));
        pcie.add_object(&NamedInteger::new(b"_SEG", segment.into()));
        pcie.add_object(&NamedInteger::new(b"_BBN", start_bus.into()));
        pcie.add_object(&NamedInteger::new(b"_CCA", 1));
        if let Some(vnode) = vnode {
            pcie.add_object(&NamedInteger::new(b"_PXM", vnode.into()));
        }

        // _OSC method: negotiate native control with the guest OS.
        //
        // Per ACPI spec §6.2.11 (_OSC, Operating System Capabilities), the OS
        // calls _OSC to negotiate platform feature control.
        //
        // Supported UUIDs:
        // - PCIe: 33DB4D5B-1FF7-401C-9657-7441C03DD766
        //   (PCI Firmware Spec §4.5.1, Table 4-3)
        // - CXL:  68F2D50B-C469-4D8A-BD3D-941A103FD3FC (CXL host bridge mode)
        //
        // Status DWORD[0] bits used here:
        // - bit 1 (0x02): unrecognized revision (CXL path, Arg1 != 1)
        // - bit 2 (0x04): unrecognized UUID
        //   (ACPI spec §6.2.11.1, Table 6.15)
        //
        // Behavior:
        // - PCIe UUID: clear status (grant all requested control)
        // - CXL UUID: if Arg1 == 1 clear status; else set bit 1
        // - Any other UUID: set bit 2
        // - Return Arg3
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

        let else_body = if cxl {
            // CXL _OSC UUID: 68f2d50b-c469-4d8a-bd3d-941a103fd3fc
            // Rev 1 is currently supported; unsupported revisions set STS0 bit 1.
            let cxl_osc_uuid = guid::guid!("68f2d50b-c469-4d8a-bd3d-941a103fd3fc");
            let cxl_uuid_buffer = Buffer(cxl_osc_uuid.as_bytes()).to_bytes();
            let cxl_uuid_match = LEqualOp {
                left: encode_arg(0),
                right: cxl_uuid_buffer,
            };

            let cxl_revision_match = LEqualOp {
                left: encode_arg(1),
                right: encode_integer(1),
            };

            let cxl_store_zero = StoreOp {
                source: encode_integer(0),
                destination: b"STS0".to_vec(),
            };
            let cxl_rev_if_op = IfOp {
                predicate: cxl_revision_match.to_bytes(),
                body: cxl_store_zero.to_bytes(),
            };

            // STS0 bit 1: unrecognized revision.
            let cxl_revision_or = OrOp {
                operand1: b"STS0".to_vec(),
                operand2: encode_integer(0x02),
                target_name: b"STS0".to_vec(),
            };
            let cxl_revision_else = ElseOp {
                body: cxl_revision_or.to_bytes(),
            };

            // STS0 bit 2: unrecognized UUID.
            let uuid_or = OrOp {
                operand1: b"STS0".to_vec(),
                operand2: encode_integer(0x04),
                target_name: b"STS0".to_vec(),
            };
            let unknown_uuid_else = ElseOp {
                body: uuid_or.to_bytes(),
            };

            let cxl_if_op = IfOp {
                predicate: cxl_uuid_match.to_bytes(),
                body: {
                    let mut body = Vec::new();
                    cxl_rev_if_op.append_to_vec(&mut body);
                    cxl_revision_else.append_to_vec(&mut body);
                    body
                },
            };

            ElseOp {
                body: {
                    let mut body = Vec::new();
                    cxl_if_op.append_to_vec(&mut body);
                    unknown_uuid_else.append_to_vec(&mut body);
                    body
                },
            }
        } else {
            // STS0 bit 2: unrecognized UUID.
            let uuid_or = OrOp {
                operand1: b"STS0".to_vec(),
                operand2: encode_integer(0x04),
                target_name: b"STS0".to_vec(),
            };
            ElseOp {
                body: uuid_or.to_bytes(),
            }
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

        // _DSM: Device Specific Method for preserving firmware BAR assignments.
        //
        // UUID {E5C937D0-3553-4D7A-9117-EA4D19C3434D} is the PCI/PCIe host
        // bridge _DSM defined in the PCI Firmware Specification §4.6.
        //
        // Function 0: returns a buffer with supported function bitmask
        // Function 5: returns 0 to indicate firmware-assigned BAR values
        //             should be preserved by the OS
        //
        // When preserve_bars is false, no _DSM is emitted and the guest
        // OS is free to reprogram BARs.
        if preserve_bars {
            let mut dsm_method = Method::new(b"_DSM");
            dsm_method.set_arg_count(4);

            let dsm_uuid = guid::guid!("E5C937D0-3553-4D7A-9117-EA4D19C3434D");
            let dsm_uuid_buffer = Buffer(dsm_uuid.as_bytes()).to_bytes();

            // If (LEqual(Arg0, UUID))
            let uuid_match = LEqualOp {
                left: encode_arg(0),
                right: dsm_uuid_buffer,
            };

            // Function 0: return supported function bitmask (bits 0 and 5).
            // Bit 0 = Function 0 supported, Bit 5 = Function 5 supported.
            let fn0_match = LEqualOp {
                left: encode_arg(2),
                right: encode_integer(0),
            };
            let fn0_return = ReturnOp {
                result: Buffer(&[0x21u8]).to_bytes(),
            };
            let fn0_if = IfOp {
                predicate: fn0_match.to_bytes(),
                body: fn0_return.to_bytes(),
            };

            // Function 5: return 0 (preserve firmware BAR assignments).
            let fn5_match = LEqualOp {
                left: encode_arg(2),
                right: encode_integer(5),
            };
            let fn5_return = ReturnOp {
                result: encode_integer(0),
            };
            let fn5_if = IfOp {
                predicate: fn5_match.to_bytes(),
                body: fn5_return.to_bytes(),
            };

            let uuid_if = IfOp {
                predicate: uuid_match.to_bytes(),
                body: {
                    let mut body = Vec::new();
                    fn0_if.append_to_vec(&mut body);
                    fn5_if.append_to_vec(&mut body);
                    body
                },
            };

            dsm_method.add_operation(&uuid_if);

            // Unrecognized UUID or function: return empty buffer.
            dsm_method.add_operation(&ReturnOp {
                result: Buffer(&[] as &[u8]).to_bytes(),
            });

            pcie.add_object(&dsm_method);
        }

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

    fn test_pcie_entry(vnode: Option<u32>) -> PcieHostBridgeEntry {
        PcieHostBridgeEntry {
            index: 0,
            segment: 0,
            start_bus: 0,
            end_bus: 255,
            ecam_range: MemoryRange::new(0x1000_0000..0x2000_0000),
            low_mmio: MemoryRange::new(0xdc00_0000..0xe000_0000),
            high_mmio: MemoryRange::new(0x10_0000_0000..0x10_4000_0000),
            cxl: false,
            vnode,
            preserve_bars: false,
        }
    }

    fn contains_name(bytes: &[u8], name: &[u8; 4]) -> bool {
        bytes.windows(4).any(|w| w == name)
    }

    fn contains_bytes(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn pcie_includes_cca() {
        let mut ssdt = Ssdt::new();
        ssdt.add_pcie(test_pcie_entry(None));

        let bytes = ssdt.to_bytes();
        verify_header(&bytes);
        assert!(
            bytes
                .windows(6)
                .any(|window| window == [8, b'_', b'C', b'C', b'A', 1,])
        );
    }

    #[test]
    fn pcie_no_pxm_when_vnode_none() {
        let mut ssdt = Ssdt::new();
        ssdt.add_pcie(test_pcie_entry(None));

        let bytes = ssdt.to_bytes();
        verify_header(&bytes);
        // _PXM should NOT be present when vnode is None.
        assert!(
            !bytes
                .windows(4)
                .any(|window| window == [b'_', b'P', b'X', b'M']),
            "_PXM should not be emitted when vnode is None"
        );
    }

    #[test]
    fn pcie_includes_pxm() {
        let mut ssdt = Ssdt::new();
        ssdt.add_pcie(test_pcie_entry(Some(0)));

        let bytes = ssdt.to_bytes();
        verify_header(&bytes);
        // _PXM with value 0
        assert!(
            bytes
                .windows(6)
                .any(|window| window == [8, b'_', b'P', b'X', b'M', 0,])
        );
    }

    #[test]
    fn pcie_pxm_nonzero_node() {
        let mut ssdt = Ssdt::new();
        ssdt.add_pcie(test_pcie_entry(Some(3)));

        let bytes = ssdt.to_bytes();
        verify_header(&bytes);
        // _PXM with value 3: Name op (0x08) + "_PXM" + BytePrefix (0x0a) + 3
        assert!(
            bytes
                .windows(7)
                .any(|window| window == [8, b'_', b'P', b'X', b'M', 0x0a, 3,])
        );
    }

    #[test]
    fn pcie_dsm_present_when_preserve_bars() {
        let mut ssdt = Ssdt::new();
        ssdt.add_pcie(PcieHostBridgeEntry {
            preserve_bars: true,
            ..test_pcie_entry(None)
        });

        let bytes = ssdt.to_bytes();
        verify_header(&bytes);

        // _DSM method name must be present.
        assert!(contains_name(&bytes, b"_DSM"));

        // The PCI firmware _DSM UUID must appear in mixed-endian form
        // (GUID wire format).
        let uuid = guid::guid!("E5C937D0-3553-4D7A-9117-EA4D19C3434D");
        assert!(contains_bytes(&bytes, uuid.as_bytes()));

        // The supported-functions bitmask byte (0x21 = bits 0+5) must
        // appear somewhere in the buffer encoding.
        assert!(contains_bytes(&bytes, &[0x21]));
    }

    #[test]
    fn pcie_dsm_absent_without_preserve_bars() {
        let mut ssdt = Ssdt::new();
        ssdt.add_pcie(PcieHostBridgeEntry {
            preserve_bars: false,
            ..test_pcie_entry(None)
        });

        let bytes = ssdt.to_bytes();
        verify_header(&bytes);

        // _DSM must NOT be present.
        assert!(!contains_name(&bytes, b"_DSM"));

        // The PCI firmware _DSM UUID must NOT appear.
        let uuid = guid::guid!("E5C937D0-3553-4D7A-9117-EA4D19C3434D");
        assert!(!contains_bytes(&bytes, uuid.as_bytes()));
    }
}
