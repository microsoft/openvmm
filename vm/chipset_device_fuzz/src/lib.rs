// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A chipset for fuzz-testing devices.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use chipset_arc_mutex_device::device::ArcMutexChipsetDeviceBuilder;
use chipset_arc_mutex_device::device::ArcMutexChipsetServicesFinalize;
use chipset_arc_mutex_device::services::ChipsetServices;
use chipset_arc_mutex_device::services::ChipsetServicesMeta;
use chipset_arc_mutex_device::services::MmioInterceptServices;
use chipset_arc_mutex_device::services::PciConfigSpaceServices;
use chipset_arc_mutex_device::services::PollDeviceServices;
use chipset_arc_mutex_device::services::PortIoInterceptServices;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use closeable_mutex::CloseableMutex;
use parking_lot::RwLock;
use range_map_vec::RangeMap;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use zerocopy::FromBytes;

type InterceptRanges<U> =
    Arc<RwLock<RangeMap<U, (Box<str>, Weak<CloseableMutex<dyn ChipsetDevice>>)>>>;

/// A chipset for fuzz-testing devices.
///
/// Intelligently generates MMIO/PIO/PCI accesses based on what interfaces the
/// device supports, and what intercepts the device has configured.
///
/// Resilient against runtime remapping of intercept regions.
#[derive(Default)]
pub struct FuzzChipset {
    devices: Vec<Arc<CloseableMutex<dyn ChipsetDevice>>>,
    mmio_ranges: InterceptRanges<u64>,
    pio_ranges: InterceptRanges<u16>,
    pci_devices: BTreeMap<(u8, u8, u8), Weak<CloseableMutex<dyn ChipsetDevice>>>,
    poll_devices: Vec<Weak<CloseableMutex<dyn ChipsetDevice>>>,
    max_defer_poll_count: usize,
}

impl FuzzChipset {
    /// Construct a new `FuzzChipset`. Any asynchronous operations will be polled
    /// at most `max_poll_count` times before panicking.
    pub fn new(max_poll_count: usize) -> Self {
        Self {
            devices: Default::default(),
            mmio_ranges: Default::default(),
            pio_ranges: Default::default(),
            pci_devices: Default::default(),
            poll_devices: Default::default(),
            max_defer_poll_count: max_poll_count,
        }
    }

    /// Return a device builder associated with the chipset
    pub fn device_builder<T: ChipsetDevice>(
        &mut self,
        name: &'static str,
    ) -> ArcMutexChipsetDeviceBuilder<FuzzChipsetServicesImpl<'_>, T> {
        ArcMutexChipsetDeviceBuilder::new(name.into(), |dev, _name| {
            FuzzChipsetServicesImpl::new(self, dev)
        })
    }

    /// Dispatch a MMIO read to the given address.
    fn mmio_read(&self, addr: u64, data: &mut [u8]) -> Option<()> {
        // devices might want to map/unmap ranges as part of a MMIO access,
        // so don't hold the range lock for any longer than we need to
        let dev = self.mmio_ranges.read().get(&addr)?.1.upgrade().unwrap();
        let mut locked_dev = dev.lock();
        let result = locked_dev
            .supports_mmio()
            .expect("objects on the mmio bus support mmio")
            .mmio_read(addr, data);
        // Convert to a non-deferred result
        let result = match result {
            IoResult::Ok => Ok(()),
            IoResult::Err(e) => Err(e),
            IoResult::Defer(t) => self.defer_read_now_or_never(&mut *locked_dev, t, data),
        };
        match result {
            Ok(()) => {}
            Err(_) => {
                data.fill(!0);
            }
        }
        Some(())
    }

    /// Dispatch a MMIO write to the given address.
    fn mmio_write(&self, addr: u64, data: &[u8]) -> Option<()> {
        // devices might want to map/unmap ranges as part of a MMIO access,
        // so don't hold the range lock for any longer than we need to
        let dev = self.mmio_ranges.read().get(&addr)?.1.upgrade().unwrap();
        let mut locked_dev = dev.lock();
        let result = locked_dev
            .supports_mmio()
            .expect("objects on the mmio bus support mmio")
            .mmio_write(addr, data);
        match result {
            IoResult::Ok => {}
            IoResult::Err(_) => {}
            IoResult::Defer(t) => {
                let _ = self.defer_write_now_or_never(&mut *locked_dev, t);
            }
        }
        Some(())
    }

    /// Dispatch a port io read to the given address.
    fn pio_read(&self, addr: u16, data: &mut [u8]) -> Option<()> {
        // devices might want to map/unmap ranges as part of a pio access,
        // so don't hold the range lock for any longer than we need to
        let dev = self.pio_ranges.read().get(&addr)?.1.upgrade().unwrap();
        let mut locked_dev = dev.lock();
        let result = locked_dev
            .supports_pio()
            .expect("objects on the pio bus support pio")
            .io_read(addr, data);
        // Convert to a non-deferred result
        let result = match result {
            IoResult::Ok => Ok(()),
            IoResult::Err(e) => Err(e),
            IoResult::Defer(t) => self.defer_read_now_or_never(&mut *locked_dev, t, data),
        };
        match result {
            Ok(()) => {}
            Err(_) => {
                data.fill(!0);
            }
        }
        Some(())
    }

    /// Dispatch a port io write to the given address.
    fn pio_write(&self, addr: u16, data: &[u8]) -> Option<()> {
        // devices might want to map/unmap ranges as part of a pio access,
        // so don't hold the range lock for any longer than we need to
        let dev = self.pio_ranges.read().get(&addr)?.1.upgrade().unwrap();
        let mut locked_dev = dev.lock();
        let result = locked_dev
            .supports_pio()
            .expect("objects on the pio bus support pio")
            .io_write(addr, data);
        match result {
            IoResult::Ok => {}
            IoResult::Err(_) => {}
            IoResult::Defer(t) => {
                let _ = self.defer_write_now_or_never(&mut *locked_dev, t);
            }
        }
        Some(())
    }

    /// Dispatch a PCI read to the given device + offset.
    fn pci_read(&self, bdf: (u8, u8, u8), offset: u16, data: &mut [u8]) -> Option<()> {
        let dev = self.pci_devices.get(&bdf)?.upgrade().unwrap();
        let mut locked_dev = dev.lock();
        let result = locked_dev
            .supports_pci()
            .expect("objects on the pci bus support pci")
            .pci_cfg_read(offset, u32::mut_from_bytes(data).unwrap());
        // Convert to a non-deferred result
        let result = match result {
            IoResult::Ok => Ok(()),
            IoResult::Err(e) => Err(e),
            IoResult::Defer(t) => self.defer_read_now_or_never(&mut *locked_dev, t, data),
        };
        match result {
            Ok(()) => {}
            Err(_) => {
                data.fill(0);
            }
        }
        Some(())
    }

    /// Dispatch a PCI write to the given device + offset.
    fn pci_write(&self, bdf: (u8, u8, u8), offset: u16, value: u32) -> Option<()> {
        let dev = self.pci_devices.get(&bdf)?.upgrade().unwrap();
        let mut locked_dev = dev.lock();
        let result = locked_dev
            .supports_pci()
            .expect("objects on the pci bus support pci")
            .pci_cfg_write(offset, value);
        match result {
            IoResult::Ok => {}
            IoResult::Err(_) => {}
            IoResult::Defer(t) => {
                let _ = self.defer_write_now_or_never(&mut *locked_dev, t);
            }
        }
        Some(())
    }

    /// Poll the given device.
    fn poll_device(&self, index: usize) -> Option<()> {
        self.poll_devices[index]
            .upgrade()
            .unwrap()
            .lock()
            .supports_poll_device()
            .expect("objects supporting polling support polling")
            .poll_device(&mut Context::from_waker(Waker::noop()));
        Some(())
    }

    /// Poll a deferred read once, panic if it isn't complete afterwards.
    fn defer_read_now_or_never(
        &self,
        dev: &mut dyn ChipsetDevice,
        mut t: DeferredToken,
        data: &mut [u8],
    ) -> Result<(), IoError> {
        let mut cx = Context::from_waker(Waker::noop());
        let dev = dev
            .supports_poll_device()
            .expect("objects returning a DeferredToken support polling");
        // Some devices (like IDE) will limit the amount of work they perform in a single poll
        // even though forward progress is still possible. We poll the device multiple times
        // to let these actions complete. If the action is still pending after all these polls
        // we know that something is actually wrong.
        for _ in 0..self.max_defer_poll_count {
            dev.poll_device(&mut cx);
            match t.poll_read(&mut cx, data) {
                Poll::Ready(r) => return r,
                Poll::Pending => {}
            }
        }
        if self.max_defer_poll_count == 0 {
            panic!(
                "Device operation returned a deferred read. Call FuzzChipset::new and set a non-zero max_poll_count to poll async operations."
            );
        } else {
            panic!(
                "Device operation returned a deferred read that didn't complete after {} polls",
                self.max_defer_poll_count
            )
        }
    }

    /// Poll a deferred write once, panic if it isn't complete afterwards.
    fn defer_write_now_or_never(
        &self,
        dev: &mut dyn ChipsetDevice,
        mut t: DeferredToken,
    ) -> Result<(), IoError> {
        let mut cx = Context::from_waker(Waker::noop());
        let dev = dev
            .supports_poll_device()
            .expect("objects returning a DeferredToken support polling");
        // Some devices (like IDE) will limit the amount of work they perform in a single poll
        // even though forward progress is still possible. We poll the device multiple times
        // to let these actions complete. If the action is still pending after all these polls
        // we know that something is actually wrong.
        for _ in 0..self.max_defer_poll_count {
            dev.poll_device(&mut cx);
            match t.poll_write(&mut cx) {
                Poll::Ready(r) => return r,
                Poll::Pending => {}
            }
        }
        if self.max_defer_poll_count == 0 {
            panic!(
                "Device operation returned a deferred write. Call FuzzChipset::new and set a non-zero max_poll_count to poll async operations."
            );
        } else {
            panic!(
                "Device operation returned a deferred write that didn't complete after {} polls",
                self.max_defer_poll_count
            )
        }
    }

    /// Intelligently suggest a random `ChipsetAction`, based on the currently
    /// registered devices, intercept regions, etc...
    pub fn get_arbitrary_action(
        &self,
        u: &mut arbitrary::Unstructured<'_>,
    ) -> arbitrary::Result<ChipsetAction> {
        #[derive(arbitrary::Arbitrary)]
        enum ChipsetActionKind {
            MmioRead,
            MmioWrite,
            PortIoRead,
            PortIoWrite,
            PciRead,
            PciWrite,
            Poll,
        }

        let action_kind: ChipsetActionKind = u.arbitrary()?;
        let action = match action_kind {
            ChipsetActionKind::MmioRead | ChipsetActionKind::MmioWrite => {
                let active_ranges = self
                    .mmio_ranges
                    .read()
                    .iter()
                    .map(|(r, _)| r)
                    .collect::<Vec<_>>();
                let range = u.choose(&active_ranges)?;

                let addr = u.int_in_range(range.clone())?;
                let len = *u.choose(&[1, 2, 4, 8])?;

                if matches!(action_kind, ChipsetActionKind::MmioRead) {
                    ChipsetAction::MmioRead { addr, len }
                } else {
                    let val = u.bytes(len)?.to_vec();
                    ChipsetAction::MmioWrite { addr, val }
                }
            }
            ChipsetActionKind::PortIoRead | ChipsetActionKind::PortIoWrite => {
                let active_ranges = self
                    .pio_ranges
                    .read()
                    .iter()
                    .map(|(r, _)| r)
                    .collect::<Vec<_>>();
                let range = u.choose(&active_ranges)?;

                let addr = u.int_in_range(range.clone())?;
                let len = *u.choose(&[1, 2, 4])?;

                if matches!(action_kind, ChipsetActionKind::PortIoRead) {
                    ChipsetAction::PortIoRead { addr, len }
                } else {
                    let val = u.bytes(len)?.to_vec();
                    ChipsetAction::PortIoWrite { addr, val }
                }
            }
            ChipsetActionKind::PciRead | ChipsetActionKind::PciWrite => {
                let attached_bdfs = self.pci_devices.keys().collect::<Vec<_>>();
                let bdf = **u.choose(&attached_bdfs)?;

                let offset = u.int_in_range(0..=4096)?; // pci-e max cfg space size

                if matches!(action_kind, ChipsetActionKind::PciRead) {
                    ChipsetAction::PciRead { bdf, offset }
                } else {
                    ChipsetAction::PciWrite {
                        bdf,
                        offset,
                        val: u.arbitrary()?,
                    }
                }
            }
            ChipsetActionKind::Poll => {
                let index = u.choose_index(self.poll_devices.len())?;
                ChipsetAction::Poll { index }
            }
        };

        Ok(action)
    }

    /// Execute the provided `ChipsetAction`
    pub fn exec_action(&self, action: ChipsetAction) -> Option<()> {
        let mut buf = [0; 8];
        match action {
            ChipsetAction::MmioRead { addr, len } => self.mmio_read(addr, &mut buf[..len]),
            ChipsetAction::MmioWrite { addr, val } => self.mmio_write(addr, &val),
            ChipsetAction::PortIoRead { addr, len } => self.pio_read(addr, &mut buf[..len]),
            ChipsetAction::PortIoWrite { addr, val } => self.pio_write(addr, &val),
            ChipsetAction::PciRead { bdf, offset } => self.pci_read(bdf, offset, &mut buf[..4]),
            ChipsetAction::PciWrite { bdf, offset, val } => self.pci_write(bdf, offset, val),
            ChipsetAction::Poll { index } => self.poll_device(index),
        }
    }
}

#[derive(Debug)]
pub enum ChipsetAction {
    MmioRead {
        addr: u64,
        len: usize,
    },
    MmioWrite {
        addr: u64,
        val: Vec<u8>,
    },
    PortIoRead {
        addr: u16,
        len: usize,
    },
    PortIoWrite {
        addr: u16,
        val: Vec<u8>,
    },
    PciRead {
        bdf: (u8, u8, u8),
        offset: u16,
    },
    PciWrite {
        bdf: (u8, u8, u8),
        offset: u16,
        val: u32,
    },
    Poll {
        index: usize,
    },
}

/// A concrete type which implements [`RegisterMmioIntercept`]
pub struct FuzzRegisterIntercept<U> {
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    map: InterceptRanges<U>,
}

// Implementation detail - the concrete type returned by TestMmioRangeMapper's
// `new_io_region` implementation
struct FuzzControlIntercept<U> {
    map: InterceptRanges<U>,
    region_name: Box<str>,
    len: U,
    addr: Option<U>,
    io: Weak<CloseableMutex<dyn ChipsetDevice>>,
}

macro_rules! impl_intercept {
    ($register_trait:ident, $control_trait:ident, $register:ident, $control:ident, $usize:ty) => {
        pub type $register = FuzzRegisterIntercept<$usize>;
        type $control = FuzzControlIntercept<$usize>;

        impl $register_trait for $register {
            fn new_io_region(&mut self, region_name: &str, len: $usize) -> Box<dyn $control_trait> {
                Box::new($control {
                    map: self.map.clone(),
                    region_name: region_name.into(),
                    len,
                    addr: None,
                    io: self.dev.clone(),
                })
            }
        }

        impl $control_trait for $control {
            fn region_name(&self) -> &str {
                &self.region_name
            }

            fn map(&mut self, addr: $usize) {
                self.unmap();
                if self.map.write().insert(
                    addr..=addr
                        .checked_add(self.len - 1)
                        .expect("overflow during addition, not possible in real hardware"),
                    (self.region_name.clone(), self.io.clone()),
                ) {
                    self.addr = Some(addr);
                } else {
                    tracing::trace!("{}::map failed", stringify!($control));
                }
            }

            fn unmap(&mut self) {
                if let Some(addr) = self.addr.take() {
                    let _entry = self.map.write().remove(&addr).unwrap();
                }
            }

            fn addr(&self) -> Option<$usize> {
                self.addr
            }

            fn len(&self) -> $usize {
                self.len
            }

            fn offset_of(&self, addr: $usize) -> Option<$usize> {
                let base = self.addr?;

                (base..(base + self.len))
                    .contains(&addr)
                    .then(|| addr - base)
            }
        }
    };
}

impl_intercept!(
    RegisterMmioIntercept,
    ControlMmioIntercept,
    FuzzRegisterMmioIntercept,
    FuzzControlMmioIntercept,
    u64
);
impl_intercept!(
    RegisterPortIoIntercept,
    ControlPortIoIntercept,
    FuzzRegisterPortIoIntercept,
    FuzzControlPortIoIntercept,
    u16
);

/// Implementation of [`ChipsetServices`] associated with [`FuzzChipset`]
pub struct FuzzChipsetServicesImpl<'a> {
    vm_chipset: &'a mut FuzzChipset,
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    took_mmio: Cell<bool>,
    took_pio: Cell<bool>,
    took_pci: Cell<bool>,
    took_poll: Cell<bool>,
}

impl<'a> FuzzChipsetServicesImpl<'a> {
    pub fn new(
        vm_chipset: &'a mut FuzzChipset,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    ) -> Self {
        Self {
            vm_chipset,
            dev,
            took_mmio: false.into(),
            took_pio: false.into(),
            took_pci: false.into(),
            took_poll: false.into(),
        }
    }
}

/// Compile-time type metadata used by [`FuzzChipsetServicesImpl`]'s
/// [`ChipsetServices`] impl
pub enum FuzzChipsetServicesMeta {}
impl ChipsetServicesMeta for FuzzChipsetServicesMeta {
    type RegisterMmioIntercept = FuzzRegisterMmioIntercept;
    type RegisterPortIoIntercept = FuzzRegisterPortIoIntercept;
}

impl ChipsetServices for FuzzChipsetServicesImpl<'_> {
    type M = FuzzChipsetServicesMeta;

    #[inline(always)]
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioInterceptServices<M = Self::M>> {
        Some(self)
    }

    #[inline(always)]
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoInterceptServices<M = Self::M>> {
        Some(self)
    }

    #[inline(always)]
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpaceServices<M = Self::M>> {
        Some(self)
    }

    #[inline(always)]
    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDeviceServices<M = Self::M>> {
        Some(self)
    }
}

impl<T: ChipsetDevice> ArcMutexChipsetServicesFinalize<T> for FuzzChipsetServicesImpl<'_> {
    fn finalize(self, dev: &Arc<CloseableMutex<T>>, _name: Arc<str>) {
        self.vm_chipset.devices.push(dev.clone());
    }
}

impl MmioInterceptServices for FuzzChipsetServicesImpl<'_> {
    fn register_mmio(&self) -> FuzzRegisterMmioIntercept {
        self.took_mmio.set(true);
        FuzzRegisterMmioIntercept {
            dev: self.dev.clone(),
            map: self.vm_chipset.mmio_ranges.clone(),
        }
    }

    fn is_being_used(&self) -> bool {
        self.took_mmio.get()
    }
}

impl PortIoInterceptServices for FuzzChipsetServicesImpl<'_> {
    fn register_pio(&self) -> FuzzRegisterPortIoIntercept {
        self.took_pio.set(true);
        FuzzRegisterPortIoIntercept {
            dev: self.dev.clone(),
            map: self.vm_chipset.pio_ranges.clone(),
        }
    }

    fn is_being_used(&self) -> bool {
        self.took_pio.get()
    }
}

impl PciConfigSpaceServices for FuzzChipsetServicesImpl<'_> {
    fn register_static_pci(&mut self, bus: u8, device: u8, function: u8) {
        self.took_pci.set(true);
        self.vm_chipset
            .pci_devices
            .insert((bus, device, function), self.dev.clone());
    }

    fn is_being_used(&self) -> bool {
        self.took_pci.get()
    }
}

impl PollDeviceServices for FuzzChipsetServicesImpl<'_> {
    fn register_poll(&mut self) {
        self.took_poll.set(true);
        self.vm_chipset.poll_devices.push(self.dev.clone());
    }

    fn is_being_used(&self) -> bool {
        self.took_poll.get()
    }
}
