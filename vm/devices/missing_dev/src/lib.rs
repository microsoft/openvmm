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
    pub fn from_manifest(
        manifest: MissingDevManifest,
        register_mmio: &mut dyn RegisterMmioIntercept,
        register_pio: &mut dyn RegisterPortIoIntercept,
    ) -> MissingDev {
        let MissingDevManifest { pio, mmio, pci } = manifest;

        let pio_control = pio
            .iter()
            .map(|(name, range)| {
                let mut control = register_pio.new_io_region(name, range.end() - range.start() + 1);
                control.map(*range.start());
                control
            })
            .collect();

        let mmio_control = mmio
            .iter()
            .map(|(name, range)| {
                let mut control =
                    register_mmio.new_io_region(name, range.end() - range.start() + 1);
                control.map(*range.start());
                control
            })
            .collect();

        MissingDev {
            pio,
            mmio,
            pci,

            _pio_control: pio_control,
            _mmio_control: mmio_control,
        }
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
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
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
            pci_core::cfg_space_emu::DeviceBars::new(),
        )
        .read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> IoResult {
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
