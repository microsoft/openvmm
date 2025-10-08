// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple chipset that only supports MMIO intercepts.

use crate::device::ArcMutexChipsetDeviceBuilder;
use crate::device::ArcMutexChipsetServicesFinalize;
use crate::services::ChipsetServices;
use crate::services::ChipsetServicesMeta;
use crate::services::MmioInterceptServices;
use crate::services::Unimplemented;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use closeable_mutex::CloseableMutex;
use parking_lot::RwLock;
use range_map_vec::RangeMap;
use std::cell::Cell;
use std::sync::Arc;
use std::sync::Weak;

type MmioRanges = Arc<RwLock<RangeMap<u64, (Box<str>, Weak<CloseableMutex<dyn ChipsetDevice>>)>>>;

/// A concrete type which implements [`RegisterMmioIntercept`]
pub struct TestMmioRangeMapper {
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    map: MmioRanges,
}

// Implementation detail - the concrete type returned by TestMmioRangeMapper's
// `new_io_region` implementation
struct TestDeviceRange {
    map: MmioRanges,
    region_name: Box<str>,
    len: u64,
    addr: Option<u64>,
    io: Weak<CloseableMutex<dyn ChipsetDevice>>,
}

impl RegisterMmioIntercept for TestMmioRangeMapper {
    fn new_io_region(&mut self, region_name: &str, len: u64) -> Box<dyn ControlMmioIntercept> {
        Box::new(TestDeviceRange {
            map: self.map.clone(),
            region_name: region_name.into(),
            len,
            addr: None,
            io: self.dev.clone(),
        })
    }
}
impl ControlMmioIntercept for TestDeviceRange {
    fn region_name(&self) -> &str {
        &self.region_name
    }

    fn map(&mut self, addr: u64) {
        self.unmap();
        if self.map.write().insert(
            addr..=addr
                .checked_add(self.len - 1)
                .expect("overflow during addition, not possible in real hardware"),
            (self.region_name.clone(), self.io.clone()),
        ) {
            self.addr = Some(addr);
        } else {
            panic!("conflict!")
        }
    }

    fn unmap(&mut self) {
        if let Some(addr) = self.addr.take() {
            let _entry = self.map.write().remove(&addr).unwrap();
        }
    }

    fn addr(&self) -> Option<u64> {
        self.addr
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn offset_of(&self, addr: u64) -> Option<u64> {
        let base = self.addr?;

        (base..(base + self.len))
            .contains(&addr)
            .then(|| addr - base)
    }
}

/// A simple chipset that only models MMIO intercepts.
#[derive(Default)]
pub struct TestChipset {
    mmio_ranges: MmioRanges,
}

impl TestChipset {
    /// Return a device builder associated with the chipset
    pub fn device_builder<T: ChipsetDevice>(
        &self,
        name: &'static str,
    ) -> ArcMutexChipsetDeviceBuilder<TestChipsetServicesImpl<'_>, T> {
        ArcMutexChipsetDeviceBuilder::new(name.into(), |dev, _name| TestChipsetServicesImpl {
            vm_chipset: self,
            dev,
            took_mmio: false.into(),
        })
    }

    /// Dispatch a MMIO read to the given address.
    pub fn mmio_read(&self, addr: u64, data: &mut [u8]) -> Option<()> {
        let dev = self.mmio_ranges.read().get(&addr)?.1.upgrade()?;
        // devices might want to map/unmap ranges as part of a MMIO access,
        // so don't hold the range lock for any longer than we need to
        match dev
            .lock()
            .supports_mmio()
            .expect("objects on the mmio bus support mmio")
            .mmio_read(addr, data)
        {
            IoResult::Ok => {}
            IoResult::Err(_) => {
                data.fill(!0);
            }
            IoResult::Defer(_) => unreachable!(),
        }
        Some(())
    }

    /// Dispatch a MMIO write to the given address.
    pub fn mmio_write(&self, addr: u64, data: &[u8]) -> Option<()> {
        let dev = self.mmio_ranges.read().get(&addr)?.1.upgrade()?;
        // devices might want to map/unmap ranges as part of a MMIO access,
        // so don't hold the range lock for any longer than we need to
        let _ = dev
            .lock()
            .supports_mmio()
            .expect("objects on the mmio bus support mmio")
            .mmio_write(addr, data);
        Some(())
    }
}

/// Implementation of [`ChipsetServices`] associated with [`TestChipset`]
pub struct TestChipsetServicesImpl<'a> {
    vm_chipset: &'a TestChipset,
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    took_mmio: Cell<bool>,
}

/// Compile-time type metadata used by [`TestChipsetServicesImpl`]'s
/// [`ChipsetServices`] impl
pub enum TestChipsetServicesMeta {}
impl ChipsetServicesMeta for TestChipsetServicesMeta {
    type RegisterMmioIntercept = TestMmioRangeMapper;
    type RegisterPortIoIntercept = Unimplemented;
}

impl ChipsetServices for TestChipsetServicesImpl<'_> {
    type M = TestChipsetServicesMeta;

    #[inline(always)]
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioInterceptServices<M = Self::M>> {
        Some(self)
    }
}

impl<T> ArcMutexChipsetServicesFinalize<T> for TestChipsetServicesImpl<'_> {
    fn finalize(self, _dev: &Arc<CloseableMutex<T>>, _name: Arc<str>) {}
}

impl MmioInterceptServices for TestChipsetServicesImpl<'_> {
    /// Obtain an instance of [`RegisterMmioIntercept`]
    fn register_mmio(&self) -> TestMmioRangeMapper {
        self.took_mmio.set(true);
        TestMmioRangeMapper {
            dev: self.dev.clone(),
            map: self.vm_chipset.mmio_ranges.clone(),
        }
    }

    fn is_being_used(&self) -> bool {
        self.took_mmio.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod sample_dev {
        use super::*;
        use chipset_device::io::IoResult;
        use chipset_device::mmio::MmioIntercept;
        use std::ops::RangeInclusive;

        pub struct SampleDevice {
            pub mmio_control: Box<dyn ControlMmioIntercept>,
            pub mmio_read_log: Vec<u64>,
        }

        impl SampleDevice {
            pub fn new(
                register_mmio: &mut dyn RegisterMmioIntercept,
            ) -> Result<Self, std::convert::Infallible> {
                Ok(SampleDevice {
                    mmio_control: register_mmio.new_io_region("dynamic", 1),
                    mmio_read_log: Vec::new(),
                })
            }
        }

        impl ChipsetDevice for SampleDevice {
            fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
                Some(self)
            }
        }

        impl MmioIntercept for SampleDevice {
            fn mmio_read(&mut self, addr: u64, _: &mut [u8]) -> IoResult {
                self.mmio_read_log.push(addr);
                IoResult::Ok
            }

            fn mmio_write(&mut self, _: u64, _: &[u8]) -> IoResult {
                IoResult::Ok
            }

            fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
                &[("static", 10..=10)]
            }
        }
    }

    #[test]
    fn closure() -> Result<(), Box<dyn std::error::Error>> {
        let vm_chipset = TestChipset::default();

        let devices_builder = ArcMutexChipsetDeviceBuilder::new("sample".into(), |dev, _name| {
            TestChipsetServicesImpl {
                vm_chipset: &vm_chipset,
                dev,
                took_mmio: false.into(),
            }
        });

        let sample_dev: Arc<CloseableMutex<sample_dev::SampleDevice>> = devices_builder
            .try_add(|services| sample_dev::SampleDevice::new(&mut services.register_mmio()))?;

        // give it a go
        assert!(vm_chipset.mmio_read(10, &mut []).is_some());
        assert!(vm_chipset.mmio_read(11, &mut []).is_none());
        sample_dev.lock().mmio_control.map(11);
        assert!(vm_chipset.mmio_read(11, &mut []).is_some());
        sample_dev.lock().mmio_control.unmap();
        assert!(vm_chipset.mmio_read(11, &mut []).is_none());

        assert_eq!(sample_dev.lock().mmio_read_log, [10, 11]);

        Ok(())
    }
}
