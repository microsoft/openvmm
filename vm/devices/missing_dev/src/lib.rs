// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exports [`MissingDev`]: a "no-op" device that silently swallows all MMIO,
//! PIO, and PCI accesses that come its way.
//!
//! Useful for claiming known-unimplemented machine resources, in order to cut
//! down on missing-device logging.

#![forbid(unsafe_code)]

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::spec::hwid::ProgrammingInterface;
use std::ops::RangeInclusive;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;

#[derive(Default, Inspect)]
struct MissingDevPciMetadata {
    #[inspect(debug)]
    bdf: (u8, u8, u8),
    #[inspect(hex)]
    vendor_id: u16,
    #[inspect(hex)]
    device_id: u16,
}

/// A device that swallows all MMIO, PIO, and PCI accesses that come its way.
#[derive(Default, InspectMut)]
pub struct MissingDev {
    #[inspect(with = "inspect_helpers::io_ranges")]
    pio: Vec<(Box<str>, RangeInclusive<u16>)>,
    #[inspect(with = "inspect_helpers::io_ranges")]
    mmio: Vec<(Box<str>, RangeInclusive<u64>)>,
    pci: Option<MissingDevPciMetadata>,

    #[inspect(skip)]
    _mmio_control: Box<[Box<dyn ControlMmioIntercept>]>,
    #[inspect(skip)]
    _pio_control: Box<[Box<dyn ControlPortIoIntercept>]>,
}

impl MissingDev {
    /// Convert a [`MissingDevManifest`] into a fully instantiated
    /// [`MissingDev`].
    ///
    /// Each claimed range is registered and mapped as an I/O region of its
    /// length, under its name. An empty range claims nothing, and a range that
    /// spans its whole address space, whose length does not fit the address
    /// type, is registered as two halves, both under its name.
    pub fn from_manifest(
        manifest: MissingDevManifest,
        register_mmio: &mut dyn RegisterMmioIntercept,
        register_pio: &mut dyn RegisterPortIoIntercept,
    ) -> MissingDev {
        let MissingDevManifest { pio, mmio, pci } = manifest;

        let mut pio_control = Vec::new();
        for (name, range) in &pio {
            for (start, len) in regions(range, u16::MAX) {
                // A region lies in the port space and is shorter than it, so
                // its start and its length fit in a port number.
                let mut control = register_pio.new_io_region(name, len as u16);
                control.map(start as u16);
                pio_control.push(control);
            }
        }

        let mut mmio_control = Vec::new();
        for (name, range) in &mmio {
            for (start, len) in regions(range, u64::MAX) {
                let mut control = register_mmio.new_io_region(name, len);
                control.map(start);
                mmio_control.push(control);
            }
        }

        MissingDev {
            pio,
            mmio,
            pci,

            _pio_control: pio_control.into(),
            _mmio_control: mmio_control.into(),
        }
    }
}

/// The I/O regions, as `(start, len)` pairs, that claim exactly the addresses
/// of `range` in an address space whose last address is `max`.
///
/// A region's length has the address type, so it is at most `max`: no region
/// claims an empty range, and two halves claim a range that spans the whole
/// address space, whose length is `max + 1`.
fn regions<T>(range: &RangeInclusive<T>, max: T) -> Vec<(u64, u64)>
where
    T: Copy + PartialOrd + Into<u64>,
{
    if range.is_empty() {
        return Vec::new();
    }
    let start: u64 = (*range.start()).into();
    let end: u64 = (*range.end()).into();
    if end - start < max.into() {
        vec![(start, end - start + 1)]
    } else {
        let mid = start + (end - start) / 2;
        vec![(start, mid - start + 1), (mid + 1, end - mid)]
    }
}

mod inspect_helpers {
    use super::*;

    pub(crate) fn io_ranges<T>(ranges: &[(Box<str>, RangeInclusive<T>)]) -> impl Inspect + '_
    where
        T: std::fmt::LowerHex,
    {
        inspect::adhoc(|req| {
            let mut res = req.respond();
            for (label, range) in ranges.iter() {
                let width = size_of::<T>() * 2 + 2;
                res.field(
                    &format!("{:#0width$x}-{:#0width$x}", range.start(), range.end()),
                    label,
                );
            }
        })
    }
}

impl ChangeDeviceState for MissingDev {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {}
}

impl ChipsetDevice for MissingDev {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        // this is technically IDET abuse...
        if self.pci.is_some() { Some(self) } else { None }
    }
}

impl SaveRestore for MissingDev {
    // This device should be constructed with `omit_saved_state`.
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}

impl MmioIntercept for MissingDev {
    fn mmio_read(&mut self, _addr: u64, data: &mut [u8]) -> IoResult {
        data.fill(!0);
        IoResult::Ok
    }

    fn mmio_write(&mut self, _addr: u64, _data: &[u8]) -> IoResult {
        IoResult::Ok
    }
}

impl PortIoIntercept for MissingDev {
    fn io_read(&mut self, _addr: u16, data: &mut [u8]) -> IoResult {
        data.fill(!0);
        IoResult::Ok
    }

    fn io_write(&mut self, _addr: u16, _data: &[u8]) -> IoResult {
        IoResult::Ok
    }
}

impl PciConfigSpace for MissingDev {
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        let pci = self.pci.as_ref().unwrap();

        pci_core::cfg_space_emu::ConfigSpaceType0Emulator::new(
            pci_core::spec::hwid::HardwareIds {
                vendor_id: pci.vendor_id,
                device_id: pci.device_id,
                revision_id: 0,
                prog_if: ProgrammingInterface::NONE,
                sub_class: pci_core::spec::hwid::Subclass::NONE,
                base_class: pci_core::spec::hwid::ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![],
            vec![],
            pci_core::cfg_space_emu::DeviceBars::new(),
        )
        .read_byte_enabled(offset, value)
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: ByteEnabledDwordWrite) -> IoResult {
        IoResult::Ok
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        let pci = self.pci.as_ref().unwrap();
        Some(pci.bdf)
    }
}

/// A [`MissingDev`] builder.
#[derive(Default)]
pub struct MissingDevManifest {
    pio: Vec<(Box<str>, RangeInclusive<u16>)>,
    mmio: Vec<(Box<str>, RangeInclusive<u64>)>,
    pci: Option<MissingDevPciMetadata>,
}

impl MissingDevManifest {
    /// Create a new [`MissingDevManifest`].
    pub fn new() -> Self {
        Default::default()
    }

    /// Claim the specified Port IO region.
    pub fn claim_pio(mut self, region_name: &str, range: RangeInclusive<u16>) -> Self {
        self.pio.push((region_name.into(), range));
        self
    }

    /// Claim the specified MMIO region.
    pub fn claim_mmio(mut self, region_name: &str, range: RangeInclusive<u64>) -> Self {
        self.mmio.push((region_name.into(), range));
        self
    }

    /// Claim the specified PCI slot.
    pub fn claim_pci(mut self, bdf: (u8, u8, u8), vendor_id: u16, device_id: u16) -> Self {
        self.pci = Some(MissingDevPciMetadata {
            bdf,
            vendor_id,
            device_id,
        });
        self
    }
}

pub mod resolver {
    //! A resolver for [`MissingDevHandle`] resources.

    use crate::MissingDev;
    use crate::MissingDevManifest;
    use chipset_device_resources::ResolveChipsetDeviceHandleParams;
    use chipset_device_resources::ResolvedChipsetDevice;
    use missing_dev_resources::MissingDevHandle;
    use std::convert::Infallible;
    use vm_resource::ResolveResource;
    use vm_resource::declare_static_resolver;
    use vm_resource::kind::ChipsetDeviceHandleKind;

    /// A resolver for [`MissingDevHandle`] resources.
    pub struct MissingDevResolver;

    declare_static_resolver!(
        MissingDevResolver,
        (ChipsetDeviceHandleKind, MissingDevHandle)
    );

    impl ResolveResource<ChipsetDeviceHandleKind, MissingDevHandle> for MissingDevResolver {
        type Output = ResolvedChipsetDevice;
        type Error = Infallible;

        fn resolve(
            &self,
            resource: MissingDevHandle,
            input: ResolveChipsetDeviceHandleParams<'_>,
        ) -> Result<Self::Output, Self::Error> {
            input.configure.omit_saved_state();
            let dev = MissingDev::from_manifest(
                MissingDevManifest {
                    pio: resource
                        .pio
                        .into_iter()
                        .map(|(name, start, end)| (name.into(), start..=end))
                        .collect(),
                    mmio: resource
                        .mmio
                        .into_iter()
                        .map(|(name, start, end)| (name.into(), start..=end))
                        .collect(),
                    pci: None,
                },
                input.register_mmio,
                input.register_pio,
            );
            Ok(dev.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipset_device::mmio::ExternallyManagedMmioIntercepts;
    use chipset_device::pio::ExternallyManagedPortIoIntercepts;
    use test_with_tracing::test;

    /// Registers regions that remember their name, their length, and where
    /// they are mapped.
    struct Recorder;

    struct Region<T> {
        name: Box<str>,
        len: T,
        addr: Option<T>,
    }

    impl RegisterPortIoIntercept for Recorder {
        fn new_io_region(
            &mut self,
            region_name: &str,
            len: u16,
        ) -> Box<dyn ControlPortIoIntercept> {
            Box::new(Region {
                name: region_name.into(),
                len,
                addr: None,
            })
        }
    }

    impl RegisterMmioIntercept for Recorder {
        fn new_io_region(&mut self, region_name: &str, len: u64) -> Box<dyn ControlMmioIntercept> {
            Box::new(Region {
                name: region_name.into(),
                len,
                addr: None,
            })
        }
    }

    impl ControlPortIoIntercept for Region<u16> {
        fn region_name(&self) -> &str {
            &self.name
        }

        fn map(&mut self, addr: u16) {
            self.addr = Some(addr);
        }

        fn unmap(&mut self) {
            self.addr = None;
        }

        fn addr(&self) -> Option<u16> {
            self.addr
        }

        fn len(&self) -> u16 {
            self.len
        }

        fn offset_of(&self, _addr: u16) -> Option<u16> {
            unreachable!()
        }
    }

    impl ControlMmioIntercept for Region<u64> {
        fn region_name(&self) -> &str {
            &self.name
        }

        fn map(&mut self, addr: u64) {
            self.addr = Some(addr);
        }

        fn unmap(&mut self) {
            self.addr = None;
        }

        fn addr(&self) -> Option<u64> {
            self.addr
        }

        fn len(&self) -> u64 {
            self.len
        }

        fn offset_of(&self, _addr: u64) -> Option<u64> {
            unreachable!()
        }
    }

    /// The regions, as `(address, length)`, that a missing device that claims
    /// the ports of `range` maps, each under the range's name.
    fn pio(range: RangeInclusive<u16>) -> Vec<(Option<u16>, u16)> {
        let dev = MissingDev::from_manifest(
            MissingDevManifest::new().claim_pio("claim", range),
            &mut ExternallyManagedMmioIntercepts,
            &mut Recorder,
        );
        dev._pio_control
            .iter()
            .map(|region| {
                assert_eq!(region.region_name(), "claim");
                (region.addr(), region.len())
            })
            .collect()
    }

    /// The regions, as `(address, length)`, that a missing device that claims
    /// the MMIO addresses of `range` maps, each under the range's name.
    fn mmio(range: RangeInclusive<u64>) -> Vec<(Option<u64>, u64)> {
        let dev = MissingDev::from_manifest(
            MissingDevManifest::new().claim_mmio("claim", range),
            &mut Recorder,
            &mut ExternallyManagedPortIoIntercepts,
        );
        dev._mmio_control
            .iter()
            .map(|region| {
                assert_eq!(region.region_name(), "claim");
                (region.addr(), region.len())
            })
            .collect()
    }

    #[test]
    fn a_claimed_range_is_mapped_with_its_length() {
        assert_eq!(pio(0x60..=0x64), [(Some(0x60), 5)]);
        assert_eq!(pio(0xffff..=0xffff), [(Some(0xffff), 1)]);
        assert_eq!(pio(0..=0xfffe), [(Some(0), 0xffff)]);
        assert_eq!(pio(1..=0xffff), [(Some(1), 0xffff)]);
        assert_eq!(mmio(0x1000..=0x1fff), [(Some(0x1000), 0x1000)]);
        assert_eq!(mmio(1..=u64::MAX), [(Some(1), u64::MAX)]);
    }

    #[test]
    fn a_full_width_range_is_mapped_in_two_halves() {
        assert_eq!(pio(0..=0xffff), [(Some(0), 0x8000), (Some(0x8000), 0x8000)]);
        assert_eq!(
            mmio(0..=u64::MAX),
            [(Some(0), 1 << 63), (Some(1 << 63), 1 << 63)]
        );
    }

    #[test]
    fn an_empty_range_claims_nothing() {
        assert_eq!(pio(RangeInclusive::new(5, 3)), []);
        assert_eq!(pio(RangeInclusive::new(4, 3)), []);
        assert_eq!(mmio(RangeInclusive::new(5, 3)), []);
        assert_eq!(mmio(RangeInclusive::new(u64::MAX, 0)), []);
    }
}
