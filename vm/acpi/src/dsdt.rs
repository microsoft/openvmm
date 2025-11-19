// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub use crate::aml::*;
use memory_range::MemoryRange;
use x86defs::apic::APIC_BASE_ADDRESS;
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

pub struct PciRoutingTableEntry {
    pub address: u32,
    pub pin: u8,
    pub source: Option<Vec<u8>>,
    pub source_index: u32,
}

pub struct PciRoutingTable {
    entries: Vec<PciRoutingTableEntry>,
}

impl PciRoutingTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn add_entry(&mut self, entry: PciRoutingTableEntry) {
        self.entries.push(entry);
    }
}

impl AmlObject for PciRoutingTable {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        let mut table_data: Vec<u8> = Vec::with_capacity(self.entries.len() * 10);
        for entry in self.entries.iter() {
            let mut elem_data: Vec<u8> = Vec::with_capacity(
                9 + match &entry.source {
                    Some(name) => name.len(),
                    None => 1,
                },
            );
            elem_data.extend_from_slice(&encode_dword(entry.address));
            elem_data.push(entry.pin);
            match &entry.source {
                Some(name) => elem_data.extend_from_slice(name),
                None => elem_data.push(0),
            }
            elem_data.extend_from_slice(&encode_dword(entry.source_index));
            StructuredPackage {
                elem_count: 4,
                elem_data,
            }
            .append_to_vec(&mut table_data);
        }

        NamedObject::new(
            b"_PRT",
            &StructuredPackage {
                elem_count: self.entries.len() as u8,
                elem_data: table_data,
            },
        )
        .append_to_vec(byte_stream);
    }
}

pub struct Dsdt {
    description_header: DescriptionHeader,
    objects: Vec<u8>,
}

impl Dsdt {
    pub fn new() -> Self {
        Self {
            description_header: DescriptionHeader {
                signature: u32::from_le_bytes(*b"DSDT"),
                _length: 0,
                revision: 2,
                _checksum: 0,
                oem_id: *b"MSFTVM",
                oem_table_id: 0x313054445344, // b'DSDT01'
                oem_revision: 1,
                creator_id: u32::from_le_bytes(*b"MSFT"),
                creator_rev: 0x5000000,
            },
            objects: vec![],
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut byte_stream = Vec::new();
        byte_stream.extend_from_slice(self.description_header.as_bytes());
        byte_stream.extend_from_slice(&self.objects);

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

    /// Add an APIC device to the DSDT with the following ASL code:
    /// ```text
    /// Device(\_SB.APIC)
    /// {
    ///     Name(_HID, EISAID("PNP0003"))
    ///     Name(_CRS,
    ///         ResourceTemplate()
    ///         {
    ///             Memory32Fixed(ReadWrite, 0xfee00000, 0x1000)
    ///             Memory32Fixed(ReadWrite, 0xfec00000, 0x1000)
    ///         })
    /// }
    /// ```
    pub fn add_apic(&mut self) {
        let mut apic = Device::new(b"\\_SB.APIC");
        apic.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0003")));
        let mut apic_crs = CurrentResourceSettings::new();
        apic_crs.add_resource(&Memory32Fixed::new(APIC_BASE_ADDRESS, 0x1000, true));
        apic_crs.add_resource(&Memory32Fixed::new(0xfec00000, 0x1000, true));
        apic.add_object(&apic_crs);
        self.add_object(&apic);
    }

    /// Add a 16650A compatible UART to the DSDT with the following ASL code:
    /// ```text
    /// Device(<name>)
    /// {
    ///     Name(_HID, EISAID("PNP0501")) // 16550A-compatible COM port
    ///     Name(_DDN, <ddn>)
    ///     Name(_UID, <uid>)
    ///     Name(_CRS, ResourceTemplate()
    ///     {
    ///         IO(Decode16, <io_base>, <io_base>, 1, 8)
    ///         Interrupt(ResourceConsumer, Edge, ActiveHigh, Exclusive)
    ///             {<irq>}
    ///     })
    /// }
    /// ```
    pub fn add_uart(&mut self, name: &[u8], ddn: &[u8], uid: u64, io_base: u16, irq: u32) {
        let mut uart = Device::new(name);
        uart.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0501")));
        uart.add_object(&NamedString::new(b"_DDN", ddn));
        uart.add_object(&NamedInteger::new(b"_UID", uid));
        let mut uart_crs = CurrentResourceSettings::new();
        uart_crs.add_resource(&IoPort::new(io_base, io_base, 8));
        let mut intr = Interrupt::new(irq);
        intr.is_edge_triggered = true;
        uart_crs.add_resource(&intr);
        uart.add_object(&uart_crs);
        self.add_object(&uart);
    }

    /// Add an ACPI module device to describe the low and high MMIO regions.
    /// This is used when PCI is not present so that VMBus can find MMIO space.
    ///
    /// ```text
    /// Device(\_SB.VMOD)
    /// {
    ///     Name(_HID, "ACPI0004")
    ///     Name(_UID, 0)
    ///     Name(_CRS, ResourceTemplate()
    ///     {
    ///         // Low gap
    ///         QWORDMemory(ResourceProducer, PosDecode, MinFixed, MaxFixed, Cacheable, ReadWrite,
    ///         // Granularity Min          Max T       Translation Range (Length = Max-Min+1)
    ///             0,         <low_min>,   <low_max>,  0,          <dynamic>,,,
    ///             MEM6)   // Name declaration for this descriptor
    ///
    ///         // High gap
    ///         QWORDMemory(ResourceProducer, PosDecode, MinFixed, MaxFixed, Cacheable, ReadWrite,
    ///         // Granularity Min          Max T       Translation Range (Length = Max-Min+1)
    ///             0,         <high_min>,  <high_max>, 0,          <dynamic>,,,
    ///             MEM6)   // Name declaration for this descriptor
    ///     })
    /// }
    /// ```
    pub fn add_mmio_module(&mut self, low: MemoryRange, high: MemoryRange) {
        let mut vmod = Device::new(b"\\_SB.VMOD");
        vmod.add_object(&NamedString::new(b"_HID", b"ACPI0004"));
        vmod.add_object(&NamedInteger::new(b"_UID", 0));
        let mut vmod_crs = CurrentResourceSettings::new();
        vmod_crs.add_resource(&QwordMemory::new(low.start(), low.end() - low.start()));
        vmod_crs.add_resource(&QwordMemory::new(high.start(), high.end() - high.start()));
        vmod.add_object(&vmod_crs);
        self.add_object(&vmod);
    }

    /// Adds a PCI bus with the specified MMIO ranges.
    ///
    /// ```text
    /// Device(\_SB.PCI0)
    /// {
    ///     Name(_HID, PNP0A03)
    ///     Name(_CRS, ResourceTemplate()
    ///     {
    ///         WordBusNumber(...) // Bus translation info
    ///         IO(Decode16, 0xcf8, 0xcf8) // IO port
    ///         QWordMemory() // Low gap
    ///         QWordMemory() // High gap
    ///     })
    ///     // PCI routing table
    ///     Name(_PRT, Package{
    ///         Package{<address>, <PCI pin>, 0, <interrupt>},
    ///         ...
    ///     })
    /// }
    /// ```
    pub fn add_pci(
        &mut self,
        low: MemoryRange,
        high: MemoryRange,
        // array of ((device, function), line)
        legacy_interrupts: &[((u8, Option<u8>), u32)],
    ) {
        let mut pci0 = Device::new(b"\\_SB.PCI0");
        pci0.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0A03")));
        // FUTURE: when implementing PCIe, switch _HID over to "PNP0A08", and a
        // _CID for "PNP0A03"

        // OS negotiation for control of the bus. See https://uefi.org/specs/ACPI/6.4/06_Device_Configuration/Device_Configuration.html#osc-operating-system-capabilities
        // TODO: Lots of work needed for _OSC.
        let mut empty_os_method = Method::new(b"_OSC");
        empty_os_method.set_arg_count(4);
        empty_os_method.add_operation(&ReturnOp {
            result: Buffer(0x10u64.to_le_bytes()).to_bytes(),
        });
        pci0.add_object(&empty_os_method);
        let mut prt = PciRoutingTable::new();
        for &((device, function), line) in legacy_interrupts {
            prt.add_entry(PciRoutingTableEntry {
                address: ((device as u32) << 16) | function.map(|x| x as u32).unwrap_or(0xffff),
                pin: 0,
                source: None,
                source_index: line, // Interrupt line
            });
        }
        pci0.add_object(&prt);
        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&BusNumber::new(0, 1));
        crs.add_resource(&IoPort::new(0xcf8, 0xcf8, 8));
        crs.add_resource(&QwordMemory::new(low.start(), low.end() - low.start()));
        crs.add_resource(&QwordMemory::new(high.start(), high.end() - high.start()));
        pci0.add_object(&crs);
        self.add_object(&pci0);
    }

    /// Add a VMBUS device to the DSDT.
    ///
    /// If `in_pci`, then enumerate the device under PCI0. Otherwise, enumerate
    /// it under the VMOD module created by `add_mmio_module`.
    ///
    /// ```text
    /// Device(\_SB.VMOD.VMBS)
    /// {
    ///     Name(STA, 0xF)
    ///     Name(_ADR, 0x00)
    ///     Name(_DDN, "VMBUS")
    ///     Name(_HID, "VMBus")
    ///     Name(_UID, 0)
    ///     Method(_DIS, 0) { And(STA, 0xD, STA) }
    ///     Method(_PS0, 0) { Or(STA, 0xF, STA) }
    ///     Method(_STA, 0)
    ///     {
    ///         return(STA)
    ///     }
    ///
    ///     Name(_PS3, 0)
    /// }
    /// ```
    pub fn add_vmbus(&mut self, in_pci: bool) {
        let name = if in_pci {
            b"\\_SB.PCI0.VMBS"
        } else {
            b"\\_SB.VMOD.VMBS"
        };
        let mut vmbs = Device::new(name);
        vmbs.add_object(&NamedInteger::new(b"STA", 0xf));
        vmbs.add_object(&NamedInteger::new(b"_ADR", 0));
        vmbs.add_object(&NamedString::new(b"_DDN", b"VMBUS"));
        vmbs.add_object(&NamedString::new(b"_HID", b"VMBus"));
        vmbs.add_object(&NamedInteger::new(b"_UID", 0));
        let op = AndOp {
            operand1: b"STA_".to_vec(),
            operand2: encode_integer(13),
            target_name: b"STA_".to_vec(),
        };
        let mut method = Method::new(b"_DIS");
        method.add_operation(&op);
        vmbs.add_object(&method);
        let op = OrOp {
            operand1: b"STA_".to_vec(),
            operand2: encode_integer(15),
            target_name: b"STA_".to_vec(),
        };
        let mut method = Method::new(b"_PS0");
        method.add_operation(&op);
        vmbs.add_object(&method);
        let op = ReturnOp {
            result: b"STA_".to_vec(),
        };
        let mut method = Method::new(b"_STA");
        method.add_operation(&op);
        vmbs.add_object(&method);
        vmbs.add_object(&NamedInteger::new(b"_PS3", 0));
        // On linux, the vmbus driver will fail if the _CRS section is not present.
        vmbs.add_object(&CurrentResourceSettings::new());
        self.add_object(&vmbs);
    }

    /// Add an RTC device with the following ASL code:
    /// ```text
    /// Device(\_SB.RTC0)
    /// {
    ///     Name(_HID, EISAID("PNP0B00")) // AT real-time clock
    ///     Name(_UID, 0)
    ///     Name(_CRS, ResourceTemplate()
    ///     {
    ///         IO(Decode16, 0x70, 0x70, 0, 0x2)
    ///         Interrupt(ResourceConsumer, Edge, ActiveHigh, Exclusive) {8}
    ///     })
    /// }
    /// ```
    pub fn add_rtc(&mut self) {
        let mut rtc = Device::new(b"\\_SB.RTC0");
        rtc.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0B00")));
        rtc.add_object(&NamedInteger::new(b"_UID", 0));
        let mut rtc_crs = CurrentResourceSettings::new();
        let mut ioport = IoPort::new(0x70, 0x70, 2);
        ioport.alignment = 0;
        rtc_crs.add_resource(&ioport);
        let mut intr = Interrupt::new(8);
        intr.is_edge_triggered = true;
        rtc_crs.add_resource(&intr);
        rtc.add_object(&rtc_crs);
        self.add_object(&rtc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::test_helpers::verify_expected_bytes;

    pub fn verify_header(bytes: &[u8]) {
        assert!(bytes.len() >= 36);

        // signature
        assert_eq!(bytes[0], b'D');
        assert_eq!(bytes[1], b'S');
        assert_eq!(bytes[2], b'D');
        assert_eq!(bytes[3], b'T');

        // length
        let dsdt_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(dsdt_len as usize, bytes.len());

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
        assert_eq!(bytes[16], b'D');
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
        assert_eq!(creator_rev, 0x5000000);
    }

    #[test]
    fn verify_simple_table() {
        let mut dsdt = Dsdt::new();
        let nobj = NamedObject::new(b"_S0", &Package(vec![0, 0]));
        dsdt.add_object(&nobj);
        let bytes = dsdt.to_bytes();
        verify_header(&bytes);
        verify_expected_bytes(&bytes[36..], &[8, b'_', b'S', b'0', b'_', 0x12, 4, 2, 0, 0]);
    }

    #[test]
    fn verify_table() {
        let mut dsdt = Dsdt::new();
        dsdt.add_object(&NamedObject::new(b"\\_S0", &Package(vec![0, 0])));
        dsdt.add_object(&NamedObject::new(b"\\_S5", &Package(vec![0, 0])));

        let mut apic = Device::new(b"\\_SB.APIC");
        apic.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0003")));
        let mut apic_crs = CurrentResourceSettings::new();
        apic_crs.add_resource(&Memory32Fixed::new(0xfee00000, 0x1000, true));
        apic_crs.add_resource(&Memory32Fixed::new(0xfec00000, 0x1000, true));
        apic.add_object(&apic_crs);
        dsdt.add_object(&apic);

        let mut uart = Device::new(b"\\_SB.UAR1");
        uart.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0501")));
        uart.add_object(&NamedString::new(b"_DDN", b"COM1"));
        uart.add_object(&NamedInteger::new(b"_UID", 1));
        let mut uart_crs = CurrentResourceSettings::new();
        uart_crs.add_resource(&IoPort::new(0x3f8, 0x3f8, 8));
        let mut intr = Interrupt::new(4);
        intr.is_edge_triggered = true;
        uart_crs.add_resource(&intr);
        uart.add_object(&uart_crs);
        dsdt.add_object(&uart);

        let mut vmod = Device::new(b"\\_SB.VMOD");
        vmod.add_object(&NamedString::new(b"_HID", b"ACPI0004"));
        vmod.add_object(&NamedInteger::new(b"_UID", 0));
        let mut vmod_crs = CurrentResourceSettings::new();
        vmod_crs.add_resource(&QwordMemory::new(0x100000000, 0x100000000));
        vmod.add_object(&vmod_crs);
        dsdt.add_object(&vmod);

        let mut vmbs = Device::new(b"\\_SB.VMOD.VMBS");
        vmbs.add_object(&NamedInteger::new(b"STA", 0xf));
        vmbs.add_object(&NamedInteger::new(b"_ADR", 0));
        vmbs.add_object(&NamedString::new(b"_DDN", b"VMBUS"));
        vmbs.add_object(&NamedString::new(b"_HID", b"VMBus"));
        vmbs.add_object(&NamedInteger::new(b"_UID", 0));
        let op = AndOp {
            operand1: vec![b'S', b'T', b'A', b'_'],
            operand2: encode_integer(13),
            target_name: vec![b'S', b'T', b'A', b'_'],
        };
        let mut method = Method::new(b"_DIS");
        method.add_operation(&op);
        vmbs.add_object(&method);
        let op = OrOp {
            operand1: vec![b'S', b'T', b'A', b'_'],
            operand2: encode_integer(15),
            target_name: vec![b'S', b'T', b'A', b'_'],
        };
        let mut method = Method::new(b"_PS0");
        method.add_operation(&op);
        vmbs.add_object(&method);
        let op = ReturnOp {
            result: vec![b'S', b'T', b'A', b'_'],
        };
        let mut method = Method::new(b"_STA");
        method.add_operation(&op);
        vmbs.add_object(&method);
        vmbs.add_object(&NamedInteger::new(b"_PS3", 0));
        dsdt.add_object(&vmbs);

        let mut rtc = Device::new(b"\\_SB.RTC0");
        rtc.add_object(&NamedObject::new(b"_HID", &EisaId(*b"PNP0B00")));
        rtc.add_object(&NamedInteger::new(b"_UID", 0));
        let mut rtc_crs = CurrentResourceSettings::new();
        let mut ioport = IoPort::new(0x70, 0x70, 2);
        ioport.alignment = 0;
        rtc_crs.add_resource(&ioport);
        let mut intr = Interrupt::new(8);
        intr.is_edge_triggered = true;
        rtc_crs.add_resource(&intr);
        rtc.add_object(&rtc_crs);
        dsdt.add_object(&rtc);

        for proc_index in 1..3 {
            let mut proc = Device::new(format!("P{:03}", proc_index).as_bytes());
            proc.add_object(&NamedString::new(b"_HID", b"ACPI0007"));
            proc.add_object(&NamedInteger::new(b"_UID", proc_index as u64));
            let mut method = Method::new(b"_STA");
            method.add_operation(&ReturnOp {
                result: encode_integer(0xf),
            });
            proc.add_object(&method);
            dsdt.add_object(&proc);
        }

        let bytes = dsdt.to_bytes();
        verify_header(&bytes);
        verify_expected_bytes(
            &bytes[36..],
            &[
                0x08, 0x5C, 0x5F, 0x53, 0x30, 0x5F, 0x12, 0x04, 0x02, 0x00, 0x00, 0x08, 0x5C, 0x5F,
                0x53, 0x35, 0x5F, 0x12, 0x04, 0x02, 0x00, 0x00, 0x5B, 0x82, 0x38, 0x5C, 0x2E, 0x5F,
                0x53, 0x42, 0x5F, 0x41, 0x50, 0x49, 0x43, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0C, 0x41,
                0xD0, 0x00, 0x03, 0x08, 0x5F, 0x43, 0x52, 0x53, 0x11, 0x1D, 0x0A, 0x1A, 0x86, 0x09,
                0x00, 0x01, 0x00, 0x00, 0xE0, 0xFE, 0x00, 0x10, 0x00, 0x00, 0x86, 0x09, 0x00, 0x01,
                0x00, 0x00, 0xC0, 0xFE, 0x00, 0x10, 0x00, 0x00, 0x79, 0x00, 0x5B, 0x82, 0x43, 0x04,
                0x5C, 0x2E, 0x5F, 0x53, 0x42, 0x5F, 0x55, 0x41, 0x52, 0x31, 0x08, 0x5F, 0x48, 0x49,
                0x44, 0x0C, 0x41, 0xD0, 0x05, 0x01, 0x08, 0x5F, 0x44, 0x44, 0x4E, 0x0D, 0x43, 0x4F,
                0x4D, 0x31, 0x00, 0x08, 0x5F, 0x55, 0x49, 0x44, 0x01, 0x08, 0x5F, 0x43, 0x52, 0x53,
                0x11, 0x16, 0x0A, 0x13, 0x47, 0x01, 0xF8, 0x03, 0xF8, 0x03, 0x01, 0x08, 0x89, 0x06,
                0x00, 0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x79, 0x00, 0x5B, 0x82, 0x4A, 0x05, 0x5C,
                0x2E, 0x5F, 0x53, 0x42, 0x5F, 0x56, 0x4D, 0x4F, 0x44, 0x08, 0x5F, 0x48, 0x49, 0x44,
                0x0D, 0x41, 0x43, 0x50, 0x49, 0x30, 0x30, 0x30, 0x34, 0x00, 0x08, 0x5F, 0x55, 0x49,
                0x44, 0x00, 0x08, 0x5F, 0x43, 0x52, 0x53, 0x11, 0x33, 0x0A, 0x30, 0x8A, 0x2B, 0x00,
                0x00, 0x0C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x01, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
                0x00, 0x79, 0x00, 0x5B, 0x82, 0x42, 0x07, 0x5C, 0x2F, 0x03, 0x5F, 0x53, 0x42, 0x5F,
                0x56, 0x4D, 0x4F, 0x44, 0x56, 0x4D, 0x42, 0x53, 0x08, 0x53, 0x54, 0x41, 0x5F, 0x0A,
                0x0F, 0x08, 0x5F, 0x41, 0x44, 0x52, 0x00, 0x08, 0x5F, 0x44, 0x44, 0x4E, 0x0D, 0x56,
                0x4D, 0x42, 0x55, 0x53, 0x00, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0D, 0x56, 0x4D, 0x42,
                0x75, 0x73, 0x00, 0x08, 0x5F, 0x55, 0x49, 0x44, 0x00, 0x14, 0x11, 0x5F, 0x44, 0x49,
                0x53, 0x00, 0x7B, 0x53, 0x54, 0x41, 0x5F, 0x0A, 0x0D, 0x53, 0x54, 0x41, 0x5F, 0x14,
                0x11, 0x5F, 0x50, 0x53, 0x30, 0x00, 0x7D, 0x53, 0x54, 0x41, 0x5F, 0x0A, 0x0F, 0x53,
                0x54, 0x41, 0x5F, 0x14, 0x0B, 0x5F, 0x53, 0x54, 0x41, 0x00, 0xA4, 0x53, 0x54, 0x41,
                0x5F, 0x08, 0x5F, 0x50, 0x53, 0x33, 0x00, 0x5B, 0x82, 0x37, 0x5C, 0x2E, 0x5F, 0x53,
                0x42, 0x5F, 0x52, 0x54, 0x43, 0x30, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0C, 0x41, 0xD0,
                0x0B, 0x00, 0x08, 0x5F, 0x55, 0x49, 0x44, 0x00, 0x08, 0x5F, 0x43, 0x52, 0x53, 0x11,
                0x16, 0x0A, 0x13, 0x47, 0x01, 0x70, 0x00, 0x70, 0x00, 0x00, 0x02, 0x89, 0x06, 0x00,
                0x03, 0x01, 0x08, 0x00, 0x00, 0x00, 0x79, 0x00, 0x5B, 0x82, 0x24, 0x50, 0x30, 0x30,
                0x31, 0x08, 0x5F, 0x48, 0x49, 0x44, 0x0D, 0x41, 0x43, 0x50, 0x49, 0x30, 0x30, 0x30,
                0x37, 0x00, 0x08, 0x5F, 0x55, 0x49, 0x44, 0x01, 0x14, 0x09, 0x5F, 0x53, 0x54, 0x41,
                0x00, 0xA4, 0x0A, 0x0F, 0x5B, 0x82, 0x25, 0x50, 0x30, 0x30, 0x32, 0x08, 0x5F, 0x48,
                0x49, 0x44, 0x0D, 0x41, 0x43, 0x50, 0x49, 0x30, 0x30, 0x30, 0x37, 0x00, 0x08, 0x5F,
                0x55, 0x49, 0x44, 0x0A, 0x02, 0x14, 0x09, 0x5F, 0x53, 0x54, 0x41, 0x00, 0xA4, 0x0A,
                0x0F,
            ],
        );
    }
}
