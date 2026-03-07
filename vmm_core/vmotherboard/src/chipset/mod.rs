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
        let r = lookup
            .dev
            .lock()
            .supports_mmio()
            .expect("objects on the mmio bus support mmio")
            .mmio_read(address, data);

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
        let r = lookup
            .dev
            .lock()
            .supports_mmio()
            .expect("objects on the mmio bus support mmio")
            .mmio_write(address, data);

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
        let r = lookup
            .dev
            .lock()
            .supports_pio()
            .expect("objects on the pio bus support pio")
            .io_read(port, data);

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
        let r = lookup
            .dev
            .lock()
            .supports_pio()
            .expect("objects on the pio bus support pio")
            .io_write(port, data);

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
        }
    }
}
