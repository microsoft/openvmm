// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IO-APIC emulator.
//!
//! Currently this supports IO-APIC version 0x11, from the PIIX4 era. This
//! version does not support an EOI register (unlike version 0x20 and newer).

use self::spec::IO_APIC_VERSION;
use self::spec::IOAPIC_DEVICE_MMIO_REGION_SIZE;
use self::spec::IndexRegister;
use self::spec::IoApicId;
use self::spec::IoApicVersion;
use self::spec::REDIRECTION_WRITE_MASK;
use self::spec::RedirectionEntry;
use crate::ioapic::spec::IOAPIC_DEVICE_MMIO_REGION_MASK;
use crate::ioapic::spec::Register;
use chipset_device::ChipsetDevice;
use chipset_device::interrupt::HandleEoi;
use chipset_device::interrupt::LineInterruptTarget;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use std::fmt;
use std::fmt::Debug;
use std::ops::RangeInclusive;
use vmcore::device_state::ChangeDeviceState;
use x86defs::apic::DeliveryMode;
use x86defs::msi::MsiAddress;
use x86defs::msi::MsiData;

pub const IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS: u64 = 0xfec00000;

mod spec {
    use bitfield_struct::bitfield;
    use inspect::Inspect;
    use open_enum::open_enum;

    /// The version reported by the Intel 82093AA.
    pub const IO_APIC_VERSION: u8 = 0x11;

    open_enum! {
        pub enum Register: u64 {
            INDEX = 0,
            DATA = 0x10,
        }
    }

    open_enum! {
        #[derive(Inspect)]
        #[inspect(debug)]
        pub enum IndexRegister: u8 {
            ID = 0,
            VERSION = 1,
            ARBITRATION_ID = 2,
            REDIRECTION_TABLE_START = 0x10,
        }
    }

    pub const IOAPIC_DEVICE_MMIO_REGION_SIZE: u64 = 0x20;
    pub const IOAPIC_DEVICE_MMIO_REGION_MASK: u64 = IOAPIC_DEVICE_MMIO_REGION_SIZE - 1;

    #[derive(Inspect)]
    #[bitfield(u64)]
    #[rustfmt::skip]
    pub struct RedirectionEntry {
        #[bits(8)] pub vector: u8,
        #[bits(3)] pub delivery_mode: u8,
        #[bits(1)] pub destination_mode_logical: bool,
        #[bits(1)] pub delivery_status: bool,
        #[bits(1)] pub active_low: bool,
        #[bits(1)] pub remote_irr: bool,
        #[bits(1)] pub trigger_mode_level: bool,
        #[bits(1)] pub masked: bool,
        #[bits(15)] _unused: u32,
        #[bits(16)] _unused2: u32,
        #[bits(8)] pub extended_destination: u8,
        #[bits(8)] pub destination: u8,
    }

    /// The bits of the redirection entry that can be set by the guest.
    pub const REDIRECTION_WRITE_MASK: u64 = RedirectionEntry::new()
        .with_vector(0xff)
        .with_delivery_mode(0x7)
        .with_destination_mode_logical(true)
        .with_active_low(true)
        .with_trigger_mode_level(true)
        .with_masked(true)
        .with_destination(0xff)
        .with_extended_destination(0xff)
        .0;

    #[bitfield(u32)]
    pub struct IoApicId {
        #[bits(24)]
        _reserved: u32,
        #[bits(4)]
        pub id: u8,
        #[bits(4)]
        _reserved2: u32,
    }

    #[bitfield(u32)]
    pub struct IoApicVersion {
        pub version: u8,
        _reserved: u8,
        pub max_entry: u8,
        _reserved2: u8,
    }
}

impl RedirectionEntry {
    fn init() -> Self {
        Self::new()
            .with_destination_mode_logical(true)
            .with_masked(true)
    }

    /// Converts the entry to MSI format.
    fn as_msi(&self) -> Option<(u64, u32)> {
        if self.masked() {
            return None;
        }

        // Copy the redirection entry's bits to the MSI as defined in the Intel
        // ICHx datasheets.
        let address = MsiAddress::new()
            .with_address(x86defs::msi::MSI_ADDRESS)
            .with_destination(self.destination())
            .with_extended_destination(self.extended_destination())
            .with_destination_mode_logical(self.destination_mode_logical())
            .with_redirection_hint(self.delivery_mode() == DeliveryMode::LOWEST_PRIORITY.0);

        let data = MsiData::new()
            .with_assert(true)
            .with_destination_mode_logical(self.destination_mode_logical())
            .with_delivery_mode(self.delivery_mode())
            .with_trigger_mode_level(self.trigger_mode_level())
            .with_vector(self.vector());

        Some((u32::from(address).into(), data.into()))
    }
}

#[derive(Debug, Inspect)]
struct IrqEntry {
    #[inspect(flatten)]
    redirection: RedirectionEntry,
    line_level: bool,
    #[inspect(skip)]
    registered_request: Option<(u64, u32)>,
}

impl IrqEntry {
    fn assert(&mut self, routing: &dyn IoApicRouting, stats: &mut IoApicStats, n: u8) {
        let old_level = std::mem::replace(&mut self.line_level, true);
        self.evaluate(routing, stats, n, !old_level);
    }

    fn deassert(&mut self) {
        self.line_level = false;
        // No need to evaluate; this interrupt definitely doesn't need to be
        // delivered now.
    }

    fn eoi(&mut self, vector: u32, routing: &dyn IoApicRouting, stats: &mut IoApicStats, n: u8) {
        if self.redirection.vector() as u32 == vector {
            // Clear remote IRR to allow further interrupts (if level
            // triggered).
            self.redirection.set_remote_irr(false);
            self.evaluate(routing, stats, n, false);
        }
    }

    fn evaluate(
        &mut self,
        routing: &dyn IoApicRouting,
        stats: &mut IoApicStats,
        n: u8,
        edge: bool,
    ) {
        // Only some delivery modes support level trigger mode.
        let is_level = self.redirection.trigger_mode_level()
            && matches!(
                DeliveryMode(self.redirection.delivery_mode()),
                DeliveryMode::FIXED | DeliveryMode::LOWEST_PRIORITY
            );

        // Masked edge-triggered interrupts are lost. Masked level-triggered
        // interrupts are reevaluated when the interrupt is unmasked.
        if self.redirection.masked()
            || (is_level && (!self.line_level || self.redirection.remote_irr()))
            || (!is_level && !edge)
        {
            return;
        }

        // Remote IRR tracks whether a level-triggered interrupt has been EOIed yet.
        self.redirection.set_remote_irr(is_level);
        stats.interrupts.increment();
        stats.interrupts_per_irq[n as usize].increment();
        routing.assert(n);
    }
}

#[derive(Debug, InspectMut)]
pub struct IoApicDevice {
    // Static configuration
    #[inspect(skip)]
    valid_lines: [RangeInclusive<u32>; 1],

    // Runtime glue
    #[inspect(skip)]
    routing: Box<dyn IoApicRouting>,

    // Runtime book-keeping
    stats: IoApicStats,

    // Volatile state
    #[inspect(with = r#"|x| inspect::iter_by_index(x.iter())"#)]
    irqs: Box<[IrqEntry]>,
    id: u8,
    index: IndexRegister,
}

#[derive(Debug, Inspect)]
struct IoApicStats {
    #[inspect(iter_by_index)]
    interrupts_per_irq: Vec<Counter>,
    interrupts: Counter,
}

/// Trait allowing the IO-APIC device to assert VM interrupts.
pub trait IoApicRouting: Send + Sync {
    /// Asserts virtual interrupt line `irq`.
    fn assert(&self, irq: u8);
    /// Sets the MSI parameters to use when virtual interrupt line `irq` is
    /// asserted.
    fn set_route(&self, irq: u8, request: Option<(u64, u32)>);
}

impl Debug for dyn IoApicRouting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("IoApicRouting")
    }
}

impl IoApicDevice {
    pub fn new(num_entries: u8, routing: Box<dyn IoApicRouting>) -> Self {
        let irqs = (0..num_entries)
            .map(|_| IrqEntry {
                redirection: RedirectionEntry::init(),
                line_level: false,
                registered_request: None,
            })
            .collect();

        IoApicDevice {
            valid_lines: [0..=num_entries as u32 - 1],
            routing,

            id: 0,
            irqs,
            index: IndexRegister::ID,
            stats: IoApicStats {
                interrupts_per_irq: (0..num_entries).map(|_| Counter::new()).collect(),
                interrupts: Counter::new(),
            },
        }
    }

    fn read_redirection_register(&self, index: u8) -> u32 {
        let mut value = self
            .irqs
            .get(index as usize / 2)
            .map_or(0, |irq| irq.redirection.into());
        if index & 1 == 1 {
            value >>= 32;
        }
        value as u32
    }

    fn write_redirection_register(&mut self, index: u8, val: u32) {
        let n = index as usize / 2;
        if let Some(irq) = self.irqs.get_mut(n) {
            let mut val = val as u64;
            let mut redirection = u64::from(irq.redirection);
            if index & 1 == 1 {
                redirection &= 0xffffffff;
                val <<= 32;
            } else {
                redirection &= !0xffffffff;
            }
            redirection |= val & REDIRECTION_WRITE_MASK;
            irq.redirection = redirection.into();

            tracing::trace!(n, entry = ?irq.redirection, "new redirection entry");

            let request = irq.redirection.as_msi();
            if request != irq.registered_request {
                self.routing.set_route(n as u8, request);
                irq.registered_request = request;
            }

            // Reevaluate in case this unmasked a level-triggered interrupt.
            irq.evaluate(self.routing.as_ref(), &mut self.stats, n as u8, false);
        }
    }

    fn read_register(&self, index: IndexRegister) -> u32 {
        match index {
            IndexRegister::ID => IoApicId::new().with_id(self.id).into(),
            IndexRegister::VERSION => IoApicVersion::new()
                .with_version(IO_APIC_VERSION)
                .with_max_entry((self.irqs.len() - 1) as u8)
                .into(),
            IndexRegister::ARBITRATION_ID => 0,
            _ if self.index >= IndexRegister::REDIRECTION_TABLE_START => {
                self.read_redirection_register(index.0 - IndexRegister::REDIRECTION_TABLE_START.0)
            }
            _ => {
                tracelimit::warn_ratelimited!(?index, "unsupported register index read");
                !0
            }
        }
    }

    fn write_register(&mut self, index: IndexRegister, val: u32) {
        match index {
            IndexRegister::ID => self.id = IoApicId::from(val).id(),
            IndexRegister::VERSION | IndexRegister::ARBITRATION_ID => {
                tracing::debug!(?index, val, "ignoring write to read-only register");
            }
            _ if self.index >= IndexRegister::REDIRECTION_TABLE_START => {
                self.write_redirection_register(
                    index.0 - IndexRegister::REDIRECTION_TABLE_START.0,
                    val,
                );
            }
            _ => {
                tracelimit::warn_ratelimited!(?index, "unsupported register index write");
            }
        }
    }
}

impl LineInterruptTarget for IoApicDevice {
    fn set_irq(&mut self, n: u32, high: bool) {
        if let Some(irq) = self.irqs.get_mut(n as usize) {
            if high {
                irq.assert(self.routing.as_ref(), &mut self.stats, n as u8);
            } else {
                irq.deassert();
            }
        }
    }

    fn valid_lines(&self) -> &[RangeInclusive<u32>] {
        &self.valid_lines
    }
}

impl HandleEoi for IoApicDevice {
    /// reSearch query: `IoApicEmulator::NotifyEoi`
    fn handle_eoi(&mut self, irq_to_end: u32) {
        for (index, irq) in self.irqs.iter_mut().enumerate() {
            irq.eoi(
                irq_to_end,
                self.routing.as_ref(),
                &mut self.stats,
                index as u8,
            );
        }
    }
}

impl ChangeDeviceState for IoApicDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let Self {
            valid_lines: _,
            routing: _,
            irqs,
            id,
            index,
            stats: _,
        } = self;
        *id = 0;
        *index = IndexRegister::ID;
        for (n, irq) in irqs.iter_mut().enumerate() {
            irq.redirection = RedirectionEntry::init();
            self.routing.set_route(n as u8, None);
            irq.registered_request = None;
        }
    }
}

impl ChipsetDevice for IoApicDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_line_interrupt_target(&mut self) -> Option<&mut dyn LineInterruptTarget> {
        Some(self)
    }

    fn supports_handle_eoi(&mut self) -> Option<&mut dyn HandleEoi> {
        Some(self)
    }
}

mod save_restore {
    use super::IoApicDevice;
    use super::spec::IndexRegister;
    use super::spec::RedirectionEntry;
    use thiserror::Error;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Clone, Debug, Default, Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.ioapic")]
        pub struct SavedState {
            #[mesh(1)]
            pub(super) id: u8,
            #[mesh(2)]
            pub(super) index: u8,
            #[mesh(3)]
            pub(super) redirection_entries: Vec<u64>,
        }
    }

    #[derive(Error, Debug)]
    #[error("wrong number of redirection entries")]
    struct WrongNumberOfRedirectionEntries;

    impl SaveRestore for IoApicDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<state::SavedState, SaveError> {
            let Self {
                valid_lines: _,
                routing: _,
                irqs,
                id,
                index,
                stats: _,
            } = &self;

            Ok(state::SavedState {
                redirection_entries: irqs.iter().map(|irq| irq.redirection.into()).collect(),
                index: index.0,
                id: *id,
            })
        }

        fn restore(&mut self, state: state::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                redirection_entries,
                id,
                index,
            } = state;
            if redirection_entries.len() != self.irqs.len() {
                return Err(RestoreError::Other(WrongNumberOfRedirectionEntries.into()));
            }
            for (n, (state, irq)) in redirection_entries
                .into_iter()
                .zip(self.irqs.iter_mut())
                .enumerate()
            {
                irq.redirection = RedirectionEntry::from(state);
                let request = irq.redirection.as_msi();
                self.routing.set_route(n as u8, request);
                irq.registered_request = request;
            }
            self.id = id;
            self.index = IndexRegister(index);
            Ok(())
        }
    }
}

impl MmioIntercept for IoApicDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        assert_eq!(
            address & !IOAPIC_DEVICE_MMIO_REGION_MASK,
            IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS
        );

        let v = match Register(address & IOAPIC_DEVICE_MMIO_REGION_MASK) {
            Register::INDEX => self.index.0.into(),
            Register::DATA => self.read_register(self.index),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        // Allow any size read.
        let n = data.len().min(size_of_val(&v));
        data.copy_from_slice(&v.to_ne_bytes()[..n]);
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        assert_eq!(
            address & !IOAPIC_DEVICE_MMIO_REGION_MASK,
            IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS
        );

        match Register(address & IOAPIC_DEVICE_MMIO_REGION_MASK) {
            Register::INDEX => self.index = IndexRegister(data[0]),
            Register::DATA => {
                // Only allow 4-byte writes.
                let Ok(data) = data.try_into() else {
                    return IoResult::Err(IoError::InvalidAccessSize);
                };
                let data = u32::from_ne_bytes(data);
                self.write_register(self.index, data);
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        &[(
            "mmio",
            IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS
                ..=IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS + IOAPIC_DEVICE_MMIO_REGION_SIZE - 1,
        )]
    }
}
