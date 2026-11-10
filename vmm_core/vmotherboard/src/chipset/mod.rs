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

    /// Compare and exchange a memory-like MMIO location, or perform the legacy
    /// unconditional write when the device does not implement this operation.
    pub async fn mmio_compare_exchange(
        &self,
        vp: u32,
        address: u64,
        current: &[u8],
        new: &[u8],
    ) -> std::io::Result<bool> {
        if current.is_empty() || current.len() != new.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "compare-and-exchange operands must be nonempty and the same size",
            ));
        }
        let lookup = self.mmio_ranges.lookup(address, false);
        let operation = lookup.dev.lock_if_open().and_then(|mut dev| {
            dev.supports_mmio()
                .expect("objects on the mmio bus support mmio")
                .supports_compare_exchange()
                .map(|atomic| atomic.compare_exchange(address, current, new))
        });
        if let Some(operation) = operation {
            let result = operation.await;
            if !matches!(result, Ok(None)) {
                if lookup.debug_break {
                    tracing::warn!(
                        device = &*lookup.dev_name,
                        address,
                        len = new.len(),
                        "debug break due to io"
                    );
                    self.debug_event_handler.on_debug_break(Some(vp));
                }
                if let Some(range_name) = &lookup.trace {
                    tracing::info!(
                        device = &*lookup.dev_name,
                        range_name = range_name.as_ref(),
                        address,
                        direction = "compare_exchange",
                        ?result,
                        "device io"
                    );
                }
            }
            match result {
                Ok(Some(exchanged)) => {
                    return Ok(exchanged);
                }
                Ok(None) => {}
                Err(err) => {
                    return Err(std::io::Error::other(format!(
                        "MMIO compare-and-exchange at {address:#x} failed: {err:?}"
                    )));
                }
            }
        }
        let result = lookup.dev.lock_if_open().map_or_else(
            || IoResult::Err(IoError::NoResponse),
            |mut dev| {
                dev.supports_mmio()
                    .expect("objects on the mmio bus support mmio")
                    .mmio_write(address, new)
            },
        );
        self.handle_io_result(
            lookup,
            vp,
            IoKind::Mmio,
            address,
            new.len(),
            IoType::Write(new),
            result,
        )
        .await;
        Ok(true)
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
mod tests {
    use super::*;
    use chipset_device::mmio::MmioCompareExchange;
    use chipset_device::mmio::MmioCompareExchangeFuture;
    use chipset_device::mmio::MmioIntercept;
    use pal_async::async_test;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    struct DebugHandler;

    impl DebugEventHandler for DebugHandler {
        fn on_debug_break(&self, _: Option<u32>) {
            panic!("unexpected debug break");
        }
    }

    struct TestDevice {
        value: Arc<AtomicU32>,
    }

    impl ChipsetDevice for TestDevice {
        fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
            Some(self)
        }
    }

    impl MmioIntercept for TestDevice {
        fn mmio_read(&mut self, _: u64, data: &mut [u8]) -> IoResult {
            data.copy_from_slice(&self.value.load(Ordering::SeqCst).to_ne_bytes());
            IoResult::Ok
        }

        fn mmio_write(&mut self, _: u64, data: &[u8]) -> IoResult {
            let Ok(bytes) = data.try_into() else {
                return IoResult::Err(IoError::InvalidAccessSize);
            };
            self.value
                .store(u32::from_ne_bytes(bytes), Ordering::SeqCst);
            IoResult::Ok
        }

        fn supports_compare_exchange(&mut self) -> Option<&mut dyn MmioCompareExchange> {
            Some(self)
        }
    }

    struct LegacyDevice(Arc<AtomicU32>);

    impl ChipsetDevice for LegacyDevice {
        fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
            Some(self)
        }
    }

    impl MmioIntercept for LegacyDevice {
        fn mmio_read(&mut self, _: u64, data: &mut [u8]) -> IoResult {
            data.copy_from_slice(&self.0.load(Ordering::SeqCst).to_ne_bytes());
            IoResult::Ok
        }

        fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
            let Ok(bytes) = data.try_into() else {
                return IoResult::Err(IoError::InvalidAccessSize);
            };
            self.0.store(u32::from_ne_bytes(bytes), Ordering::SeqCst);
            if address == 0x10c {
                let (write, token) = chipset_device::io::deferred::defer_write();
                write.complete();
                IoResult::Defer(token)
            } else {
                IoResult::Ok
            }
        }
    }

    impl MmioCompareExchange for TestDevice {
        fn compare_exchange(
            &mut self,
            address: u64,
            current: &[u8],
            new: &[u8],
        ) -> MmioCompareExchangeFuture {
            let value = self.value.clone();
            let current = <[u8; 4]>::try_from(current);
            let new = <[u8; 4]>::try_from(new);
            Box::pin(async move {
                if address == 0x104 {
                    return Ok(None);
                }
                if address == 0x108 {
                    return Err(IoError::NoResponse);
                }
                let (Ok(current), Ok(new)) = (current, new) else {
                    return Err(IoError::InvalidAccessSize);
                };
                Ok(Some(
                    value
                        .compare_exchange(
                            u32::from_ne_bytes(current),
                            u32::from_ne_bytes(new),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok(),
                ))
            })
        }
    }

    fn test_chipset(atomic: bool) -> (Chipset, Arc<AtomicU32>) {
        let value = Arc::new(AtomicU32::new(1));
        let device: Arc<CloseableMutex<dyn ChipsetDevice>> = if atomic {
            Arc::new(CloseableMutex::new(TestDevice {
                value: value.clone(),
            }))
        } else {
            Arc::new(CloseableMutex::new(LegacyDevice(value.clone())))
        };
        (
            Chipset {
                mmio_ranges: IoRanges::new(false, Some(device)),
                pio_ranges: IoRanges::new(false, None),
                pic: None,
                eoi_handler: None,
                debug_event_handler: Arc::new(DebugHandler),
            },
            value,
        )
    }

    #[async_test]
    async fn atomic_mmio_dispatch_preserves_comparison_and_errors() {
        let (chipset, value) = test_chipset(true);
        assert!(
            chipset
                .mmio_compare_exchange(0, 0x100, &1u32.to_ne_bytes(), &2u32.to_ne_bytes())
                .await
                .unwrap()
        );
        assert!(
            !chipset
                .mmio_compare_exchange(0, 0x100, &1u32.to_ne_bytes(), &3u32.to_ne_bytes())
                .await
                .unwrap()
        );
        assert_eq!(value.load(Ordering::SeqCst), 2);
        assert!(
            chipset
                .mmio_compare_exchange(0, 0x100, &[1, 2], &[3])
                .await
                .is_err()
        );
        assert_eq!(value.load(Ordering::SeqCst), 2);
        assert!(
            chipset
                .mmio_compare_exchange(0, 0x108, &2u32.to_ne_bytes(), &3u32.to_ne_bytes())
                .await
                .is_err()
        );
        assert_eq!(value.load(Ordering::SeqCst), 2);
        assert!(
            chipset
                .mmio_compare_exchange(0, 0x104, &0u32.to_ne_bytes(), &4u32.to_ne_bytes())
                .await
                .unwrap()
        );
        assert_eq!(value.load(Ordering::SeqCst), 4);
    }

    #[async_test]
    async fn legacy_mmio_compare_exchange_still_writes_unconditionally() {
        let (chipset, value) = test_chipset(false);
        assert!(
            chipset
                .mmio_compare_exchange(0, 0x100, &0u32.to_ne_bytes(), &7u32.to_ne_bytes())
                .await
                .unwrap()
        );
        assert_eq!(value.load(Ordering::SeqCst), 7);
        assert!(
            chipset
                .mmio_compare_exchange(0, 0x10c, &0u32.to_ne_bytes(), &8u32.to_ne_bytes())
                .await
                .unwrap()
        );
        assert_eq!(value.load(Ordering::SeqCst), 8);
    }
}
