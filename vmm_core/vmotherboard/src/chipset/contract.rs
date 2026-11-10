// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The contracts of chipset devices' state units: what the chipset derives
//! from a device's registrations and configuration space, which a snapshot
//! records and a restore checks.

use chipset_device::io::IoResult;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::PciConfigSpace;
use state_unit::UnitDescriptor;
use std::ops::RangeInclusive;

/// The declared part of a dynamically added device's contract, beyond what
/// the chipset reads from the device itself.
#[derive(Clone, Debug, Default)]
pub struct DynamicDeviceContract {
    /// The guest-visible identity of what the device occupies, such as its
    /// PCIe port. Defaults to the device's name.
    pub stable_id: Option<String>,
    /// What the device is, such as its resource kind.
    pub kind: String,
}

/// The prefix of the stable identity of a PCIe device's state unit.
const PCIE_PORT_STABLE_ID_PREFIX: &str = "pcie-port:";

/// Returns the stable identity that the state unit of the PCIe device on the
/// downstream port `port_name` records, whether the VM's configuration placed
/// the device there or it was hot-plugged.
pub fn pcie_port_stable_id(port_name: &str) -> String {
    format!("{PCIE_PORT_STABLE_ID_PREFIX}{port_name}")
}

/// Returns the downstream port that a [`pcie_port_stable_id`] names, or
/// `None` for another stable identity.
pub fn pcie_port_of_stable_id(stable_id: &str) -> Option<&str> {
    stable_id.strip_prefix(PCIE_PORT_STABLE_ID_PREFIX)
}

/// Returns the descriptor of the static I/O region `label`, at `range` of the
/// address space `space`.
pub(crate) fn region_descriptor<T: Copy + Into<u64>>(
    space: &str,
    label: &str,
    range: &RangeInclusive<T>,
) -> UnitDescriptor {
    let start: u64 = (*range.start()).into();
    let end: u64 = (*range.end()).into();
    UnitDescriptor::new(space, format!("{label}@{start:#x}+{:#x}", end - start + 1))
}

/// Returns the descriptors of the identity that a PCI function reports in its
/// configuration space header: its vendor and device IDs, its class code and
/// revision, and, for a type 0 header, its subsystem IDs. Returns none for a
/// function that does not answer synchronously.
pub(crate) fn pci_identity(device: &mut (impl PciConfigSpace + ?Sized)) -> Vec<UnitDescriptor> {
    let mut read = |offset| {
        let mut value = 0;
        match device.pci_cfg_read(
            offset,
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
        ) {
            IoResult::Ok => Some(value),
            IoResult::Err(_) | IoResult::Defer(_) => None,
        }
    };
    let Some(id) = read(0) else {
        return Vec::new();
    };
    let (vendor, device_id) = (id & 0xffff, id >> 16);
    if vendor == 0xffff {
        return Vec::new();
    }
    let mut descriptors = vec![UnitDescriptor::new(
        "pci.id",
        format!("{vendor:04x}:{device_id:04x}"),
    )];
    if let Some(class) = read(0x8) {
        descriptors.push(UnitDescriptor::new(
            "pci.class",
            format!("{:06x}", class >> 8),
        ));
        descriptors.push(UnitDescriptor::new(
            "pci.revision",
            format!("{:02x}", class & 0xff),
        ));
    }
    let header_type = read(0xc).map(|value| (value >> 16) & 0x7f);
    if header_type == Some(0) {
        if let Some(subsystem) = read(0x2c) {
            descriptors.push(UnitDescriptor::new(
                "pci.subsystem",
                format!("{:04x}:{:04x}", subsystem & 0xffff, subsystem >> 16),
            ));
        }
    }
    descriptors
}

#[cfg(test)]
mod tests {
    use super::pci_identity;
    use super::region_descriptor;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigSpace;
    use state_unit::UnitDescriptor;
    use test_with_tracing::test;

    /// A PCI function whose configuration space reads `dwords`, at their
    /// offsets, and all ones elsewhere.
    struct Header(&'static [(u16, u32)]);

    impl ChipsetDevice for Header {}

    impl PciConfigSpace for Header {
        fn pci_cfg_read(&mut self, offset: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
            let dword = self
                .0
                .iter()
                .find_map(|&(at, dword)| (at == offset).then_some(dword))
                .unwrap_or(!0);
            value.set(dword);
            IoResult::Ok
        }

        fn pci_cfg_write(&mut self, _: u16, _: ByteEnabledDwordWrite) -> IoResult {
            IoResult::Ok
        }
    }

    #[test]
    fn pci_identity_reads_the_header() {
        let mut endpoint = Header(&[
            (0x0, 0x1044_1af4),
            (0x8, 0x00ff_0001),
            (0xc, 0x0000_0000),
            (0x2c, 0x0004_1af4),
        ]);
        assert_eq!(
            pci_identity(&mut endpoint),
            [
                UnitDescriptor::new("pci.id", "1af4:1044"),
                UnitDescriptor::new("pci.class", "00ff00"),
                UnitDescriptor::new("pci.revision", "01"),
                UnitDescriptor::new("pci.subsystem", "1af4:0004"),
            ]
        );

        // A bridge's type 1 header has no subsystem IDs at 0x2c.
        let mut bridge = Header(&[(0x0, 0xc030_1414), (0x8, 0x0604_0000), (0xc, 0x0001_0000)]);
        assert_eq!(
            pci_identity(&mut bridge),
            [
                UnitDescriptor::new("pci.id", "1414:c030"),
                UnitDescriptor::new("pci.class", "060400"),
                UnitDescriptor::new("pci.revision", "00"),
            ]
        );

        // An absent function reads all ones.
        assert!(pci_identity(&mut Header(&[])).is_empty());
    }

    #[test]
    fn region_descriptor_records_base_and_length() {
        assert_eq!(
            region_descriptor("pio", "pit", &(0x40u16..=0x43)),
            UnitDescriptor::new("pio", "pit@0x40+0x4")
        );
        assert_eq!(
            region_descriptor("mmio", "ioapic", &(0xfec0_0000u64..=0xfec0_0fff)),
            UnitDescriptor::new("mmio", "ioapic@0xfec00000+0x1000")
        );
    }

    #[test]
    fn pcie_port_stable_ids_round_trip() {
        let stable_id = super::pcie_port_stable_id("s0rc0rp0");
        assert_eq!(stable_id, "pcie-port:s0rc0rp0");
        assert_eq!(super::pcie_port_of_stable_id(&stable_id), Some("s0rc0rp0"));
        assert_eq!(super::pcie_port_of_stable_id("vpci:1234"), None);
    }
}
