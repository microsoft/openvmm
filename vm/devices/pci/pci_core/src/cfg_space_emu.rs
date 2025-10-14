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
use std::ops::RangeInclusive;
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
// TODO: Figure out how to split this up and share the handling of common
// registers (hardware IDs, command, status, etc.) with the type 1 emulator.
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
            _ if offset < 0x40 && offset.is_multiple_of(4) => (),
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

#[derive(Debug, Inspect)]
struct ConfigSpaceType1EmulatorState {
    /// The command register
    command: cfg_space::Command,
    /// The subordinate bus number register. Software programs
    /// this register with the highest bus number below the bridge.
    subordinate_bus_number: u8,
    /// The secondary bus number register. Software programs
    /// this register with the bus number assigned to the secondary
    /// side of the bridge.
    secondary_bus_number: u8,
    /// The primary bus number register. This is unused for PCI Express but
    /// is supposed to be read/write for compability with legacy software.
    primary_bus_number: u8,
    /// The memory base register. Software programs the upper 12 bits of this
    /// register with the upper 12 bits of a 32-bit base address of MMIO assigned
    /// to the hierarchy under the bridge (the lower 20 bits are assumed to be 0s).
    memory_base: u16,
    /// The memory limit register. Software programs the upper 12 bits of this
    /// register with the upper 12 bits of a 32-bit limit address of MMIO assigned
    /// to the hierarchy under the bridge (the lower 20 bits are assumed to be 1s).
    memory_limit: u16,
    /// The prefetchable memory base register. Software programs the upper 12 bits of
    /// this register with bits 20:31 of the base address of the prefetchable MMIO
    /// window assigned to the hierarchy under the bridge. Bits 0:19 are assumed to
    /// be 0s.
    prefetch_base: u16,
    /// The prefetchable memory limit register. Software programs the upper 12 bits of
    /// this register with bits 20:31 of the limit address of the prefetchable MMIO
    /// window assigned to the hierarchy under the bridge. Bits 0:19 are assumed to
    /// be 1s.
    prefetch_limit: u16,
    /// The prefetchable memory base upper 32 bits register. When the bridge supports
    /// 64-bit addressing for prefetchable memory, software programs this register
    /// with the upper 32 bits of the base address of the prefetchable MMIO window
    /// assigned to the hierarchy under the bridge.
    prefetch_base_upper: u32,
    /// The prefetchable memory limit upper 32 bits register. When the bridge supports
    /// 64-bit addressing for prefetchable memory, software programs this register
    /// with the upper 32 bits of the base address of the prefetchable MMIO window
    /// assigned to the hierarchy under the bridge.
    prefetch_limit_upper: u32,
}

impl ConfigSpaceType1EmulatorState {
    fn new() -> Self {
        Self {
            command: cfg_space::Command::new(),
            subordinate_bus_number: 0,
            secondary_bus_number: 0,
            primary_bus_number: 0,
            memory_base: 0,
            memory_limit: 0,
            prefetch_base: 0,
            prefetch_limit: 0,
            prefetch_base_upper: 0,
            prefetch_limit_upper: 0,
        }
    }
}

/// Emulator for the standard Type 1 PCI configuration space header.
//
// TODO: Figure out how to split this up and share the handling of common
// registers (hardware IDs, command, status, etc.) with the type 0 emulator.
// TODO: Support type 1 BARs (only two)
#[derive(Inspect)]
pub struct ConfigSpaceType1Emulator {
    hardware_ids: HardwareIds,
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|cap| (cap.label(), cap)))")]
    capabilities: Vec<Box<dyn PciCapability>>,
    state: ConfigSpaceType1EmulatorState,
}

impl ConfigSpaceType1Emulator {
    /// Create a new [`ConfigSpaceType1Emulator`]
    pub fn new(hardware_ids: HardwareIds, capabilities: Vec<Box<dyn PciCapability>>) -> Self {
        Self {
            hardware_ids,
            capabilities,
            state: ConfigSpaceType1EmulatorState::new(),
        }
    }

    /// Resets the configuration space state.
    pub fn reset(&mut self) {
        self.state = ConfigSpaceType1EmulatorState::new();

        for cap in &mut self.capabilities {
            cap.reset();
        }
    }

    /// Returns the range of bus numbers the bridge is programmed to decode.
    pub fn assigned_bus_range(&self) -> RangeInclusive<u8> {
        let secondary = self.state.secondary_bus_number;
        let subordinate = self.state.subordinate_bus_number;
        if secondary <= subordinate {
            secondary..=subordinate
        } else {
            0..=0
        }
    }

    fn decode_memory_range(&self, base_register: u16, limit_register: u16) -> (u32, u32) {
        let base_addr = ((base_register & !0b1111) as u32) << 16;
        let limit_addr = ((limit_register & !0b1111) as u32) << 16 | 0xF_FFFF;
        (base_addr, limit_addr)
    }

    /// If memory decoding is currently enabled, and the memory window assignment is valid,
    /// returns the 32-bit memory addresses the bridge is programmed to decode.
    pub fn assigned_memory_range(&self) -> Option<RangeInclusive<u32>> {
        let (base_addr, limit_addr) =
            self.decode_memory_range(self.state.memory_base, self.state.memory_limit);
        if self.state.command.mmio_enabled() && base_addr <= limit_addr {
            Some(base_addr..=limit_addr)
        } else {
            None
        }
    }

    /// If memory decoding is currently enabled, and the prefetchable memory window assignment
    /// is valid, returns the 64-bit prefetchable memory addresses the bridge is programmed to decode.
    pub fn assigned_prefetch_range(&self) -> Option<RangeInclusive<u64>> {
        let (base_low, limit_low) =
            self.decode_memory_range(self.state.prefetch_base, self.state.prefetch_limit);
        let base_addr = (self.state.prefetch_base_upper as u64) << 32 | base_low as u64;
        let limit_addr = (self.state.prefetch_limit_upper as u64) << 32 | limit_low as u64;
        if self.state.command.mmio_enabled() && base_addr <= limit_addr {
            Some(base_addr..=limit_addr)
        } else {
            None
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
        use cfg_space::HeaderType01;

        *value = match HeaderType01(offset) {
            HeaderType01::DEVICE_VENDOR => {
                (self.hardware_ids.device_id as u32) << 16 | self.hardware_ids.vendor_id as u32
            }
            HeaderType01::STATUS_COMMAND => {
                let status =
                    cfg_space::Status::new().with_capabilities_list(!self.capabilities.is_empty());

                (status.into_bits() as u32) << 16 | self.state.command.into_bits() as u32
            }
            HeaderType01::CLASS_REVISION => {
                (u8::from(self.hardware_ids.base_class) as u32) << 24
                    | (u8::from(self.hardware_ids.sub_class) as u32) << 16
                    | (u8::from(self.hardware_ids.prog_if) as u32) << 8
                    | self.hardware_ids.revision_id as u32
            }
            HeaderType01::BIST_HEADER => {
                // Header type 01
                0x00010000
            }
            HeaderType01::BAR0 => 0,
            HeaderType01::BAR1 => 0,
            HeaderType01::LATENCY_BUS_NUMBERS => {
                (self.state.subordinate_bus_number as u32) << 16
                    | (self.state.secondary_bus_number as u32) << 8
                    | self.state.primary_bus_number as u32
            }
            HeaderType01::SEC_STATUS_IO_RANGE => 0,
            HeaderType01::MEMORY_RANGE => {
                (self.state.memory_limit as u32) << 16 | self.state.memory_base as u32
            }
            HeaderType01::PREFETCH_RANGE => {
                // Set the low bit in both the limit and base registers to indicate
                // support for 64-bit addressing.
                ((self.state.prefetch_limit | 0b0001) as u32) << 16
                    | (self.state.prefetch_base | 0b0001) as u32
            }
            HeaderType01::PREFETCH_BASE_UPPER => self.state.prefetch_base_upper,
            HeaderType01::PREFETCH_LIMIT_UPPER => self.state.prefetch_limit_upper,
            HeaderType01::IO_RANGE_UPPER => 0,
            HeaderType01::RESERVED_CAP_PTR => {
                if self.capabilities.is_empty() {
                    0
                } else {
                    0x40
                }
            }
            HeaderType01::EXPANSION_ROM_BASE => 0,
            HeaderType01::BRDIGE_CTRL_INTERRUPT => 0,
            // rest of the range is reserved for device capabilities
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

    /// Write to the config space. `offset` must be 32-bit aligned.
    pub fn write_u32(&mut self, offset: u16, val: u32) -> IoResult {
        use cfg_space::HeaderType01;

        match HeaderType01(offset) {
            HeaderType01::STATUS_COMMAND => {
                let mut command = cfg_space::Command::from_bits(val as u16);
                if command.into_bits() & !SUPPORTED_COMMAND_BITS != 0 {
                    tracelimit::warn_ratelimited!(offset, val, "setting invalid command bits");
                    // still do our best
                    command =
                        cfg_space::Command::from_bits(command.into_bits() & SUPPORTED_COMMAND_BITS);
                };

                // TODO: when the memory space enable bit is written, sanity check the programmed
                // memory and prefetch ranges...

                self.state.command = command;
            }
            HeaderType01::LATENCY_BUS_NUMBERS => {
                self.state.subordinate_bus_number = (val >> 16) as u8;
                self.state.secondary_bus_number = (val >> 8) as u8;
                self.state.primary_bus_number = val as u8;
            }
            HeaderType01::MEMORY_RANGE => {
                self.state.memory_base = val as u16;
                self.state.memory_limit = (val >> 16) as u16;
            }
            HeaderType01::PREFETCH_RANGE => {
                self.state.prefetch_base = val as u16;
                self.state.prefetch_limit = (val >> 16) as u16;
            }
            HeaderType01::PREFETCH_BASE_UPPER => {
                self.state.prefetch_base_upper = val;
            }
            HeaderType01::PREFETCH_LIMIT_UPPER => {
                self.state.prefetch_limit_upper = val;
            }
            // all other base regs are noops
            _ if offset < 0x40 && offset.is_multiple_of(4) => (),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::read_only::ReadOnlyCapability;
    use crate::spec::hwid::ClassCode;
    use crate::spec::hwid::ProgrammingInterface;
    use crate::spec::hwid::Subclass;

    fn create_type1_emulator(caps: Vec<Box<dyn PciCapability>>) -> ConfigSpaceType1Emulator {
        ConfigSpaceType1Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::BRIDGE_PCI_TO_PCI,
                base_class: ClassCode::BRIDGE,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            caps,
        )
    }

    fn read_cfg(emulator: &ConfigSpaceType1Emulator, offset: u16) -> u32 {
        let mut val = 0;
        emulator.read_u32(offset, &mut val).unwrap();
        val
    }

    #[test]
    fn test_type1_probe() {
        let emu = create_type1_emulator(vec![]);
        assert_eq!(read_cfg(&emu, 0), 0x2222_1111);
        assert_eq!(read_cfg(&emu, 4) & 0x10_0000, 0); // Capabilities pointer

        let emu = create_type1_emulator(vec![Box::new(ReadOnlyCapability::new("foo", 0))]);
        assert_eq!(read_cfg(&emu, 0), 0x2222_1111);
        assert_eq!(read_cfg(&emu, 4) & 0x10_0000, 0x10_0000); // Capabilities pointer
    }

    #[test]
    fn test_type1_bus_number_assignment() {
        let mut emu = create_type1_emulator(vec![]);

        // The bus number (and latency timer) registers are
        // all default 0.
        assert_eq!(read_cfg(&emu, 0x18), 0);
        assert_eq!(emu.assigned_bus_range(), 0..=0);

        // The bus numbers can be programmed one by one,
        // and the range may not be valid during the middle
        // of allocation.
        emu.write_u32(0x18, 0x0000_1000).unwrap();
        assert_eq!(read_cfg(&emu, 0x18), 0x0000_1000);
        assert_eq!(emu.assigned_bus_range(), 0..=0);
        emu.write_u32(0x18, 0x0012_1000).unwrap();
        assert_eq!(read_cfg(&emu, 0x18), 0x0012_1000);
        assert_eq!(emu.assigned_bus_range(), 0x10..=0x12);

        // The primary bus number register is read/write for compatability
        // but unused.
        emu.write_u32(0x18, 0x0012_1033).unwrap();
        assert_eq!(read_cfg(&emu, 0x18), 0x0012_1033);
        assert_eq!(emu.assigned_bus_range(), 0x10..=0x12);

        // Software can also just write the entire 4byte value at once
        emu.write_u32(0x18, 0x0047_4411).unwrap();
        assert_eq!(read_cfg(&emu, 0x18), 0x0047_4411);
        assert_eq!(emu.assigned_bus_range(), 0x44..=0x47);

        // The subordinate bus number can equal the secondary bus number...
        emu.write_u32(0x18, 0x0088_8800).unwrap();
        assert_eq!(emu.assigned_bus_range(), 0x88..=0x88);

        // ... but it cannot be less, that's a confused guest OS.
        emu.write_u32(0x18, 0x0087_8800).unwrap();
        assert_eq!(emu.assigned_bus_range(), 0..=0);
    }

    #[test]
    fn test_type1_memory_assignment() {
        const MMIO_ENABLED: u32 = 0x0000_0002;
        const MMIO_DISABLED: u32 = 0x0000_0000;

        let mut emu = create_type1_emulator(vec![]);
        assert!(emu.assigned_memory_range().is_none());

        // The guest can write whatever it wants while MMIO
        // is disabled.
        emu.write_u32(0x20, 0xDEAD_BEEF).unwrap();
        assert!(emu.assigned_memory_range().is_none());

        // The guest can program a valid resource assignment...
        emu.write_u32(0x20, 0xFFF0_FF00).unwrap();
        assert!(emu.assigned_memory_range().is_none());
        // ... enable memory decoding...
        emu.write_u32(0x4, MMIO_ENABLED).unwrap();
        assert_eq!(emu.assigned_memory_range(), Some(0xFF00_0000..=0xFFFF_FFFF));
        // ... then disable memory decoding it.
        emu.write_u32(0x4, MMIO_DISABLED).unwrap();
        assert!(emu.assigned_memory_range().is_none());

        // Setting memory base equal to memory limit is a valid 1MB range.
        emu.write_u32(0x20, 0xBBB0_BBB0).unwrap();
        emu.write_u32(0x4, MMIO_ENABLED).unwrap();
        assert_eq!(emu.assigned_memory_range(), Some(0xBBB0_0000..=0xBBBF_FFFF));
        emu.write_u32(0x4, MMIO_DISABLED).unwrap();
        assert!(emu.assigned_memory_range().is_none());

        // The guest can try to program an invalid assignment (base > limit), we
        // just won't decode it.
        emu.write_u32(0x20, 0xAA00_BB00).unwrap();
        assert!(emu.assigned_memory_range().is_none());
        emu.write_u32(0x4, MMIO_ENABLED).unwrap();
        assert!(emu.assigned_memory_range().is_none());
        emu.write_u32(0x4, MMIO_DISABLED).unwrap();
        assert!(emu.assigned_memory_range().is_none());
    }

    #[test]
    fn test_type1_prefetch_assignment() {
        const MMIO_ENABLED: u32 = 0x0000_0002;
        const MMIO_DISABLED: u32 = 0x0000_0000;

        let mut emu = create_type1_emulator(vec![]);
        assert!(emu.assigned_prefetch_range().is_none());

        // The guest can program a valid prefetch range...
        emu.write_u32(0x24, 0xFFF0_FF00).unwrap(); // limit + base
        emu.write_u32(0x28, 0x00AA_BBCC).unwrap(); // base upper
        emu.write_u32(0x2C, 0x00DD_EEFF).unwrap(); // limit upper
        assert!(emu.assigned_prefetch_range().is_none());
        // ... enable memory decoding...
        emu.write_u32(0x4, MMIO_ENABLED).unwrap();
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x00AA_BBCC_FF00_0000..=0x00DD_EEFF_FFFF_FFFF)
        );
        // ... then disable memory decoding it.
        emu.write_u32(0x4, MMIO_DISABLED).unwrap();
        assert!(emu.assigned_prefetch_range().is_none());

        // The validity of the assignment is determined using the combined 64-bit
        // address, not the lower bits or the upper bits in isolation.

        // Lower bits of the limit are greater than the lower bits of the
        // base, but the upper bits make that valid.
        emu.write_u32(0x24, 0xFF00_FFF0).unwrap(); // limit + base
        emu.write_u32(0x28, 0x00AA_BBCC).unwrap(); // base upper
        emu.write_u32(0x2C, 0x00DD_EEFF).unwrap(); // limit upper
        assert!(emu.assigned_prefetch_range().is_none());
        emu.write_u32(0x4, MMIO_ENABLED).unwrap();
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x00AA_BBCC_FFF0_0000..=0x00DD_EEFF_FF0F_FFFF)
        );
        emu.write_u32(0x4, MMIO_DISABLED).unwrap();
        assert!(emu.assigned_prefetch_range().is_none());

        // The base can equal the limit, which is a valid 1MB range.
        emu.write_u32(0x24, 0xDD00_DD00).unwrap(); // limit + base
        emu.write_u32(0x28, 0x00AA_BBCC).unwrap(); // base upper
        emu.write_u32(0x2C, 0x00AA_BBCC).unwrap(); // limit upper
        assert!(emu.assigned_prefetch_range().is_none());
        emu.write_u32(0x4, MMIO_ENABLED).unwrap();
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x00AA_BBCC_DD00_0000..=0x00AA_BBCC_DD0F_FFFF)
        );
        emu.write_u32(0x4, MMIO_DISABLED).unwrap();
        assert!(emu.assigned_prefetch_range().is_none());
    }
}
