// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers that implement standardized PCI configuration space functionality.
//!
//! To be clear: PCI devices are not required to use these helpers, and may
//! choose to implement configuration space accesses manually.

use crate::PciInterruptPin;
use crate::bar_mapping::BarMappings;
use crate::capabilities::PciCapability;
use crate::spec::cfg_space;
use crate::spec::hwid::HardwareIds;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use guestmem::MappableGuestMemory;
use inspect::Inspect;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use vmcore::line_interrupt::LineInterrupt;

const SUPPORTED_COMMAND_BITS: u16 = cfg_space::Command::new()
    .with_pio_enabled(true)
    .with_mmio_enabled(true)
    .with_bus_master(true)
    .with_special_cycles(true)
    .with_enable_memory_write_invalidate(true)
    .with_vga_palette_snoop(true)
    .with_parity_error_response(true)
    .with_enable_serr(true)
    .with_enable_fast_b2b(true)
    .with_intx_disable(true)
    .into_bits();

/// A wrapper around a [`LineInterrupt`] that considers PCI configuration space
/// interrupt control bits.
#[derive(Debug, Inspect)]
pub struct IntxInterrupt {
    pin: PciInterruptPin,
    line: LineInterrupt,
    interrupt_disabled: AtomicBool,
    interrupt_status: AtomicBool,
}

impl IntxInterrupt {
    /// Sets the line level high or low.
    ///
    /// NOTE: whether or not this will actually trigger an interrupt will depend
    /// the status of the Interrupt Disabled bit in the PCI configuration space.
    pub fn set_level(&self, high: bool) {
        tracing::debug!(
            disabled = ?self.interrupt_disabled,
            status = ?self.interrupt_status,
            ?high,
            %self.line,
            "set_level"
        );

        // the actual config space bit is set unconditionally
        self.interrupt_status.store(high, Ordering::SeqCst);

        // ...but whether it also fires an interrupt is a different story
        if self.interrupt_disabled.load(Ordering::SeqCst) {
            self.line.set_level(false);
        } else {
            self.line.set_level(high);
        }
    }

    fn set_disabled(&self, disabled: bool) {
        tracing::debug!(
            disabled = ?self.interrupt_disabled,
            status = ?self.interrupt_status,
            ?disabled,
            %self.line,
            "set_disabled"
        );

        self.interrupt_disabled.store(disabled, Ordering::SeqCst);
        if disabled {
            self.line.set_level(false)
        } else {
            if self.interrupt_status.load(Ordering::SeqCst) {
                self.line.set_level(true)
            }
        }
    }
}

#[derive(Debug, Inspect)]
struct ConfigSpaceType0EmulatorState {
    /// The command register
    command: cfg_space::Command,
    /// OS-configured BARs
    #[inspect(with = "inspect_helpers::bars")]
    base_addresses: [u32; 6],
    /// The PCI device doesn't actually care about what value is stored here -
    /// this register is just a bit of standardized "scratch space", ostensibly
    /// for firmware to communicate IRQ assignments to the OS, but it can really
    /// be used for just about anything.
    interrupt_line: u8,
    /// A read/write register that doesn't matter in virtualized contexts
    latency_timer: u8,
}

impl ConfigSpaceType0EmulatorState {
    fn new() -> Self {
        Self {
            latency_timer: 0,
            command: cfg_space::Command::new(),
            base_addresses: [0; 6],
            interrupt_line: 0,
        }
    }
}

/// Emulator for the standard Type 0 PCI configuration space header.
//
// TODO: split + share shared registers with other (yet unimplemented)
// header types
#[derive(Inspect)]
pub struct ConfigSpaceType0Emulator {
    // Fixed configuration
    #[inspect(with = "inspect_helpers::bars")]
    bar_masks: [u32; 6],
    hardware_ids: HardwareIds,
    multi_function_bit: bool,

    // Runtime glue
    #[inspect(with = r#"|x| inspect::iter_by_index(x).prefix("bar")"#)]
    mapped_memory: [Option<BarMemoryKind>; 6],
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|cap| (cap.label(), cap)))")]
    capabilities: Vec<Box<dyn PciCapability>>,
    intx_interrupt: Option<Arc<IntxInterrupt>>,

    // Runtime book-keeping
    active_bars: BarMappings,

    // Volatile state
    state: ConfigSpaceType0EmulatorState,
}

mod inspect_helpers {
    use super::*;

    pub(crate) fn bars(bars: &[u32; 6]) -> impl Inspect + '_ {
        inspect::AsHex(inspect::iter_by_index(bars).prefix("bar"))
    }
}

/// Different kinds of memory that a BAR can be backed by
#[derive(Inspect)]
#[inspect(tag = "kind")]
pub enum BarMemoryKind {
    /// BAR memory is routed to the device's `MmioIntercept` handler
    Intercept(#[inspect(rename = "handle")] Box<dyn ControlMmioIntercept>),
    /// BAR memory is routed to a shared memory region
    SharedMem(#[inspect(skip)] Box<dyn MappableGuestMemory>),
    /// **TESTING ONLY** BAR memory isn't backed by anything!
    Dummy,
}

impl std::fmt::Debug for BarMemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Intercept(control) => {
                write!(f, "Intercept(region_name: {}, ..)", control.region_name())
            }
            Self::SharedMem(_) => write!(f, "Mmap(..)"),
            Self::Dummy => write!(f, "Dummy"),
        }
    }
}

impl BarMemoryKind {
    fn map_to_guest(&mut self, gpa: u64) -> std::io::Result<()> {
        match self {
            BarMemoryKind::Intercept(control) => {
                control.map(gpa);
                Ok(())
            }
            BarMemoryKind::SharedMem(control) => control.map_to_guest(gpa, true),
            BarMemoryKind::Dummy => Ok(()),
        }
    }

    fn unmap_from_guest(&mut self) {
        match self {
            BarMemoryKind::Intercept(control) => control.unmap(),
            BarMemoryKind::SharedMem(control) => control.unmap_from_guest(),
            BarMemoryKind::Dummy => {}
        }
    }
}

/// Container type that describes a device's available BARs
// TODO: support more advanced BAR configurations
// e.g: mixed 32-bit and 64-bit
// e.g: IO space BARs
#[derive(Debug)]
pub struct DeviceBars {
    bars: [Option<(u64, BarMemoryKind)>; 6],
}

impl DeviceBars {
    /// Create a new instance of [`DeviceBars`]
    pub fn new() -> DeviceBars {
        DeviceBars {
            bars: Default::default(),
        }
    }

    /// Set BAR0
    pub fn bar0(mut self, len: u64, memory: BarMemoryKind) -> Self {
        self.bars[0] = Some((len, memory));
        self
    }

    /// Set BAR2
    pub fn bar2(mut self, len: u64, memory: BarMemoryKind) -> Self {
        self.bars[2] = Some((len, memory));
        self
    }

    /// Set BAR4
    pub fn bar4(mut self, len: u64, memory: BarMemoryKind) -> Self {
        self.bars[4] = Some((len, memory));
        self
    }
}

impl ConfigSpaceType0Emulator {
    /// Create a new [`ConfigSpaceType0Emulator`]
    pub fn new(
        hardware_ids: HardwareIds,
        capabilities: Vec<Box<dyn PciCapability>>,
        bars: DeviceBars,
    ) -> Self {
        let mut bar_masks = [0; 6];
        let mut mapped_memory = {
            const NONE: Option<BarMemoryKind> = None;
            [NONE; 6]
        };
        for (bar_index, bar) in bars.bars.into_iter().enumerate() {
            let (len, mapped) = match bar {
                Some(bar) => bar,
                None => continue,
            };
            // use 64-bit aware BARs
            assert!(bar_index < 5);
            // Round up regions to a power of 2, as required by PCI (and
            // inherently required by the BAR representation). Round up to at
            // least one page to avoid various problems in guest OSes.
            const MIN_BAR_SIZE: u64 = 4096;
            let len = std::cmp::max(len.next_power_of_two(), MIN_BAR_SIZE);
            let mask64 = !(len - 1);
            bar_masks[bar_index] = cfg_space::BarEncodingBits::from_bits(mask64 as u32)
                .with_type_64_bit(true)
                .into_bits();
            bar_masks[bar_index + 1] = (mask64 >> 32) as u32;
            mapped_memory[bar_index] = Some(mapped);
        }

        Self {
            bar_masks,
            hardware_ids,
            multi_function_bit: false,

            active_bars: Default::default(),

            mapped_memory,
            capabilities,
            intx_interrupt: None,

            state: ConfigSpaceType0EmulatorState {
                command: cfg_space::Command::new(),
                base_addresses: [0; 6],
                interrupt_line: 0,
                latency_timer: 0,
            },
        }
    }

    /// If the device is multi-function, enable bit 7 in the Header register.
    pub fn with_multi_function_bit(mut self, bit: bool) -> Self {
        self.multi_function_bit = bit;
        self
    }

    /// If using legacy INT#x interrupts: wire a LineInterrupt to one of the 4
    /// INT#x pins, returning an object that manages configuration space bits
    /// when the device sets the interrupt level.
    pub fn set_interrupt_pin(
        &mut self,
        pin: PciInterruptPin,
        line: LineInterrupt,
    ) -> Arc<IntxInterrupt> {
        let intx_interrupt = Arc::new(IntxInterrupt {
            pin,
            line,
            interrupt_disabled: AtomicBool::new(false),
            interrupt_status: AtomicBool::new(false),
        });
        self.intx_interrupt = Some(intx_interrupt.clone());
        intx_interrupt
    }

    /// Resets the configuration space state.
    pub fn reset(&mut self) {
        self.state = ConfigSpaceType0EmulatorState::new();

        self.sync_command_register(self.state.command);

        for cap in &mut self.capabilities {
            cap.reset();
        }

        if let Some(intx) = &mut self.intx_interrupt {
            intx.set_level(false);
        }
    }

    fn get_capability_index_and_offset(&self, offset: u16) -> Option<(usize, u16)> {
        let mut cap_offset = 0;
        for i in 0..self.capabilities.len() {
            let cap_size = self.capabilities[i].len() as u16;
            if offset < cap_offset + cap_size {
                return Some((i, offset - cap_offset));
            }
            cap_offset += cap_size;
        }
        None
    }

    /// Read from the config space. `offset` must be 32-bit aligned.
    pub fn read_u32(&self, offset: u16, value: &mut u32) -> IoResult {
        use cfg_space::HeaderType00;

        *value = match HeaderType00(offset) {
            HeaderType00::DEVICE_VENDOR => {
                (self.hardware_ids.device_id as u32) << 16 | self.hardware_ids.vendor_id as u32
            }
            HeaderType00::STATUS_COMMAND => {
                let mut status =
                    cfg_space::Status::new().with_capabilities_list(!self.capabilities.is_empty());

                if let Some(intx_interrupt) = &self.intx_interrupt {
                    if intx_interrupt.interrupt_status.load(Ordering::SeqCst) {
                        status.set_interrupt_status(true);
                    }
                }

                (status.into_bits() as u32) << 16 | self.state.command.into_bits() as u32
            }
            HeaderType00::CLASS_REVISION => {
                (u8::from(self.hardware_ids.base_class) as u32) << 24
                    | (u8::from(self.hardware_ids.sub_class) as u32) << 16
                    | (u8::from(self.hardware_ids.prog_if) as u32) << 8
                    | self.hardware_ids.revision_id as u32
            }
            HeaderType00::BIST_HEADER => {
                let mut v = (self.state.latency_timer as u32) << 8;
                if self.multi_function_bit {
                    // enable top-most bit of the header register
                    v |= 0x80 << 16;
                }
                v
            }
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                self.state.base_addresses[(offset - HeaderType00::BAR0.0) as usize / 4]
            }
            HeaderType00::CARDBUS_CIS_PTR => 0,
            HeaderType00::SUBSYSTEM_ID => {
                (self.hardware_ids.type0_sub_system_id as u32) << 16
                    | self.hardware_ids.type0_sub_vendor_id as u32
            }
            HeaderType00::EXPANSION_ROM_BASE => 0,
            HeaderType00::RESERVED_CAP_PTR => {
                if self.capabilities.is_empty() {
                    0
                } else {
                    0x40
                }
            }
            HeaderType00::RESERVED => 0,
            HeaderType00::LATENCY_INTERRUPT => {
                let interrupt_pin = if let Some(intx_interrupt) = &self.intx_interrupt {
                    match intx_interrupt.pin {
                        PciInterruptPin::IntA => 1,
                        PciInterruptPin::IntB => 2,
                        PciInterruptPin::IntC => 3,
                        PciInterruptPin::IntD => 4,
                    }
                } else {
                    0
                };
                self.state.interrupt_line as u32 | (interrupt_pin as u32) << 8
            }
            // rest of the range is reserved for extended device capabilities
            _ if (0x40..0x100).contains(&offset) => {
                if let Some((cap_index, cap_offset)) =
                    self.get_capability_index_and_offset(offset - 0x40)
                {
                    let mut value = self.capabilities[cap_index].read_u32(cap_offset);
                    if cap_offset == 0 {
                        let next = if cap_index < self.capabilities.len() - 1 {
                            offset as u32 + self.capabilities[cap_index].len() as u32
                        } else {
                            0
                        };
                        assert!(value & 0xff00 == 0);
                        value |= next << 8;
                    }
                    value
                } else {
                    tracelimit::warn_ratelimited!(offset, "unhandled config space read");
                    return IoResult::Err(IoError::InvalidRegister);
                }
            }
            _ if (0x100..0x1000).contains(&offset) => {
                // TODO: properly support extended pci express configuration space
                if offset == 0x100 {
                    tracelimit::warn_ratelimited!(offset, "unexpected pci express probe");
                    0x000ffff
                } else {
                    tracelimit::warn_ratelimited!(offset, "unhandled extended config space read");
                    return IoResult::Err(IoError::InvalidRegister);
                }
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, "unexpected config space read");
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        IoResult::Ok
    }

    fn update_intx_disable(&mut self, command: cfg_space::Command) {
        if let Some(intx_interrupt) = &self.intx_interrupt {
            intx_interrupt.set_disabled(command.intx_disable())
        }
    }

    fn update_mmio_enabled(&mut self, command: cfg_space::Command) {
        if command.mmio_enabled() {
            self.active_bars = BarMappings::parse(&self.state.base_addresses, &self.bar_masks);
            for (bar, mapping) in self.mapped_memory.iter_mut().enumerate() {
                if let Some(mapping) = mapping {
                    let base = self.active_bars.get(bar as u8).expect("bar exists");
                    match mapping.map_to_guest(base) {
                        Ok(_) => {}
                        Err(err) => {
                            tracelimit::error_ratelimited!(
                                error = &err as &dyn std::error::Error,
                                bar,
                                base,
                                "failed to map bar",
                            )
                        }
                    }
                }
            }
        } else {
            self.active_bars = Default::default();
            for mapping in self.mapped_memory.iter_mut().flatten() {
                mapping.unmap_from_guest();
            }
        }
    }

    fn sync_command_register(&mut self, command: cfg_space::Command) {
        self.update_intx_disable(command);
        self.update_mmio_enabled(command);
    }

    /// Write to the config space. `offset` must be 32-bit aligned.
    pub fn write_u32(&mut self, offset: u16, val: u32) -> IoResult {
        use cfg_space::HeaderType00;

        match HeaderType00(offset) {
            HeaderType00::STATUS_COMMAND => {
                let mut command = cfg_space::Command::from_bits(val as u16);
                if command.into_bits() & !SUPPORTED_COMMAND_BITS != 0 {
                    tracelimit::warn_ratelimited!(offset, val, "setting invalid command bits");
                    // still do our best
                    command =
                        cfg_space::Command::from_bits(command.into_bits() & SUPPORTED_COMMAND_BITS);
                };

                if self.state.command.intx_disable() != command.intx_disable() {
                    self.update_intx_disable(command)
                }

                if self.state.command.mmio_enabled() != command.mmio_enabled() {
                    self.update_mmio_enabled(command)
                }

                self.state.command = command;
            }
            HeaderType00::BIST_HEADER => {
                // allow writes to the latency timer
                let timer_val = (val >> 8) as u8;
                self.state.latency_timer = timer_val;
            }
            HeaderType00::BAR0
            | HeaderType00::BAR1
            | HeaderType00::BAR2
            | HeaderType00::BAR3
            | HeaderType00::BAR4
            | HeaderType00::BAR5 => {
                if !self.state.command.mmio_enabled() {
                    let bar_index = (offset - HeaderType00::BAR0.0) as usize / 4;
                    let mut bar_value = val & self.bar_masks[bar_index];
                    if bar_index & 1 == 0 && self.bar_masks[bar_index] != 0 {
                        bar_value = cfg_space::BarEncodingBits::from_bits(bar_value)
                            .with_type_64_bit(true)
                            .into_bits();
                    }
                    self.state.base_addresses[bar_index] = bar_value;
                }
            }
            HeaderType00::LATENCY_INTERRUPT => {
                self.state.interrupt_line = ((val & 0xff00) >> 8) as u8;
            }
            // all other base regs are noops
            _ if offset < 0x40 && offset % 4 == 0 => (),
            // rest of the range is reserved for extended device capabilities
            _ if (0x40..0x100).contains(&offset) => {
                if let Some((cap_index, cap_offset)) =
                    self.get_capability_index_and_offset(offset - 0x40)
                {
                    self.capabilities[cap_index].write_u32(cap_offset, val);
                } else {
                    tracelimit::warn_ratelimited!(
                        offset,
                        value = val,
                        "unhandled config space write"
                    );
                    return IoResult::Err(IoError::InvalidRegister);
                }
            }
            _ if (0x100..0x1000).contains(&offset) => {
                // TODO: properly support extended pci express configuration space
                tracelimit::warn_ratelimited!(
                    offset,
                    value = val,
                    "unhandled extended config space write"
                );
                return IoResult::Err(IoError::InvalidRegister);
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, value = val, "unexpected config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    /// Finds a BAR + offset by address.
    pub fn find_bar(&self, address: u64) -> Option<(u8, u16)> {
        self.active_bars.find(address)
    }
}

mod save_restore {
    use super::*;
    use thiserror::Error;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateBlob;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.cfg_space_emu")]
        pub struct SavedState {
            #[mesh(1)]
            pub command: u16,
            #[mesh(2)]
            pub base_addresses: [u32; 6],
            #[mesh(3)]
            pub interrupt_line: u8,
            #[mesh(4)]
            pub latency_timer: u8,
            #[mesh(5)]
            pub capabilities: Vec<(String, SavedStateBlob)>,
        }
    }

    #[derive(Debug, Error)]
    enum ConfigSpaceRestoreError {
        #[error("found invalid config bits in saved state")]
        InvalidConfigBits,
        #[error("found unexpected capability {0}")]
        InvalidCap(String),
    }

    impl SaveRestore for ConfigSpaceType0Emulator {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let ConfigSpaceType0EmulatorState {
                command,
                base_addresses,
                interrupt_line,
                latency_timer,
            } = self.state;

            let saved_state = state::SavedState {
                command: command.into_bits(),
                base_addresses,
                interrupt_line,
                latency_timer,
                capabilities: self
                    .capabilities
                    .iter_mut()
                    .map(|cap| {
                        let id = cap.label().to_owned();
                        Ok((id, cap.save()?))
                    })
                    .collect::<Result<_, _>>()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                command,
                base_addresses,
                interrupt_line,
                latency_timer,
                capabilities,
            } = state;

            self.state = ConfigSpaceType0EmulatorState {
                command: cfg_space::Command::from_bits(command),
                base_addresses,
                interrupt_line,
                latency_timer,
            };

            if command & !SUPPORTED_COMMAND_BITS != 0 {
                return Err(RestoreError::InvalidSavedState(
                    ConfigSpaceRestoreError::InvalidConfigBits.into(),
                ));
            }

            self.sync_command_register(self.state.command);
            for (id, entry) in capabilities {
                tracing::debug!(save_id = id.as_str(), "restoring pci capability");

                // yes, yes, this is O(n^2), but devices never have more than a
                // handful of caps, so it's totally fine.
                let mut restored = false;
                for cap in self.capabilities.iter_mut() {
                    if cap.label() == id {
                        cap.restore(entry)?;
                        restored = true;
                        break;
                    }
                }

                if !restored {
                    return Err(RestoreError::InvalidSavedState(
                        ConfigSpaceRestoreError::InvalidCap(id).into(),
                    ));
                }
            }

            Ok(())
        }
    }
}
