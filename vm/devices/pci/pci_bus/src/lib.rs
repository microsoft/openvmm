// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generic PCI Bus infrastructure.
//!
//! [`GenericPciBus`] is a [`ChipsetDevice`] that implements a chipset and
//! architecture agnostic PCI bus.
//!
//! [`GenericPciBus`] can be configured to support various spec-compliant PCI
//! configuration space access mechanisms, such as legacy port-io based
//! configuration space access, ECAM (Enhanced Configuration Access Mechanism),
//! etc...
//!
//! Incoming config space accesses are then routed to connected
//! [`GenericPciBusDevice`] devices.

#![forbid(unsafe_code)]

use bitfield_struct::bitfield;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::PortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::Inspect;
use inspect::InspectMut;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Standard x86 IO ports associated with PCI
#[expect(missing_docs)] // self explanatory constants
pub mod standard_x86_io_ports {
    pub const ADDR_START: u16 = 0xCF8;
    pub const ADDR_END: u16 = 0xCFB;

    pub const DATA_START: u16 = 0xCFC;
    pub const DATA_END: u16 = 0xCFF;
}

/// An abstract interface for a PCI device accessed via the [`GenericPciBus`].
///
/// This trait is nearly identical to [`chipset_device::pci::PciConfigSpace`],
/// except for the fact that the return values are wrapped in an `Option`, where
/// `None` indicates that the backing device is no longer responding to
/// accesses.
///
/// e.g: a GenericPciBusDevice backed by a `Weak` pointer to a device could get
/// invalidated, in which case, these APIs would return `None`.
///
/// This trait decouples the PCI bus implementation from any concrete
/// `ChipsetDevice` ownership model being employed by upper-level code (i.e:
/// Arc/Weak + Mutex vs. Channels, etc...).
///
/// This is also the reason why the read/write methods are fallible: the PCI bus
/// should be resilient to backing devices unexpectedly going offline.
///
/// PCI devices can optionally implement routing functionality (like switches and bridges)
/// by providing implementations for the forwarding methods.
pub trait GenericPciBusDevice: 'static + Send {
    /// Dispatch a PCI config space read to the device with the given address.
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult>;

    /// Dispatch a PCI config space write to the device with the given address.
    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult>;

    /// Forward a PCI configuration space read to a downstream device.
    ///
    /// Default implementation returns `None`, indicating this device doesn't support routing.
    /// Routing components like switches and bridges should override this method.
    ///
    /// # Parameters
    /// - `bus`: Target bus number for the downstream device
    /// - `device_function`: Combined device and function number (device << 3 | function)
    /// - `offset`: Configuration space offset within the target device
    /// - `value`: Pointer to receive the read value
    ///
    /// # Returns
    /// `Some(IoResult)` if the routing component handled the forward, `None` if
    /// the component doesn't support routing or the target is not reachable.
    fn pci_cfg_read_forward(
        &mut self,
        _bus: u8,
        _device_function: u8,
        _offset: u16,
        _value: &mut u32,
    ) -> Option<IoResult> {
        None
    }

    /// Forward a PCI configuration space write to a downstream device.
    ///
    /// Default implementation returns `None`, indicating this device doesn't support routing.
    /// Routing components like switches and bridges should override this method.
    ///
    /// # Parameters
    /// - `bus`: Target bus number for the downstream device
    /// - `device_function`: Combined device and function number (device << 3 | function)
    /// - `offset`: Configuration space offset within the target device
    /// - `value`: Value to write to the target device
    ///
    /// # Returns
    /// `Some(IoResult)` if the routing component handled the forward, `None` if
    /// the component doesn't support routing or the target is not reachable.
    fn pci_cfg_write_forward(
        &mut self,
        _bus: u8,
        _device_function: u8,
        _offset: u16,
        _value: u32,
    ) -> Option<IoResult> {
        None
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Inspect)]
#[inspect(display)]
struct PciAddr {
    bus: u8,
    device: u8,
    function: u8,
}

impl std::fmt::Display for PciAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Use standard-ish BDF notation (bb:dd.f).
        write!(
            f,
            "{:02x}:{:02x}.{:x}",
            self.bus, self.device, self.function
        )
    }
}

#[derive(Inspect)]
struct GenericPciBusState {
    pio_addr_reg: AddressRegister,
}

// This type is effectively two hand-rolled state machines combined into one, as
// only one action can be taking place at a time.
//
// When a read is issued and deferred that results in a `DeferredAction::Read`,
// which will then be processed asynchronously.
//
// When a write is issued, if the write is undersized, we must first read the
// existing value on alignment before combining that with the  new value and
// writing it. That read could be deferred, which will result in a
// `DeferredAction::ReadForWrite`. If the write after this read is deferred
// it will result in a `DeferredAction::Write`.
//
// If a fully sized write is issued and gets deferred, that does not result in a
// `DeferredAction::Write`. Instead it is simply returned up the stack to let our
// caller handle it, as we don't need to perform any extra work after completion.
#[derive(Inspect)]
#[inspect(tag = "kind")]
enum DeferredAction {
    Read {
        #[inspect(skip)]
        deferred_device_read: DeferredToken,
        #[inspect(skip)]
        bus_read: DeferredRead,
        read_len: usize,
        io_port: u16,
        address: PciAddr,
    },
    ReadForWrite {
        #[inspect(skip)]
        deferred_device_read: DeferredToken,
        #[inspect(skip)]
        bus_write: DeferredWrite,
        write_len: usize,
        io_port: u16,
        new_value: u32,
        address: PciAddr,
    },
    Write {
        #[inspect(skip)]
        deferred_device_write: DeferredToken,
        #[inspect(skip)]
        bus_write: DeferredWrite,
        value: u32,
        address: PciAddr,
    },
}

/// A generic PCI bus.
#[derive(InspectMut)]
pub struct GenericPciBus {
    // Runtime glue
    pio_addr: Box<dyn ControlPortIoIntercept>,
    pio_data: Box<dyn ControlPortIoIntercept>,
    #[inspect(with = "|x| inspect::iter_by_key(x).map_value(|(name, _)| name)")]
    pci_devices: BTreeMap<PciAddr, (Arc<str>, Box<dyn GenericPciBusDevice>)>,

    // Async bookkeeping
    #[inspect(with = "|x| x.is_some()")]
    waker: Option<std::task::Waker>,
    #[inspect(iter_by_index)]
    deferred_actions: Vec<DeferredAction>,

    // Volatile state
    state: GenericPciBusState,
}

/// Error indicating that a PCI slot is already occupied.
#[derive(Debug, Error)]
#[error("PCI slot already occupied by device '{existing_device_name}'")]
pub struct PciSlotOccupiedError<D> {
    /// Name of the existing device occupying the slot.
    pub existing_device_name: Arc<str>,
    /// The device that was attempted to be added.
    pub device: D,
}

impl GenericPciBus {
    /// Create a new [`GenericPciBus`] with the specified (4-byte) IO ports.
    pub fn new(
        register_pio: &mut dyn RegisterPortIoIntercept,
        pio_addr: u16,
        pio_data: u16,
    ) -> GenericPciBus {
        let mut addr_control = register_pio.new_io_region("addr", 4);
        let mut data_control = register_pio.new_io_region("data", 4);
        addr_control.map(pio_addr);
        data_control.map(pio_data);
        GenericPciBus {
            pio_addr: addr_control,
            pio_data: data_control,
            pci_devices: BTreeMap::new(),

            waker: None,
            deferred_actions: Vec::new(),

            state: GenericPciBusState {
                pio_addr_reg: AddressRegister::new(),
            },
        }
    }

    /// Try to add a PCI device.
    pub fn add_pci_device<D: GenericPciBusDevice>(
        &mut self,
        bus: u8,
        device: u8,
        function: u8,
        name: impl AsRef<str>,
        dev: D,
    ) -> Result<(), PciSlotOccupiedError<D>> {
        let key = PciAddr {
            bus,
            device,
            function,
        };

        if let Some((name, _)) = self.pci_devices.get(&key) {
            return Err(PciSlotOccupiedError {
                existing_device_name: name.clone(),
                device: dev,
            });
        }

        self.pci_devices
            .insert(key, (name.as_ref().into(), Box::new(dev)));
        Ok(())
    }

    /// Handle a read from the ADDR register
    fn handle_addr_read(&self, value: &mut u32) -> IoResult {
        *value = self.state.pio_addr_reg.0;
        IoResult::Ok
    }

    /// Handle a write to the ADDR register
    fn handle_addr_write(&mut self, addr: u32) -> IoResult {
        let addr_fixup = {
            let mut addr = AddressRegister(addr);
            addr.fixup();
            addr
        };

        self.state.pio_addr_reg = addr_fixup;
        IoResult::Ok
    }

    /// Handle a read from the DATA register
    fn handle_data_read(&mut self, value: &mut u32) -> IoResult {
        tracing::trace!(%self.state.pio_addr_reg, "data read");

        if !self.state.pio_addr_reg.enabled() {
            tracelimit::warn_ratelimited!("addr enable bit is set to disabled");
            *value = !0;
            return IoResult::Ok;
        }

        let address = self.state.pio_addr_reg.address();

        match self.pci_devices.get_mut(&address) {
            Some((name, device)) => {
                let offset = self.state.pio_addr_reg.register().into();
                let res = device.pci_cfg_read(offset, value);
                if let Some(result) = res {
                    tracing::trace!(
                        device = &**name,
                        %address,
                        offset,
                        value,
                        "cfg space read"
                    );
                    result
                } else {
                    // TODO: should probably unregister from bus?
                    // but then again, shouldn't the device do that as part of
                    // its destructor?
                    tracelimit::warn_ratelimited!(
                        device = &**name,
                        %address,
                        offset,
                        "cfg space read failed, device went away"
                    );
                    *value = !0;
                    IoResult::Ok
                }
            }
            None => {
                tracing::trace!(%address, "no device found - returning F's");
                *value = !0;
                IoResult::Ok
            }
        }
    }

    /// Handler a write to the DATA register
    fn handle_data_write(&mut self, data: u32) -> IoResult {
        tracing::trace!(%self.state.pio_addr_reg, "data write");

        if !self.state.pio_addr_reg.enabled() {
            tracelimit::warn_ratelimited!("addr enable bit is set to disabled");
            return IoResult::Ok;
        }

        let address = self.state.pio_addr_reg.address();
        match self.pci_devices.get_mut(&address) {
            Some((name, device)) => {
                let offset = self.state.pio_addr_reg.register().into();
                let res = device.pci_cfg_write(offset, data);
                if let Some(result) = res {
                    tracing::trace!(
                        device = &**name,
                        %address,
                        offset,
                        data,
                        "cfg space write"
                    );
                    result
                } else {
                    // TODO: should probably unregister from bus?
                    // but then again, shouldn't the device do that as part of
                    // its destructor?
                    tracelimit::warn_ratelimited!(
                        device = &**name,
                        %address,
                        offset,
                        "cfg space write failed, device went away"
                    );
                    IoResult::Ok
                }
            }
            None => {
                tracing::debug!(%address, "no device found");
                IoResult::Ok
            }
        }
    }

    fn trace_error(&self, e: IoError, operation: &'static str) {
        let error = match e {
            IoError::InvalidRegister => "offset not supported",
            IoError::InvalidAccessSize => "invalid access size",
            IoError::UnalignedAccess => "unaligned access",
        };
        tracelimit::warn_ratelimited!(
            address = %self.state.pio_addr_reg.address(),
            offset = self.state.pio_addr_reg.register(),
            "pci config space {} operation error: {}",
            operation,
            error
        );
    }

    fn trace_recv_error(&self, e: mesh::RecvError, operation: &'static str) {
        tracelimit::warn_ratelimited!(
            address = %self.state.pio_addr_reg.address(),
            offset = self.state.pio_addr_reg.register(),
            "pci config space {} operation recv error: {:?}",
            operation,
            e,
        );
    }
}

impl ChangeDeviceState for GenericPciBus {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.state.pio_addr_reg = AddressRegister::new();
    }
}

impl ChipsetDevice for GenericPciBus {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

fn shift_read_value(io_port: u16, len: usize, value: u32) -> u32 {
    let shift = (io_port & 0x3) * 8;
    match len {
        4 => value,
        2 => value >> shift & 0xFFFF,
        1 => value >> shift & 0xFF,
        _ => unreachable!(),
    }
}

fn combine_old_new_values(io_port: u16, old_value: u32, new_value: u32, len: usize) -> u32 {
    let shift = (io_port & 0x3) * 8;
    let mask = (1 << (len * 8)) - 1;
    (old_value & !(mask << shift)) | (new_value << shift)
}

impl PortIoIntercept for GenericPciBus {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if !matches!(data.len(), 1 | 2 | 4) {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        if !(data.len() == 4 && io_port & 3 == 0
            || data.len() == 2 && io_port & 1 == 0
            || data.len() == 1)
        {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        let mut value = 0;
        let res = match io_port {
            _ if self.pio_addr.offset_of(io_port).is_some() => self.handle_addr_read(&mut value),
            _ if self.pio_data.offset_of(io_port).is_some() => self.handle_data_read(&mut value),
            _ => {
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        tracing::trace!(?io_port, ?res, ?data, "io port read");

        match res {
            IoResult::Ok => {
                let value = shift_read_value(io_port, data.len(), value);
                data.copy_from_slice(&value.as_bytes()[..data.len()]);
                IoResult::Ok
            }
            IoResult::Err(e) => {
                self.trace_error(e, "read");
                // Regardless of the pci error that occurred we return all zeros.
                // This is technically device-specific behavior, but it's what all
                // hyper-v devices do and it's worked for us so far.
                data.zero();
                IoResult::Ok
            }
            IoResult::Defer(deferred_device_read) => {
                let (bus_read, bus_token) = defer_read();
                self.deferred_actions.push(DeferredAction::Read {
                    deferred_device_read,
                    bus_read,
                    read_len: data.len(),
                    io_port,
                    address: self.state.pio_addr_reg.address(),
                });
                if let Some(waker) = self.waker.take() {
                    waker.wake();
                }
                IoResult::Defer(bus_token)
            }
        }
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        if !matches!(data.len(), 1 | 2 | 4) {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        let new_value = {
            let mut temp: u32 = 0;
            temp.as_mut_bytes()[..data.len()].copy_from_slice(data);
            temp
        };

        tracing::trace!(?io_port, data = ?new_value, "io port write");

        match io_port {
            _ if self.pio_addr.offset_of(io_port).is_some() => {
                // In theory, only 4-byte accesses are valid here, but
                // RedHat Linux modifies the bottom byte of the PCI
                // configuration address by using a 1-byte access
                let v = if data.len() == 4 {
                    new_value
                } else {
                    let mut old_value = 0;
                    self.handle_addr_read(&mut old_value).unwrap();
                    match data.len() {
                        2 => (old_value & 0xFFFF0000) | (new_value & 0xFFFF),
                        1 => (old_value & 0xFFFFFF00) | (new_value & 0xFF),
                        _ => unreachable!(),
                    }
                };

                self.handle_addr_write(v)
            }
            _ if self.pio_data.offset_of(io_port).is_some() => {
                let merged_value = if data.len() == 4 {
                    new_value
                } else {
                    // If the access isn't a double word, read in the old data
                    // to form a full word.
                    //
                    // Note that this isn't *really* correct, because reading
                    // bits may have a side-effect. Also, writing to bits that
                    // weren't actually written to may have side-effects...
                    //
                    // However, this technique appears to work fine for
                    // everything we've encountered so far ¯\_(ツ)_/¯
                    let mut old_value = 0;
                    match self.handle_data_read(&mut old_value) {
                        IoResult::Ok => {
                            combine_old_new_values(io_port, old_value, new_value, data.len())
                        }
                        IoResult::Err(e) => {
                            self.trace_error(e, "read for undersized write");
                            // Regardless of the pci error that occurred, we return all zeros.
                            // This is technically device-specific behavior, but it's what all
                            // hyper-v devices do and it's worked for us so far.
                            0
                        }
                        IoResult::Defer(deferred_device_read) => {
                            let (bus_write, bus_token) = defer_write();
                            self.deferred_actions.push(DeferredAction::ReadForWrite {
                                deferred_device_read,
                                bus_write,
                                write_len: data.len(),
                                io_port,
                                new_value,
                                address: self.state.pio_addr_reg.address(),
                            });
                            if let Some(waker) = self.waker.take() {
                                waker.wake();
                            }
                            return IoResult::Defer(bus_token);
                        }
                    }
                };

                let write_result = self.handle_data_write(merged_value);
                match write_result {
                    IoResult::Err(e) => {
                        self.trace_error(e, "write");
                        IoResult::Ok
                    }
                    IoResult::Ok | IoResult::Defer(_) => {
                        // If the write was successful we're all set.
                        // If the write is deferred we have no extra work to do after
                        // it resolves, unlike with read, so we can just return it and
                        // let the motherboard poll.
                        write_result
                    }
                }
            }
            _ => IoResult::Err(IoError::InvalidRegister),
        }
    }
}

impl PollDevice for GenericPciBus {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.waker = Some(cx.waker().clone());
        self.deferred_actions = std::mem::take(&mut self.deferred_actions)
            .into_iter()
            .filter_map(|action| match action {
                DeferredAction::Read {
                    mut deferred_device_read,
                    bus_read,
                    read_len,
                    io_port,
                    address,
                } => {
                    let mut buf = 0;
                    if let Poll::Ready(res) = deferred_device_read.poll_read(cx, buf.as_mut_bytes())
                    {
                        let value = match res {
                            Ok(()) => buf,
                            Err(e) => {
                                self.trace_recv_error(e, "deferred read");
                                0
                            }
                        };
                        let value = shift_read_value(io_port, read_len, value);
                        bus_read.complete(&value.as_bytes()[..read_len]);
                        None
                    } else {
                        Some(DeferredAction::Read {
                            deferred_device_read,
                            bus_read,
                            read_len,
                            io_port,
                            address,
                        })
                    }
                }
                DeferredAction::ReadForWrite {
                    mut deferred_device_read,
                    bus_write,
                    write_len,
                    io_port,
                    new_value,
                    address,
                } => {
                    let mut buf = 0;
                    if let Poll::Ready(res) = deferred_device_read.poll_read(cx, buf.as_mut_bytes())
                    {
                        let old_value = match res {
                            Ok(()) => buf,
                            Err(e) => {
                                self.trace_recv_error(e, "deferred read for write");
                                0
                            }
                        };
                        let merged_value =
                            combine_old_new_values(io_port, old_value, new_value, write_len);
                        match self.handle_data_write(merged_value) {
                            IoResult::Ok => {
                                bus_write.complete();
                                None
                            }
                            IoResult::Err(e) => {
                                self.trace_error(e, "write");
                                bus_write.complete();
                                None
                            }
                            IoResult::Defer(deferred_device_write) => {
                                cx.waker().wake_by_ref();
                                Some(DeferredAction::Write {
                                    deferred_device_write,
                                    bus_write,
                                    value: merged_value,
                                    address,
                                })
                            }
                        }
                    } else {
                        Some(DeferredAction::ReadForWrite {
                            deferred_device_read,
                            bus_write,
                            write_len,
                            io_port,
                            new_value,
                            address,
                        })
                    }
                }
                DeferredAction::Write {
                    mut deferred_device_write,
                    bus_write,
                    value,
                    address,
                } => {
                    if let Poll::Ready(res) = deferred_device_write.poll_write(cx) {
                        match res {
                            Ok(()) => {}
                            Err(e) => {
                                self.trace_recv_error(e, "deferred write");
                            }
                        }
                        bus_write.complete();
                        None
                    } else {
                        Some(DeferredAction::Write {
                            deferred_device_write,
                            bus_write,
                            value,
                            address,
                        })
                    }
                }
            })
            .collect();
    }
}

#[rustfmt::skip]
#[derive(Inspect)]
#[bitfield(u32)]
struct AddressRegister {
    #[bits(8)] register: u8,
    #[bits(3)] function: u8,
    #[bits(5)] device: u8,
    #[bits(8)] bus: u8,
    #[bits(7)] reserved: u8,
    #[bits(1)] enabled: bool,
}

impl AddressRegister {
    fn address(&self) -> PciAddr {
        PciAddr {
            bus: self.bus(),
            device: self.device(),
            function: self.function(),
        }
    }

    /// Set all reserved / zero bits to zero
    fn fixup(&mut self) {
        // the register accessed is always DWORD aligned
        // (the low two bits are hard-coded to 0)
        self.set_register(self.register() & !0b11);
        self.set_reserved(0);
    }
}

impl core::fmt::Display for AddressRegister {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{:04x}", self.address(), self.register())
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.bus")]
        pub struct SavedState {
            #[mesh(1)]
            pub pio_addr_reg: u32,
        }
    }

    #[derive(Debug, Error)]
    enum GenericPciBusRestoreError {
        #[error("saved address contained non-zero reserved bits")]
        AddressNonZeroReserved,
        #[error("saved address contained non-dword aligned register bits")]
        AddressNotDwordAligned,
    }

    impl SaveRestore for GenericPciBus {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let GenericPciBusState { pio_addr_reg } = self.state;

            let saved_state = state::SavedState {
                pio_addr_reg: pio_addr_reg.into(),
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { pio_addr_reg } = state;

            self.state = GenericPciBusState {
                pio_addr_reg: pio_addr_reg.into(),
            };

            // saved state sanity checks
            {
                if self.state.pio_addr_reg.reserved() != 0 {
                    return Err(RestoreError::InvalidSavedState(
                        GenericPciBusRestoreError::AddressNonZeroReserved.into(),
                    ));
                }

                if self.state.pio_addr_reg.register() & 0b11 != 0 {
                    return Err(RestoreError::InvalidSavedState(
                        GenericPciBusRestoreError::AddressNotDwordAligned.into(),
                    ));
                }
            }

            Ok(())
        }
    }
}
