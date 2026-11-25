// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Notable Exports: [`Chipset`], [`ChipsetBuilder`]

pub mod backing;
mod builder;
mod io_ranges;
mod line_sets;

pub use self::builder::ChipsetBuilder;
pub use self::builder::ChipsetDevices;
pub use self::builder::DynamicDeviceUnit;

use self::io_ranges::IoRanges;
use self::io_ranges::LookupResult;
use crate::DebugEventHandler;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use closeable_mutex::CloseableMutex;
use cvm_tracing::CVM_CONFIDENTIAL;
use inspect::Inspect;
use std::future::poll_fn;
use std::sync::Arc;

/// The "glue" that interconnects virtual devices, and exposes an API for
/// external entities (such as VCPUs) to access devices.
#[derive(Inspect)]
pub struct Chipset {
    #[inspect(rename = "mmio")]
    mmio_ranges: IoRanges<u64>,
    #[inspect(rename = "pio")]
    pio_ranges: IoRanges<u16>,

    #[inspect(rename = "has_pic", with = "Option::is_some")]
    pic: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
    #[inspect(rename = "has_eoi_handler", with = "Option::is_some")]
    eoi_handler: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,

    #[inspect(skip)]
    debug_event_handler: Arc<dyn DebugEventHandler>,
}

enum IoType<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
}

#[derive(Copy, Clone, Debug)]
enum IoKind {
    Pio,
    Mmio,
}

impl IoType<'_> {
    fn bytes(&self) -> &[u8] {
        match self {
            IoType::Read(b) => b,
            IoType::Write(b) => b,
        }
    }
}

impl Chipset {
    async fn handle_io_result(
        &self,
        lookup: LookupResult,
        vp: u32,
        kind: IoKind,
        address: u64,
        len: usize,
        mut io_type: IoType<'_>,
        result: IoResult,
    ) {
        if lookup.debug_break {
            tracing::warn!(
                device = &*lookup.dev_name,
                address,
                len,
                ?kind,
                "debug break due to io"
            );
            self.debug_event_handler.on_debug_break(Some(vp));
        }
        match result {
            IoResult::Ok => {}
            IoResult::Defer(mut token) => match &mut io_type {
                IoType::Read(bytes) => {
                    if let Err(err) = poll_fn(|cx| token.poll_read(cx, bytes)).await {
                        self.handle_io_error(err, &lookup, kind, address, len, &mut io_type);
                    }
                }
                IoType::Write(_) => {
                    if let Err(err) = poll_fn(|cx| token.poll_write(cx)).await {
                        self.handle_io_error(err, &lookup, kind, address, len, &mut io_type);
                    }
                }
            },
            IoResult::Err(err) => {
                self.handle_io_error(err, &lookup, kind, address, len, &mut io_type);
            }
        };

        if let Some(range_name) = &lookup.trace {
            // Don't lower the tracing level or the whole thing is
            // useless.
            tracing::info!(
                device = &*lookup.dev_name,
                range_name = range_name.as_ref(),
                ?kind,
                address,
                direction = if matches!(io_type, IoType::Read(_)) {
                    "read"
                } else {
                    "write"
                },
                data = format_args!("{:02x?}", io_type.bytes()),
                "device io"
            );
        }
    }

    fn handle_io_error(
        &self,
        err: IoError,
        lookup: &LookupResult,
        kind: IoKind,
        address: u64,
        len: usize,
        io_type: &mut IoType<'_>,
    ) {
        let error = match err {
            IoError::InvalidRegister => "register not present",
            IoError::InvalidAccessSize => "invalid access size",
            IoError::UnalignedAccess => "unaligned access",
            IoError::NoResponse => "device didn't respond",
        };
        match io_type {
            IoType::Read(bytes) => {
                // Fill data with !0 to indicate an error to the guest.
                bytes.fill(!0);
                tracelimit::warn_ratelimited!(
                    CVM_CONFIDENTIAL,
                    device = &*lookup.dev_name,
                    address,
                    len,
                    ?kind,
                    error,
                    "device io read error"
                );
            }
            IoType::Write(bytes) => tracelimit::warn_ratelimited!(
                CVM_CONFIDENTIAL,
                device = &*lookup.dev_name,
                address,
                len,
                ?kind,
                error,
                ?bytes,
                "device io write error"
            ),
        }
    }

    /// Dispatch a MMIO read to the given address.
    pub async fn mmio_read(&self, vp: u32, address: u64, data: &mut [u8]) {
        let lookup = self.mmio_ranges.lookup(address, true);
        let r = lookup.dev.lock_if_open().map_or_else(
            || IoResult::Err(IoError::NoResponse),
            |mut dev| {
                dev.supports_mmio()
                    .expect("objects on the mmio bus support mmio")
                    .mmio_read(address, data)
            },
        );

        self.handle_io_result(
            lookup,
            vp,
            IoKind::Mmio,
            address,
            data.len(),
            IoType::Read(data),
            r,
        )
        .await
    }

    /// Dispatch a MMIO write to the given address.
    pub async fn mmio_write(&self, vp: u32, address: u64, data: &[u8]) {
        let lookup = self.mmio_ranges.lookup(address, false);
        let r = lookup.dev.lock_if_open().map_or_else(
            || IoResult::Err(IoError::NoResponse),
            |mut dev| {
                dev.supports_mmio()
                    .expect("objects on the mmio bus support mmio")
                    .mmio_write(address, data)
            },
        );

        self.handle_io_result(
            lookup,
            vp,
            IoKind::Mmio,
            address,
            data.len(),
            IoType::Write(data),
            r,
        )
        .await
    }

    /// Check if a MMIO device exists at the given address
    pub fn is_mmio(&self, addr: u64) -> bool {
        self.mmio_ranges.is_occupied(addr)
    }

    /// Dispatch a Port IO read to the given address.
    pub async fn io_read(&self, vp: u32, port: u16, data: &mut [u8]) {
        let lookup = self.pio_ranges.lookup(port, true);
        let r = lookup.dev.lock_if_open().map_or_else(
            || IoResult::Err(IoError::NoResponse),
            |mut dev| {
                dev.supports_pio()
                    .expect("objects on the pio bus support pio")
                    .io_read(port, data)
            },
        );

        self.handle_io_result(
            lookup,
            vp,
            IoKind::Pio,
            port.into(),
            data.len(),
            IoType::Read(data),
            r,
        )
        .await
    }

    /// Dispatch a Port IO write to the given address.
    pub async fn io_write(&self, vp: u32, port: u16, data: &[u8]) {
        let lookup = self.pio_ranges.lookup(port, false);
        let r = lookup.dev.lock_if_open().map_or_else(
            || IoResult::Err(IoError::NoResponse),
            |mut dev| {
                dev.supports_pio()
                    .expect("objects on the pio bus support pio")
                    .io_write(port, data)
            },
        );

        self.handle_io_result(
            lookup,
            vp,
            IoKind::Pio,
            port.into(),
            data.len(),
            IoType::Write(data),
            r,
        )
        .await
    }

    /// Gets the vector of the next interrupt to inject from the legacy
    /// interrupt controller (PIC) and sets the IRQ in service.
    pub fn acknowledge_pic_interrupt(&self) -> Option<u8> {
        self.pic
            .as_ref()?
            .lock()
            .supports_acknowledge_pic_interrupt()
            .unwrap()
            .acknowledge_interrupt()
    }

    /// Handle End Of Interrupt (EOI)
    ///
    /// A `u32` is used for the IRQ value for (future) ARM compat.
    pub fn handle_eoi(&self, irq: u32) {
        if let Some(eoi_handler) = &self.eoi_handler {
            eoi_handler
                .lock()
                .supports_handle_eoi()
                .unwrap()
                .handle_eoi(irq);
        } else {
            unreachable!("eoi exit received without a registered interrupt controller");
        }
    }
}

#[derive(Debug)]
pub enum PciConflictReason {
    ExistingDev(Arc<str>),
    MissingBus,
}

#[derive(Debug)]
pub struct PciConflict {
    pub bdf: (u8, u8, u8),
    pub conflict_dev: Arc<str>,
    pub reason: PciConflictReason,
}

impl std::fmt::Display for PciConflict {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            PciConflictReason::ExistingDev(existing_dev) => {
                let (b, d, f) = self.bdf;
                write!(
                    fmt,
                    "cannot attach {} to {:02x}:{:02x}:{}, already occupied by {}",
                    self.conflict_dev, b, d, f, existing_dev
                )
            }
            PciConflictReason::MissingBus => {
                let (b, d, f) = self.bdf;
                write!(
                    fmt,
                    "cannot attach {} to {:02x}:{:02x}:{}, no valid PCI bus",
                    self.conflict_dev, b, d, f
                )
            }
        }
    }
}

#[derive(Debug)]
pub enum PcieConflictReason {
    ExistingDev(Arc<str>),
    MissingDownstreamPort,
    MissingEnumerator,
    RciepNotSupported,
}

#[derive(Debug)]
pub struct PcieConflict {
    pub conflict_dev: Arc<str>,
    pub reason: PcieConflictReason,
}

impl std::fmt::Display for PcieConflict {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            PcieConflictReason::ExistingDev(existing_dev) => {
                write!(
                    fmt,
                    "cannot attach {}, port already occupied by {}",
                    self.conflict_dev, existing_dev
                )
            }
            PcieConflictReason::MissingDownstreamPort => {
                write!(
                    fmt,
                    "cannot attach {}, no valid pcie downstream port",
                    self.conflict_dev
                )
            }
            PcieConflictReason::MissingEnumerator => {
                write!(
                    fmt,
                    "cannot attach {}, no valid pcie enumerator",
                    self.conflict_dev
                )
            }
            PcieConflictReason::RciepNotSupported => {
                write!(
                    fmt,
                    "cannot attach {} as RCiEP, enumerator does not support RCiEPs",
                    self.conflict_dev
                )
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::Chipset;
    use super::ChipsetBuilder;
    use super::ChipsetDevices;
    use crate::BusIdPci;
    use crate::DebugEventHandler;
    use crate::chipset::backing::arc_mutex::device::AddDeviceError;
    use chipset_device::ChipsetDevice;
    use chipset_device::interrupt::AcknowledgePicInterrupt;
    use chipset_device::interrupt::HandleEoi;
    use chipset_device::io::IoError;
    use chipset_device::io::IoResult;
    use chipset_device::io::deferred::DeferredRead;
    use chipset_device::io::deferred::DeferredWrite;
    use chipset_device::io::deferred::defer_read;
    use chipset_device::io::deferred::defer_write;
    use chipset_device::mmio::ControlMmioIntercept;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::mmio::RegisterMmioIntercept;
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigSpace;
    use chipset_device::pio::PortIoIntercept;
    use closeable_mutex::CloseableMutex;
    use futures::FutureExt;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use state_unit::StateUnits;
    use std::ops::RangeInclusive;
    use std::sync::Arc;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;
    use vmcore::vmtime::VmTimeSource;

    struct PanicOnDebugBreak;

    impl DebugEventHandler for PanicOnDebugBreak {
        fn on_debug_break(&self, _: Option<u32>) {
            panic!("unexpected debug break");
        }
    }

    pub(crate) struct Fixture {
        pub(crate) source: VmTaskDriverSource,
        pub(crate) units: StateUnits,
        pub(crate) time_unit: state_unit::SpawnedUnit<()>,
        pub(crate) time: VmTimeSource,
        _keeper: VmTimeKeeper,
    }

    impl Fixture {
        pub(crate) async fn new(driver: &DefaultDriver) -> Self {
            let units = StateUnits::new();
            let time_unit = units
                .add("vmtime")
                .spawn(driver.clone(), async |mut recv| {
                    while let Ok(req) = recv.recv().await {
                        req.apply(&mut NoopUnit).await;
                    }
                })
                .unwrap();
            let keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
            let time = keeper.builder().build(driver).await.unwrap();
            Self {
                source: VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
                units,
                time_unit,
                time,
                _keeper: keeper,
            }
        }

        pub(crate) fn builder(
            &self,
            fallback_mmio: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
        ) -> ChipsetBuilder<'_> {
            self.builder_with_debug(fallback_mmio, Arc::new(PanicOnDebugBreak))
        }

        pub(crate) fn builder_with_debug(
            &self,
            fallback_mmio: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
            debug: Arc<dyn DebugEventHandler>,
        ) -> ChipsetBuilder<'_> {
            ChipsetBuilder::new(
                &self.source,
                &self.units,
                debug,
                &self.time,
                self.time_unit.handle(),
                false,
                false,
                fallback_mmio,
            )
        }
    }

    /// Stands in for the VM time unit so whole-VM state transitions complete.
    #[derive(InspectMut)]
    struct NoopUnit;

    impl state_unit::StateUnit for NoopUnit {
        async fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn save(
            &mut self,
        ) -> Result<Option<vmcore::save_restore::SavedStateBlob>, SaveError> {
            Ok(None)
        }

        async fn restore(
            &mut self,
            _: vmcore::save_restore::SavedStateBlob,
        ) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    #[derive(Debug, PartialEq)]
    pub(crate) struct Access {
        pub(crate) device: &'static str,
        pub(crate) kind: &'static str,
        pub(crate) addr: u64,
        pub(crate) data: Vec<u8>,
    }

    #[derive(Default)]
    pub(crate) struct Shared {
        pub(crate) accesses: Vec<Access>,
        pub(crate) events: Vec<String>,
        deferred_reads: Vec<DeferredRead>,
        deferred_writes: Vec<DeferredWrite>,
        eois: Vec<u32>,
    }

    #[derive(Clone, Copy, PartialEq)]
    pub(crate) enum Pci {
        None,
        NoSuggestedBdf,
        SuggestedBdf(u8, u8, u8),
    }

    #[derive(Clone, Copy)]
    pub(crate) enum Response {
        Ok,
        Fail,
        Defer,
    }

    pub(crate) struct TestDevice {
        pub(crate) name: &'static str,
        pub(crate) mmio: Vec<(&'static str, RangeInclusive<u64>)>,
        pub(crate) pio: Vec<(&'static str, RangeInclusive<u16>)>,
        pub(crate) response: Response,
        pub(crate) read_fill: u8,
        pub(crate) region: Option<Box<dyn ControlMmioIntercept>>,
        pub(crate) pci: Pci,
        pub(crate) pic_vector: Option<u8>,
        pub(crate) eoi: bool,
        pub(crate) line: Option<LineInterrupt>,
        pub(crate) shared: Arc<Mutex<Shared>>,
    }

    impl TestDevice {
        pub(crate) fn new(name: &'static str, shared: &Arc<Mutex<Shared>>) -> Self {
            Self {
                name,
                mmio: Vec::new(),
                pio: Vec::new(),
                response: Response::Ok,
                read_fill: 0,
                region: None,
                pci: Pci::None,
                pic_vector: None,
                eoi: false,
                line: None,
                shared: shared.clone(),
            }
        }

        fn read(&mut self, kind: &'static str, addr: u64, data: &mut [u8]) -> IoResult {
            let mut shared = self.shared.lock();
            shared.accesses.push(Access {
                device: self.name,
                kind,
                addr,
                data: Vec::new(),
            });
            match self.response {
                Response::Ok => {
                    data.fill(self.read_fill);
                    IoResult::Ok
                }
                Response::Fail => {
                    data.fill(self.read_fill);
                    IoResult::Err(IoError::InvalidRegister)
                }
                Response::Defer => {
                    let (read, token) = defer_read();
                    shared.deferred_reads.push(read);
                    IoResult::Defer(token)
                }
            }
        }

        fn write(&mut self, kind: &'static str, addr: u64, data: &[u8]) -> IoResult {
            let mut shared = self.shared.lock();
            shared.accesses.push(Access {
                device: self.name,
                kind,
                addr,
                data: data.to_vec(),
            });
            match self.response {
                Response::Ok => IoResult::Ok,
                Response::Fail => IoResult::Err(IoError::InvalidRegister),
                Response::Defer => {
                    let (write, token) = defer_write();
                    shared.deferred_writes.push(write);
                    IoResult::Defer(token)
                }
            }
        }
    }

    impl InspectMut for TestDevice {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.ignore();
        }
    }

    impl ChangeDeviceState for TestDevice {
        fn start(&mut self) {
            self.shared
                .lock()
                .events
                .push(format!("{}:start", self.name));
        }

        async fn stop(&mut self) {
            self.shared
                .lock()
                .events
                .push(format!("{}:stop", self.name));
        }

        async fn reset(&mut self) {
            self.shared
                .lock()
                .events
                .push(format!("{}:reset", self.name));
        }
    }

    impl SaveRestore for TestDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    impl ChipsetDevice for TestDevice {
        fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
            Some(self)
        }

        fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
            Some(self)
        }

        fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
            (self.pci != Pci::None).then_some(self)
        }

        fn supports_acknowledge_pic_interrupt(
            &mut self,
        ) -> Option<&mut dyn AcknowledgePicInterrupt> {
            self.pic_vector.is_some().then_some(self)
        }

        fn supports_handle_eoi(&mut self) -> Option<&mut dyn HandleEoi> {
            self.eoi.then_some(self)
        }
    }

    impl MmioIntercept for TestDevice {
        fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
            &self.mmio
        }

        fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
            self.read("mmio", addr, data)
        }

        fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
            self.write("mmio", addr, data)
        }
    }

    impl PortIoIntercept for TestDevice {
        fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
            &self.pio
        }

        fn io_read(&mut self, port: u16, data: &mut [u8]) -> IoResult {
            self.read("pio", port.into(), data)
        }

        fn io_write(&mut self, port: u16, data: &[u8]) -> IoResult {
            self.write("pio", port.into(), data)
        }
    }

    impl PciConfigSpace for TestDevice {
        fn pci_cfg_read(&mut self, _: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
            value.set(u32::from_ne_bytes([self.read_fill; 4]));
            IoResult::Ok
        }

        fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
            self.shared.lock().accesses.push(Access {
                device: self.name,
                kind: "pci",
                addr: offset.into(),
                data: value.as_valid_byte_slice().to_vec(),
            });
            IoResult::Ok
        }

        fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
            match self.pci {
                Pci::SuggestedBdf(b, d, f) => Some((b, d, f)),
                Pci::None | Pci::NoSuggestedBdf => None,
            }
        }
    }

    impl AcknowledgePicInterrupt for TestDevice {
        fn acknowledge_interrupt(&mut self) -> Option<u8> {
            self.pic_vector
        }
    }

    impl HandleEoi for TestDevice {
        fn handle_eoi(&mut self, irq: u32) {
            self.shared.lock().eois.push(irq);
        }
    }

    pub(crate) fn access(
        device: &'static str,
        kind: &'static str,
        addr: u64,
        data: &[u8],
    ) -> Access {
        Access {
            device,
            kind,
            addr,
            data: data.to_vec(),
        }
    }

    pub(crate) fn build(builder: ChipsetBuilder<'_>) -> (Arc<Chipset>, ChipsetDevices) {
        builder.build().unwrap()
    }

    pub(crate) fn add_error<T>(result: Result<T, AddDeviceError>) -> String {
        match result {
            Ok(_) => panic!("add unexpectedly succeeded"),
            Err(err) => format!("{:#}", anyhow::Error::from(err)),
        }
    }

    pub(crate) fn build_error(builder: ChipsetBuilder<'_>) -> String {
        match builder.build() {
            Ok(_) => panic!("build unexpectedly succeeded"),
            Err(err) => format!("{:#}", anyhow::Error::from(err)),
        }
    }

    // Test: Two devices with static MMIO/PIO ranges, then reads and writes at
    //       range boundaries through the chipset.
    // Expected: Each access reaches only the owning device with its address and
    //       data, and reads return that device's data.
    #[async_test]
    async fn accesses_reach_owning_device(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let builder = fixture.builder(None);
        let shared = Arc::new(Mutex::new(Shared::default()));
        builder
            .arc_mutex_device("a")
            .add(|_| TestDevice {
                mmio: vec![("a-mmio", 0x1000..=0x1fff)],
                pio: vec![("a-pio", 0x60..=0x63)],
                read_fill: 0xaa,
                ..TestDevice::new("a", &shared)
            })
            .unwrap();
        builder
            .arc_mutex_device("b")
            .add(|_| TestDevice {
                mmio: vec![("b-mmio", 0x2000..=0x2fff)],
                pio: vec![("b-pio", 0x64..=0x64)],
                read_fill: 0xbb,
                ..TestDevice::new("b", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        let mut data = [0; 4];
        chipset.mmio_read(0, 0x1ffc, &mut data).await;
        assert_eq!(data, [0xaa; 4]);
        chipset.mmio_read(0, 0x2000, &mut data).await;
        assert_eq!(data, [0xbb; 4]);
        chipset.mmio_write(0, 0x1000, &[1, 2, 3, 4]).await;
        let mut byte = [0];
        chipset.io_read(0, 0x63, &mut byte).await;
        assert_eq!(byte, [0xaa]);
        chipset.io_read(0, 0x64, &mut byte).await;
        assert_eq!(byte, [0xbb]);
        chipset.io_write(0, 0x60, &[5]).await;

        assert_eq!(
            std::mem::take(&mut shared.lock().accesses),
            [
                access("a", "mmio", 0x1ffc, &[]),
                access("b", "mmio", 0x2000, &[]),
                access("a", "mmio", 0x1000, &[1, 2, 3, 4]),
                access("a", "pio", 0x63, &[]),
                access("b", "pio", 0x64, &[]),
                access("a", "pio", 0x60, &[5]),
            ]
        );
        assert!(chipset.is_mmio(0x1fff));
        assert!(chipset.is_mmio(0x2000));
    }

    // Test: MMIO/PIO accesses just outside a device's static ranges.
    // Expected: Reads return all ones, writes are dropped, the device sees
    //       nothing, and `is_mmio` reports the address as unclaimed.
    #[async_test]
    async fn unclaimed_accesses_read_all_ones(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let builder = fixture.builder(None);
        let shared = Arc::new(Mutex::new(Shared::default()));
        builder
            .arc_mutex_device("a")
            .add(|_| TestDevice {
                mmio: vec![("a-mmio", 0x1000..=0x1fff)],
                pio: vec![("a-pio", 0x60..=0x63)],
                ..TestDevice::new("a", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        let mut data = [0; 8];
        chipset.mmio_read(0, 0x2000, &mut data).await;
        assert_eq!(data, [!0; 8]);
        chipset.mmio_write(0, 0xfff, &[1; 4]).await;
        let mut data = [0; 2];
        chipset.io_read(0, 0x64, &mut data).await;
        assert_eq!(data, [!0; 2]);
        chipset.io_write(0, 0x5f, &[1]).await;

        assert!(shared.lock().accesses.is_empty());
        assert!(!chipset.is_mmio(0xfff));
        assert!(!chipset.is_mmio(0x2000));
    }

    // Test: A device that partly fills the read buffer and then fails both
    //       reads and writes.
    // Expected: The guest reads all ones rather than the partial data, and a
    //       failed write still reaches the device without disturbing later I/O.
    #[async_test]
    async fn device_errors_read_all_ones(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let builder = fixture.builder(None);
        let shared = Arc::new(Mutex::new(Shared::default()));
        builder
            .arc_mutex_device("a")
            .add(|_| TestDevice {
                mmio: vec![("a-mmio", 0x1000..=0x1fff)],
                pio: vec![("a-pio", 0x60..=0x63)],
                response: Response::Fail,
                read_fill: 0x12,
                ..TestDevice::new("a", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        let mut data = [0; 4];
        chipset.mmio_read(0, 0x1000, &mut data).await;
        assert_eq!(data, [!0; 4]);
        let mut data = [0; 2];
        chipset.io_read(0, 0x60, &mut data).await;
        assert_eq!(data, [!0; 2]);
        chipset.mmio_write(0, 0x1004, &[7]).await;
        chipset.io_write(0, 0x61, &[8]).await;

        assert_eq!(
            shared.lock().accesses,
            [
                access("a", "mmio", 0x1000, &[]),
                access("a", "pio", 0x60, &[]),
                access("a", "mmio", 0x1004, &[7]),
                access("a", "pio", 0x61, &[8]),
            ]
        );
    }

    // Test: A device that defers its reads and writes, then completes, fails,
    //       or abandons them.
    // Expected: The chipset access stays pending until the device answers;
    //       completed reads return the device data, and failed or abandoned
    //       reads return all ones.
    #[async_test]
    async fn deferred_accesses_complete_with_device_result(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let builder = fixture.builder(None);
        let shared = Arc::new(Mutex::new(Shared::default()));
        builder
            .arc_mutex_device("a")
            .add(|_| TestDevice {
                mmio: vec![("a-mmio", 0x1000..=0x1fff)],
                pio: vec![("a-pio", 0x60..=0x63)],
                response: Response::Defer,
                ..TestDevice::new("a", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        let mut data = [0; 4];
        {
            let mut read = std::pin::pin!(chipset.mmio_read(0, 0x1000, &mut data));
            assert!((&mut read).now_or_never().is_none());
            let deferred = shared.lock().deferred_reads.pop().unwrap();
            deferred.complete(&[1, 2, 3, 4]);
            read.await;
        }
        assert_eq!(data, [1, 2, 3, 4]);

        let mut data = [0; 2];
        {
            let mut read = std::pin::pin!(chipset.io_read(0, 0x60, &mut data));
            assert!((&mut read).now_or_never().is_none());
            let deferred = shared.lock().deferred_reads.pop().unwrap();
            deferred.complete_error(IoError::InvalidAccessSize);
            read.await;
        }
        assert_eq!(data, [!0; 2]);

        let mut data = [0; 4];
        {
            let mut read = std::pin::pin!(chipset.mmio_read(0, 0x1000, &mut data));
            assert!((&mut read).now_or_never().is_none());
            drop(shared.lock().deferred_reads.pop().unwrap());
            read.await;
        }
        assert_eq!(data, [!0; 4]);

        let mut write = std::pin::pin!(chipset.mmio_write(0, 0x1008, &[9]));
        assert!((&mut write).now_or_never().is_none());
        shared.lock().deferred_writes.pop().unwrap().complete();
        write.await;

        let mut write = std::pin::pin!(chipset.io_write(0, 0x62, &[10]));
        assert!((&mut write).now_or_never().is_none());
        shared
            .lock()
            .deferred_writes
            .pop()
            .unwrap()
            .complete_error(IoError::InvalidRegister);
        write.await;

        assert_eq!(shared.lock().accesses.len(), 5);
    }

    // Test: A chipset with a fallback MMIO device and one claimed range.
    // Expected: Unclaimed MMIO goes to the fallback, claimed MMIO goes to its
    //       owner, unclaimed port I/O still reads all ones, and `is_mmio` stays
    //       false for fallback-only addresses.
    #[async_test]
    async fn fallback_device_receives_only_unclaimed_mmio(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let fallback: Arc<CloseableMutex<dyn ChipsetDevice>> =
            Arc::new(CloseableMutex::new(TestDevice {
                read_fill: 0xfb,
                ..TestDevice::new("fallback", &shared)
            }));
        let builder = fixture.builder(Some(fallback));
        builder
            .arc_mutex_device("a")
            .add(|_| TestDevice {
                mmio: vec![("a-mmio", 0x1000..=0x1fff)],
                read_fill: 0xaa,
                ..TestDevice::new("a", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        let mut data = [0; 4];
        chipset.mmio_read(0, 0x8000, &mut data).await;
        assert_eq!(data, [0xfb; 4]);
        chipset.mmio_write(0, 0x8004, &[3]).await;
        chipset.mmio_read(0, 0x1000, &mut data).await;
        assert_eq!(data, [0xaa; 4]);
        let mut byte = [0];
        chipset.io_read(0, 0x80, &mut byte).await;
        assert_eq!(byte, [!0]);

        assert_eq!(
            shared.lock().accesses,
            [
                access("fallback", "mmio", 0x8000, &[]),
                access("fallback", "mmio", 0x8004, &[3]),
                access("a", "mmio", 0x1000, &[]),
            ]
        );
        assert!(!chipset.is_mmio(0x8000));
    }

    // Test: Static MMIO and PIO ranges that overlap by one address, and ranges
    //       that are exactly adjacent.
    // Expected: Overlaps make build fail and name both devices; adjacent ranges
    //       build successfully.
    #[async_test]
    async fn overlapping_static_ranges_fail_build(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));

        let builder = fixture.builder(None);
        builder
            .arc_mutex_device("first")
            .add(|_| TestDevice {
                mmio: vec![("first-mmio", 0x1000..=0x1fff)],
                pio: vec![("first-pio", 0x60..=0x63)],
                ..TestDevice::new("first", &shared)
            })
            .unwrap();
        builder
            .arc_mutex_device("second")
            .add(|_| TestDevice {
                mmio: vec![("second-mmio", 0x1fff..=0x2fff)],
                pio: vec![("second-pio", 0x63..=0x64)],
                ..TestDevice::new("second", &shared)
            })
            .unwrap();
        let err = build_error(builder);
        assert!(
            err.contains("static mmio intercept region conflict"),
            "{err}"
        );
        assert!(
            err.contains("static pio intercept region conflict"),
            "{err}"
        );
        assert!(err.contains("second/second-mmio"), "{err}");
        assert!(err.contains("first/first-mmio"), "{err}");
        assert!(err.contains("second/second-pio"), "{err}");
        assert!(err.contains("first/first-pio"), "{err}");

        let builder = fixture.builder(None);
        builder
            .arc_mutex_device("first")
            .add(|_| TestDevice {
                mmio: vec![("first-mmio", 0x1000..=0x1fff)],
                pio: vec![("first-pio", 0x60..=0x63)],
                ..TestDevice::new("first", &shared)
            })
            .unwrap();
        builder
            .arc_mutex_device("second")
            .add(|_| TestDevice {
                mmio: vec![("second-mmio", 0x2000..=0x2fff)],
                pio: vec![("second-pio", 0x64..=0x64)],
                ..TestDevice::new("second", &shared)
            })
            .unwrap();
        build(builder);
    }

    // Test: PCI devices added without a bus, without a BDF, and on a bus that
    //       was never registered.
    // Expected: The first two are rejected when added and the third makes build
    //       fail with the device name and requested BDF.
    #[async_test]
    async fn pci_device_wiring_errors_are_reported(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let builder = fixture.builder(None);

        let err = add_error(builder.arc_mutex_device("no-bus").add(|_| TestDevice {
            pci: Pci::SuggestedBdf(0, 1, 0),
            ..TestDevice::new("no-bus", &shared)
        }));
        assert!(err.contains("no pci bus specified"), "{err}");

        let err = add_error(
            builder
                .arc_mutex_device("no-bdf")
                .on_pci_bus(BusIdPci::new("missing"))
                .add(|_| TestDevice {
                    pci: Pci::NoSuggestedBdf,
                    ..TestDevice::new("no-bdf", &shared)
                }),
        );
        assert!(err.contains("no pci bus address provided"), "{err}");

        builder
            .arc_mutex_device("orphan")
            .on_pci_bus(BusIdPci::new("missing"))
            .with_pci_addr(0, 3, 1)
            .add(|_| TestDevice {
                pci: Pci::SuggestedBdf(0, 2, 0),
                ..TestDevice::new("orphan", &shared)
            })
            .unwrap();
        let err = build_error(builder);
        assert!(
            err.contains("cannot attach orphan to 00:03:1, no valid PCI bus"),
            "{err}"
        );
    }

    // Test: A device remaps a dynamic MMIO region after build, then tries to
    //       map it over another device's static range.
    // Expected: Dispatch follows the current mapping with the old address
    //       released, and the conflicting map never takes over the static
    //       range.
    #[async_test]
    async fn runtime_region_mapping_moves_dispatch(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let builder = fixture.builder(None);
        builder
            .arc_mutex_device("static")
            .add(|_| TestDevice {
                mmio: vec![("static-mmio", 0x1000..=0x1fff)],
                read_fill: 0x55,
                ..TestDevice::new("static", &shared)
            })
            .unwrap();
        let dynamic = builder
            .arc_mutex_device("dynamic")
            .add(|services| TestDevice {
                region: Some(services.register_mmio().new_io_region("bar", 0x100)),
                read_fill: 0xdd,
                ..TestDevice::new("dynamic", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);
        let mut data = [0; 4];

        dynamic.lock().region.as_mut().unwrap().map(0x4000);
        chipset.mmio_read(0, 0x40fc, &mut data).await;
        assert_eq!(data, [0xdd; 4]);

        dynamic.lock().region.as_mut().unwrap().map(0x5000);
        assert!(!chipset.is_mmio(0x4000));
        chipset.mmio_read(0, 0x4000, &mut data).await;
        assert_eq!(data, [!0; 4]);
        chipset.mmio_read(0, 0x5000, &mut data).await;
        assert_eq!(data, [0xdd; 4]);

        dynamic.lock().region.as_mut().unwrap().map(0x1f80);
        assert!(!chipset.is_mmio(0x5000));
        chipset.mmio_read(0, 0x5000, &mut data).await;
        assert_eq!(data, [!0; 4]);
        chipset.mmio_read(0, 0x1f80, &mut data).await;
        assert_eq!(data, [0x55; 4]);
        chipset.mmio_read(0, 0x2000, &mut data).await;
        assert_eq!(data, [!0; 4]);

        dynamic.lock().region.as_mut().unwrap().map(0x5000);
        assert!(chipset.is_mmio(0x5000));
        chipset.mmio_read(0, 0x5000, &mut data).await;
        assert_eq!(data, [0xdd; 4]);
        dynamic.lock().region.as_mut().unwrap().unmap();
        assert!(!chipset.is_mmio(0x5000));
        chipset.mmio_read(0, 0x5000, &mut data).await;
        assert_eq!(data, [!0; 4]);
    }

    // Test: A chipset without a PIC, then one with a PIC/EOI device, and a
    //       second PIC/EOI device added after the first.
    // Expected: Acknowledgement returns no vector without a PIC, routes to the
    //       PIC when present, EOIs reach the handler, and the duplicate handler
    //       is rejected.
    #[async_test]
    async fn pic_and_eoi_route_to_registered_handlers(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));

        {
            let (chipset, _devices) = build(fixture.builder(None));
            assert_eq!(chipset.acknowledge_pic_interrupt(), None);
        }

        let builder = fixture.builder(None);
        builder
            .arc_mutex_device("pic")
            .add(|_| TestDevice {
                pic_vector: Some(0x20),
                eoi: true,
                ..TestDevice::new("pic", &shared)
            })
            .unwrap();
        let err = add_error(builder.arc_mutex_device("second-pic").add(|_| TestDevice {
            pic_vector: Some(0x28),
            ..TestDevice::new("second-pic", &shared)
        }));
        assert!(err.contains("pic handler already set"), "{err}");
        let err = add_error(builder.arc_mutex_device("second-eoi").add(|_| TestDevice {
            eoi: true,
            ..TestDevice::new("second-eoi", &shared)
        }));
        assert!(err.contains("eoi handler already set"), "{err}");
        let (chipset, _devices) = build(builder);

        assert_eq!(chipset.acknowledge_pic_interrupt(), Some(0x20));
        chipset.handle_eoi(4);
        chipset.handle_eoi(9);
        assert_eq!(shared.lock().eois, [4, 9]);
    }

    const ECAM_BASE: u64 = 0xe000_0000;

    fn ecam(bus: u8, devfn: u8, offset: u16) -> u64 {
        ECAM_BASE | (u64::from(bus) << 20) | (u64::from(devfn) << 12) | u64::from(offset)
    }

    async fn ecam_read(chipset: &Chipset, bus: u8, devfn: u8, offset: u16) -> u32 {
        let mut data = [0; 4];
        chipset
            .mmio_read(0, ecam(bus, devfn, offset), &mut data)
            .await;
        u32::from_le_bytes(data)
    }

    /// Adds a PCIe root complex named `rc0` covering buses 0-1, with one root
    /// port `rp0` at device 1.
    fn add_root_complex(builder: &ChipsetBuilder<'_>, msi: &pci_core::msi::MsiTarget) {
        let rc = builder
            .arc_mutex_device("rc0")
            .try_add(|services| {
                let mut mmio = services.register_mmio();
                let mut pio = services.register_pio();
                pcie::root::GenericPcieRootComplex::builder(
                    &mut mmio,
                    &mut pio,
                    0..=1,
                    memory_range::MemoryRange::new(ECAM_BASE..ECAM_BASE + 0x20_0000),
                )
                .root_ports(
                    vec![pcie::GenericPciePortDefinition {
                        name: "rp0".into(),
                        devfn: Some(0x08),
                        hotplug: false,
                        settings: Default::default(),
                    }],
                    msi,
                )
                .build()
            })
            .unwrap();
        builder.register_weak_mutex_pcie_enumerator(crate::BusId::new("rc0"), Box::new(rc));
    }

    // Test: A root complex with an integrated endpoint at device 0 and an
    //       endpoint behind root port `rp0`; the guest reads config space
    //       through ECAM, programs the root port's bus numbers, then reads
    //       again.
    // Expected: The integrated endpoint answers on bus 0, the endpoint is
    //       unreachable until the root port forwards bus 1 and answers after,
    //       and a config write reaches it.
    #[async_test]
    async fn pcie_config_space_reaches_root_complex_devices(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let msi = pci_core::msi::MsiTarget::disconnected();
        let builder = fixture.builder(None);
        add_root_complex(&builder, &msi);
        builder
            .arc_mutex_device("rciep")
            .on_pcie_root_complex(crate::BusId::new("rc0"), 0)
            .add(|_| TestDevice {
                pci: Pci::NoSuggestedBdf,
                read_fill: 0x11,
                ..TestDevice::new("rciep", &shared)
            })
            .unwrap();
        builder
            .arc_mutex_device("endpoint")
            .on_pcie_port(crate::BusId::new("rp0"))
            .add(|_| TestDevice {
                pci: Pci::NoSuggestedBdf,
                read_fill: 0x22,
                ..TestDevice::new("endpoint", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        assert_eq!(ecam_read(&chipset, 0, 0, 0).await, 0x1111_1111);
        assert_ne!(ecam_read(&chipset, 0, 0x08, 0).await, !0);
        assert_eq!(ecam_read(&chipset, 0, 0x10, 0).await, !0);
        assert_eq!(ecam_read(&chipset, 1, 0, 0).await, !0);

        // Primary bus 0, secondary bus 1, subordinate bus 1.
        chipset
            .mmio_write(0, ecam(0, 0x08, 0x18), &0x0001_0100u32.to_le_bytes())
            .await;
        assert_eq!(ecam_read(&chipset, 1, 0, 0).await, 0x2222_2222);
        chipset
            .mmio_write(0, ecam(1, 0, 0x10), &0xfeed_0000u32.to_le_bytes())
            .await;
        assert_eq!(
            shared.lock().accesses,
            [access(
                "endpoint",
                "pci",
                0x10,
                &0xfeed_0000u32.to_le_bytes()
            )]
        );
    }

    // Test: PCIe devices placed on a port and an enumerator that don't exist, a
    //       second device on an occupied root port, and an integrated endpoint
    //       on the root port's device number.
    // Expected: Build fails and reports every misplaced device by name.
    #[async_test]
    async fn pcie_placement_errors_fail_build(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let msi = pci_core::msi::MsiTarget::disconnected();
        let builder = fixture.builder(None);
        add_root_complex(&builder, &msi);
        let pci = |name| TestDevice {
            pci: Pci::NoSuggestedBdf,
            ..TestDevice::new(name, &shared)
        };
        builder
            .arc_mutex_device("first")
            .on_pcie_port(crate::BusId::new("rp0"))
            .add(|_| pci("first"))
            .unwrap();
        builder
            .arc_mutex_device("second")
            .on_pcie_port(crate::BusId::new("rp0"))
            .add(|_| pci("second"))
            .unwrap();
        builder
            .arc_mutex_device("no-port")
            .on_pcie_port(crate::BusId::new("rp9"))
            .add(|_| pci("no-port"))
            .unwrap();
        builder
            .arc_mutex_device("no-rc")
            .on_pcie_root_complex(crate::BusId::new("rc9"), 0)
            .add(|_| pci("no-rc"))
            .unwrap();
        builder
            .arc_mutex_device("on-port")
            .on_pcie_root_complex(crate::BusId::new("rc0"), 0x08)
            .add(|_| pci("on-port"))
            .unwrap();

        let err = build_error(builder);
        for expected in [
            "cannot attach second, port already occupied by first",
            "cannot attach no-port, no valid pcie downstream port",
            "cannot attach no-rc, no valid pcie enumerator",
            "cannot attach on-port, port already occupied by rp0",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
    }

    // IGNORED: this test fails because of a known bug. The default
    // `pci_cfg_read_with_routing` accepts only `devfn == 0`, although its docs
    // say "function 0". The root complex passes the real devfn to integrated
    // endpoints, so one at any device other than 0 reads as absent. The only
    // current user (the AMD IOMMU) sits at device 0. Re-enable this test
    // together with the fix by removing #[ignore].
    //
    // Test: An integrated endpoint that only implements `pci_cfg_read` is
    //       placed at device 2 of the root complex and read through ECAM.
    // Expected: Config reads at 0:2.0 reach the device.
    #[async_test]
    #[ignore = "known bug: default PCI routing only reaches integrated endpoints at devfn 0"]
    async fn integrated_endpoint_at_nonzero_device_is_reachable(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let msi = pci_core::msi::MsiTarget::disconnected();
        let builder = fixture.builder(None);
        add_root_complex(&builder, &msi);
        builder
            .arc_mutex_device("rciep")
            .on_pcie_root_complex(crate::BusId::new("rc0"), 0x10)
            .add(|_| TestDevice {
                pci: Pci::NoSuggestedBdf,
                read_fill: 0x33,
                ..TestDevice::new("rciep", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);
        assert_eq!(ecam_read(&chipset, 0, 0x10, 0).await, 0x3333_3333);
    }

    // Test: After the chipset is built, add a device at runtime that maps an
    //       MMIO region, access it, then remove the device.
    // Expected: The region reaches the device while it is present and reads all
    //       ones after removal.
    #[async_test]
    async fn dynamic_device_is_reachable_until_removed(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (chipset, devices) = build(fixture.builder(None));

        let (unit, _device) = devices
            .add_dyn_device(
                &fixture.source,
                &fixture.units,
                "dynamic",
                async |mmio: &mut (dyn RegisterMmioIntercept + Send)| {
                    let mut region = mmio.new_io_region("bar", 0x1000);
                    region.map(0x9000_0000);
                    Ok(TestDevice {
                        region: Some(region),
                        read_fill: 0x77,
                        ..TestDevice::new("dynamic", &shared)
                    })
                },
            )
            .await
            .unwrap();

        let mut data = [0; 4];
        chipset.mmio_read(0, 0x9000_0ffc, &mut data).await;
        assert_eq!(data, [0x77; 4]);
        assert!(chipset.is_mmio(0x9000_0000));

        unit.remove().await;
        chipset.mmio_read(0, 0x9000_0ffc, &mut data).await;
        assert_eq!(data, [!0; 4]);
        assert!(!chipset.is_mmio(0x9000_0000));
    }

    // Test: Start, stop, save, reset, restore, and restart the VM's state units
    //       with a device that drives an interrupt line and a second device
    //       that opts out of saved state.
    // Expected: Every transition reaches both devices, saved state includes
    //       only the device that didn't opt out, restore accepts it, and I/O
    //       works after the restart.
    #[async_test]
    async fn vm_state_transitions_reach_devices(driver: DefaultDriver) {
        let mut fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (chipset, _devices) = {
            let builder = fixture.builder(None);
            builder
                .arc_mutex_device("saved")
                .add(|services| TestDevice {
                    mmio: vec![("saved-mmio", 0x1000..=0x1fff)],
                    read_fill: 0xaa,
                    line: Some(services.new_line(
                        chipset_device_resources::IRQ_LINE_SET,
                        "test",
                        5,
                    )),
                    ..TestDevice::new("saved", &shared)
                })
                .unwrap();
            builder
                .arc_mutex_device("omitted")
                .omit_saved_state()
                .add(|_| TestDevice::new("omitted", &shared))
                .unwrap();
            build(builder)
        };
        let take_events = || {
            let mut events = std::mem::take(&mut shared.lock().events);
            events.sort();
            events
        };

        fixture.units.start().await;
        assert_eq!(take_events(), ["omitted:start", "saved:start"]);
        fixture.units.stop().await;
        assert_eq!(take_events(), ["omitted:stop", "saved:stop"]);

        let saved = fixture.units.save().await.unwrap();
        let names: Vec<_> = saved.iter().map(|unit| unit.name.as_str()).collect();
        assert!(names.contains(&"saved"), "{names:?}");
        assert!(!names.contains(&"omitted"), "{names:?}");

        fixture.units.reset().await.unwrap();
        assert_eq!(take_events(), ["omitted:reset", "saved:reset"]);
        fixture.units.restore(saved).await.unwrap();
        fixture.units.start().await;
        assert_eq!(take_events(), ["omitted:start", "saved:start"]);

        let mut data = [0; 4];
        chipset.mmio_read(0, 0x1000, &mut data).await;
        assert_eq!(data, [0xaa; 4]);
    }

    #[derive(Default)]
    struct RecordBreaks(Mutex<Vec<Option<u32>>>);

    impl DebugEventHandler for RecordBreaks {
        fn on_debug_break(&self, vp: Option<u32>) {
            self.0.lock().push(vp);
        }
    }

    // Test: Through inspect, set an MMIO break filter on one address, a port
    //       I/O break filter on unclaimed ports, and tracing on all MMIO, then
    //       access claimed and unclaimed addresses from different VPs and
    //       inspect the MMIO ranges.
    // Expected: Only accesses matching a break filter call the debug handler,
    //       with the accessing VP; accesses still complete normally; and
    //       inspect lists the claimed range under its device.
    #[async_test]
    async fn inspect_controls_debug_breaks_and_lists_ranges(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let breaks = Arc::new(RecordBreaks::default());
        let builder = fixture.builder_with_debug(None, breaks.clone());
        builder
            .arc_mutex_device("a")
            .add(|_| TestDevice {
                mmio: vec![("a-mmio", 0x1000..=0x1fff)],
                pio: vec![("a-pio", 0x60..=0x63)],
                read_fill: 0xaa,
                ..TestDevice::new("a", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        inspect::update("mmio/break_on", "0x1004", &*chipset)
            .await
            .unwrap();
        inspect::update("pio/break_on", "?", &*chipset)
            .await
            .unwrap();
        inspect::update("mmio/trace_on", "all", &*chipset)
            .await
            .unwrap();

        let mut data = [0; 4];
        chipset.mmio_read(3, 0x1000, &mut data).await;
        assert!(breaks.0.lock().is_empty());
        chipset.mmio_read(3, 0x1004, &mut data).await;
        assert_eq!(data, [0xaa; 4]);
        chipset.mmio_read(2, 0x8000, &mut data).await;
        assert_eq!(data, [!0; 4]);
        let mut byte = [0];
        chipset.io_read(1, 0x60, &mut byte).await;
        chipset.io_read(1, 0x80, &mut byte).await;
        assert_eq!(byte, [!0]);
        assert_eq!(*breaks.0.lock(), [Some(3), Some(1)]);

        let mut inspection = inspect::inspect("mmio", &*chipset);
        inspection.resolve().await;
        let output = inspection.results().to_string();
        assert!(output.contains("0x1000-0x1fff"), "{output}");
        assert!(output.contains("a-mmio"), "{output}");
    }

    // Test: Add a device whose constructor fails, a device built by an async
    //       constructor, and a PCI device marked as externally managed with no
    //       bus.
    // Expected: The failure is reported with the device name and cause, the
    //       async device answers I/O, and the external PCI device doesn't need
    //       a bus or address for the build to succeed.
    #[async_test]
    async fn device_builder_variants(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let builder = fixture.builder(None);

        let err = add_error(
            builder
                .arc_mutex_device::<TestDevice>("broken")
                .try_add(|_| Err(anyhow::anyhow!("no backing store"))),
        );
        assert!(err.contains("could not initialize broken"), "{err}");
        assert!(err.contains("could not construct device"), "{err}");
        assert!(err.contains("no backing store"), "{err}");

        builder
            .arc_mutex_device("async")
            .add_async(async |_| TestDevice {
                mmio: vec![("async-mmio", 0x3000..=0x3fff)],
                read_fill: 0x3a,
                ..TestDevice::new("async", &shared)
            })
            .await
            .unwrap();
        builder
            .arc_mutex_device("external")
            .with_external_pci()
            .add(|_| TestDevice {
                pci: Pci::NoSuggestedBdf,
                ..TestDevice::new("external", &shared)
            })
            .unwrap();
        let (chipset, _devices) = build(builder);

        let mut data = [0; 4];
        chipset.mmio_read(0, 0x3000, &mut data).await;
        assert_eq!(data, [0x3a; 4]);
    }
}
