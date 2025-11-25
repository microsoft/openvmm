// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers that implement standardized PCI configuration space functionality.
//!
//! To be clear: PCI devices are not required to use these helpers, and may
//! choose to implement configuration space accesses manually.

use crate::PciInterruptPin;
use crate::bar_mapping::BarMappings;
use crate::capabilities::PciCapability;
use crate::spec::caps::CapabilityId;
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

/// PCI configuration space header type with corresponding BAR count
///
/// This enum provides a type-safe way to work with PCI configuration space header types
/// and their corresponding BAR counts. It improves readability over raw constants.
///
/// # Examples
///
/// ```rust
/// # use pci_core::cfg_space_emu::HeaderType;
/// // Get BAR count for different header types
/// assert_eq!(HeaderType::Type0.bar_count(), 6);
/// assert_eq!(HeaderType::Type1.bar_count(), 2);
///
/// // Convert to usize for use in generic contexts
/// let bar_count: usize = HeaderType::Type0.into();
/// assert_eq!(bar_count, 6);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderType {
    /// Type 0 header with 6 BARs (endpoint devices)
    Type0,
    /// Type 1 header with 2 BARs (bridge devices)  
    Type1,
}

impl HeaderType {
    /// Get the number of BARs for this header type
    pub const fn bar_count(self) -> usize {
        match self {
            HeaderType::Type0 => 6,
            HeaderType::Type1 => 2,
        }
    }
}

impl From<HeaderType> for usize {
    fn from(header_type: HeaderType) -> usize {
        header_type.bar_count()
    }
}

/// Constants for header type BAR counts
pub mod header_type_consts {
    use super::HeaderType;

    /// Number of BARs for Type 0 headers
    pub const TYPE0_BAR_COUNT: usize = HeaderType::Type0.bar_count();

    /// Number of BARs for Type 1 headers  
    pub const TYPE1_BAR_COUNT: usize = HeaderType::Type1.bar_count();
}

/// Result type for common header emulator operations
#[derive(Debug)]
pub enum CommonHeaderResult {
    /// The access was handled by the common header emulator
    Handled,
    /// The access is not handled by common header, caller should handle it
    Unhandled,
    /// The access failed with an error
    Failed(IoError),
}

impl PartialEq for CommonHeaderResult {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Handled, Self::Handled) => true,
            (Self::Unhandled, Self::Unhandled) => true,
            (Self::Failed(_), Self::Failed(_)) => true, // Consider all failures equal for testing
            _ => false,
        }
    }
}

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
struct ConfigSpaceCommonHeaderEmulatorState<const N: usize> {
    /// The command register
    command: cfg_space::Command,
    /// OS-configured BARs
    #[inspect(with = "inspect_helpers::bars_generic")]
    base_addresses: [u32; N],
    /// The PCI device doesn't actually care about what value is stored here -
    /// this register is just a bit of standardized "scratch space", ostensibly
    /// for firmware to communicate IRQ assignments to the OS, but it can really
    /// be used for just about anything.
    interrupt_line: u8,
}

impl<const N: usize> ConfigSpaceCommonHeaderEmulatorState<N> {
    fn new() -> Self {
        Self {
            command: cfg_space::Command::new(),
            base_addresses: {
                const ZERO: u32 = 0;
                [ZERO; N]
            },
            interrupt_line: 0,
        }
    }
}

/// Common emulator for shared PCI configuration space functionality.
/// Generic over the number of BARs (6 for Type 0, 2 for Type 1).
#[derive(Inspect)]
pub struct ConfigSpaceCommonHeaderEmulator<const N: usize> {
    // Fixed configuration
    #[inspect(with = "inspect_helpers::bars_generic")]
    bar_masks: [u32; N],
    hardware_ids: HardwareIds,
    multi_function_bit: bool,

    // Runtime glue
    #[inspect(with = r#"|x| inspect::iter_by_index(x).prefix("bar")"#)]
    mapped_memory: [Option<BarMemoryKind>; N],
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|cap| (cap.label(), cap)))")]
    capabilities: Vec<Box<dyn PciCapability>>,
    intx_interrupt: Option<Arc<IntxInterrupt>>,

    // Runtime book-keeping
    active_bars: BarMappings,

    // Volatile state
    state: ConfigSpaceCommonHeaderEmulatorState<N>,
}

/// Type alias for Type 0 common header emulator (6 BARs)
pub type ConfigSpaceCommonHeaderEmulatorType0 =
    ConfigSpaceCommonHeaderEmulator<{ header_type_consts::TYPE0_BAR_COUNT }>;

/// Type alias for Type 1 common header emulator (2 BARs)
pub type ConfigSpaceCommonHeaderEmulatorType1 =
    ConfigSpaceCommonHeaderEmulator<{ header_type_consts::TYPE1_BAR_COUNT }>;

impl<const N: usize> ConfigSpaceCommonHeaderEmulator<N> {
    /// Create a new common header emulator
    pub fn new(
        hardware_ids: HardwareIds,
        capabilities: Vec<Box<dyn PciCapability>>,
        bars: DeviceBars,
    ) -> Self {
        let mut bar_masks = {
            const ZERO: u32 = 0;
            [ZERO; N]
        };
        let mut mapped_memory = {
            const NONE: Option<BarMemoryKind> = None;
            [NONE; N]
        };

        // Only process BARs that fit within our supported range (N)
        for (bar_index, bar) in bars.bars.into_iter().enumerate().take(N) {
            let (len, mapped) = match bar {
                Some(bar) => bar,
                None => continue,
            };
            // use 64-bit aware BARs
            assert!(bar_index < N.saturating_sub(1));
            // Round up regions to a power of 2, as required by PCI (and
            // inherently required by the BAR representation). Round up to at
            // least one page to avoid various problems in guest OSes.
            const MIN_BAR_SIZE: u64 = 4096;
            let len = std::cmp::max(len.next_power_of_two(), MIN_BAR_SIZE);
            let mask64 = !(len - 1);
            bar_masks[bar_index] = cfg_space::BarEncodingBits::from_bits(mask64 as u32)
                .with_type_64_bit(true)
                .into_bits();
            if bar_index + 1 < N {
                bar_masks[bar_index + 1] = (mask64 >> 32) as u32;
            }
            mapped_memory[bar_index] = Some(mapped);
        }

        Self {
            hardware_ids,
            capabilities,
            bar_masks,
            mapped_memory,
            multi_function_bit: false,
            intx_interrupt: None,
            active_bars: Default::default(),
            state: ConfigSpaceCommonHeaderEmulatorState::new(),
        }
    }

    /// Get the number of BARs supported by this emulator
    pub const fn bar_count(&self) -> usize {
        N
    }

    /// Validate that this emulator has the correct number of BARs for the given header type
    pub fn validate_header_type(&self, expected: HeaderType) -> bool {
        N == expected.bar_count()
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

    /// Reset the common header state
    pub fn reset(&mut self) {
        tracing::info!("ConfigSpaceCommonHeaderEmulator: resetting state");
        self.state = ConfigSpaceCommonHeaderEmulatorState::new();

        tracing::info!("ConfigSpaceCommonHeaderEmulator: syncing command register after reset");
        self.sync_command_register(self.state.command);

        tracing::info!(
            "ConfigSpaceCommonHeaderEmulator: resetting {} capabilities",
            self.capabilities.len()
        );
        for cap in &mut self.capabilities {
            cap.reset();
        }

        if let Some(intx) = &mut self.intx_interrupt {
            tracing::info!("ConfigSpaceCommonHeaderEmulator: resetting interrupt level");
            intx.set_level(false);
        }
        tracing::info!("ConfigSpaceCommonHeaderEmulator: reset completed");
    }

    /// Get hardware IDs
    pub fn hardware_ids(&self) -> &HardwareIds {
        &self.hardware_ids
    }

    /// Get capabilities
    pub fn capabilities(&self) -> &[Box<dyn PciCapability>] {
        &self.capabilities
    }

    /// Get capabilities mutably
    pub fn capabilities_mut(&mut self) -> &mut [Box<dyn PciCapability>] {
        &mut self.capabilities
    }

    /// Get multi-function bit
    pub fn multi_function_bit(&self) -> bool {
        self.multi_function_bit
    }

    /// Get the header type for this emulator
    pub const fn header_type(&self) -> HeaderType {
        match N {
            header_type_consts::TYPE0_BAR_COUNT => HeaderType::Type0,
            header_type_consts::TYPE1_BAR_COUNT => HeaderType::Type1,
            _ => panic!("Unsupported BAR count - must be 6 (Type0) or 2 (Type1)"),
        }
    }

    /// Get current command register state
    pub fn command(&self) -> cfg_space::Command {
        self.state.command
    }

    /// Get current base addresses
    pub fn base_addresses(&self) -> &[u32; N] {
        &self.state.base_addresses
    }

    /// Get current interrupt line
    pub fn interrupt_line(&self) -> u8 {
        self.state.interrupt_line
    }

    /// Get current interrupt pin (returns the pin number + 1, or 0 if no pin configured)
    pub fn interrupt_pin(&self) -> u8 {
        if let Some(intx) = &self.intx_interrupt {
            (intx.pin as u8) + 1 // PCI spec: 1=INTA, 2=INTB, 3=INTC, 4=INTD, 0=no interrupt
        } else {
            0 // No interrupt pin configured
        }
    }

    /// Set interrupt line (for save/restore)
    pub fn set_interrupt_line(&mut self, interrupt_line: u8) {
        self.state.interrupt_line = interrupt_line;
    }

    /// Set base addresses (for save/restore)
    pub fn set_base_addresses(&mut self, base_addresses: &[u32; N]) {
        self.state.base_addresses = *base_addresses;
    }

    /// Set command register (for save/restore)
    pub fn set_command(&mut self, command: cfg_space::Command) {
        self.state.command = command;
    }

    /// Sync command register changes by updating both interrupt and MMIO state
    pub fn sync_command_register(&mut self, command: cfg_space::Command) {
        tracing::info!(
            "ConfigSpaceCommonHeaderEmulator: syncing command register - intx_disable={}, mmio_enabled={}",
            command.intx_disable(),
            command.mmio_enabled()
        );
        self.update_intx_disable(command.intx_disable());
        self.update_mmio_enabled(command.mmio_enabled());
    }

    /// Update interrupt disable setting
    pub fn update_intx_disable(&mut self, disabled: bool) {
        tracing::info!(
            "ConfigSpaceCommonHeaderEmulator: updating intx_disable={}",
            disabled
        );
        if let Some(intx_interrupt) = &self.intx_interrupt {
            intx_interrupt.set_disabled(disabled)
        }
    }

    /// Update MMIO enabled setting and handle BAR mapping
    pub fn update_mmio_enabled(&mut self, enabled: bool) {
        tracing::info!(
            "ConfigSpaceCommonHeaderEmulator: updating mmio_enabled={}",
            enabled
        );
        if enabled {
            // Note that BarMappings expects 6 BARs. Pad with 0 for Type 1 (N=2)
            // and use directly for Type 0 (N=6).
            let mut full_base_addresses = [0u32; 6];
            let mut full_bar_masks = [0u32; 6];

            // Copy our data into the first N positions
            full_base_addresses[..N].copy_from_slice(&self.state.base_addresses[..N]);
            full_bar_masks[..N].copy_from_slice(&self.bar_masks[..N]);

            self.active_bars = BarMappings::parse(&full_base_addresses, &full_bar_masks);
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

    // ===== Configuration Space Read/Write Functions =====

    /// Read from the config space. `offset` must be 32-bit aligned.
    /// Returns CommonHeaderResult indicating if handled, unhandled, or failed.
    pub fn read_u32(&self, offset: u16, value: &mut u32) -> CommonHeaderResult {
        use cfg_space::CommonHeader;

        tracing::trace!(
            "ConfigSpaceCommonHeaderEmulator: read_u32 offset={:#x}",
            offset
        );

        *value = match CommonHeader(offset) {
            CommonHeader::DEVICE_VENDOR => {
                (self.hardware_ids.device_id as u32) << 16 | self.hardware_ids.vendor_id as u32
            }
            CommonHeader::STATUS_COMMAND => {
                let mut status =
                    cfg_space::Status::new().with_capabilities_list(!self.capabilities.is_empty());

                if let Some(intx_interrupt) = &self.intx_interrupt {
                    if intx_interrupt.interrupt_status.load(Ordering::SeqCst) {
                        status.set_interrupt_status(true);
                    }
                }

                (status.into_bits() as u32) << 16 | self.state.command.into_bits() as u32
            }
            CommonHeader::CLASS_REVISION => {
                (u8::from(self.hardware_ids.base_class) as u32) << 24
                    | (u8::from(self.hardware_ids.sub_class) as u32) << 16
                    | (u8::from(self.hardware_ids.prog_if) as u32) << 8
                    | self.hardware_ids.revision_id as u32
            }
            CommonHeader::RESERVED_CAP_PTR => {
                if self.capabilities.is_empty() {
                    0
                } else {
                    0x40
                }
            }
            // Capabilities space - handled by common emulator
            _ if (0x40..0x100).contains(&offset) => {
                return self.read_capabilities(offset, value);
            }
            // Extended capabilities space - handled by common emulator
            _ if (0x100..0x1000).contains(&offset) => {
                return self.read_extended_capabilities(offset, value);
            }
            // Check if this is a BAR read
            _ if self.is_bar_offset(offset) => {
                return self.read_bar(offset, value);
            }
            // Unhandled access - not part of common header, caller should handle
            _ => {
                return CommonHeaderResult::Unhandled;
            }
        };

        tracing::trace!(
            "ConfigSpaceCommonHeaderEmulator: read_u32 offset={:#x} -> value={:#x}",
            offset,
            *value
        );
        // Handled access
        CommonHeaderResult::Handled
    }

    /// Write to the config space. `offset` must be 32-bit aligned.
    /// Returns CommonHeaderResult indicating if handled, unhandled, or failed.
    pub fn write_u32(&mut self, offset: u16, val: u32) -> CommonHeaderResult {
        use cfg_space::CommonHeader;

        tracing::trace!(
            "ConfigSpaceCommonHeaderEmulator: write_u32 offset={:#x} val={:#x}",
            offset,
            val
        );

        match CommonHeader(offset) {
            CommonHeader::STATUS_COMMAND => {
                let mut command = cfg_space::Command::from_bits(val as u16);
                if command.into_bits() & !SUPPORTED_COMMAND_BITS != 0 {
                    tracelimit::warn_ratelimited!(offset, val, "setting invalid command bits");
                    // still do our best
                    command =
                        cfg_space::Command::from_bits(command.into_bits() & SUPPORTED_COMMAND_BITS);
                };

                if self.state.command.intx_disable() != command.intx_disable() {
                    self.update_intx_disable(command.intx_disable())
                }

                if self.state.command.mmio_enabled() != command.mmio_enabled() {
                    self.update_mmio_enabled(command.mmio_enabled())
                }

                self.state.command = command;
            }
            // Capabilities space - handled by common emulator
            _ if (0x40..0x100).contains(&offset) => {
                return self.write_capabilities(offset, val);
            }
            // Extended capabilities space - handled by common emulator
            _ if (0x100..0x1000).contains(&offset) => {
                return self.write_extended_capabilities(offset, val);
            }
            // Check if this is a BAR write (Type 0: 0x10-0x27, Type 1: 0x10-0x17)
            _ if self.is_bar_offset(offset) => {
                return self.write_bar(offset, val);
            }
            // Unhandled access - not part of common header, caller should handle
            _ => {
                return CommonHeaderResult::Unhandled;
            }
        }

        // Handled access
        CommonHeaderResult::Handled
    }

    /// Helper for reading BAR registers
    fn read_bar(&self, offset: u16, value: &mut u32) -> CommonHeaderResult {
        if !self.is_bar_offset(offset) {
            return CommonHeaderResult::Unhandled;
        }

        let bar_index = self.get_bar_index(offset);
        if bar_index < N {
            *value = self.state.base_addresses[bar_index];
        } else {
            *value = 0;
        }
        CommonHeaderResult::Handled
    }

    /// Helper for writing BAR registers
    fn write_bar(&mut self, offset: u16, val: u32) -> CommonHeaderResult {
        if !self.is_bar_offset(offset) {
            return CommonHeaderResult::Unhandled;
        }

        // Handle BAR writes - only allow when MMIO is disabled
        if !self.state.command.mmio_enabled() {
            let bar_index = self.get_bar_index(offset);
            if bar_index < N {
                let mut bar_value = val & self.bar_masks[bar_index];

                // For even-indexed BARs, set the 64-bit type bit if the BAR is configured
                if bar_index & 1 == 0 && self.bar_masks[bar_index] != 0 {
                    bar_value = cfg_space::BarEncodingBits::from_bits(bar_value)
                        .with_type_64_bit(true)
                        .into_bits();
                }

                self.state.base_addresses[bar_index] = bar_value;
            }
        }
        CommonHeaderResult::Handled
    }

    /// Read from capabilities space. `offset` must be 32-bit aligned and >= 0x40.
    fn read_capabilities(&self, offset: u16, value: &mut u32) -> CommonHeaderResult {
        if (0x40..0x100).contains(&offset) {
            if let Some((cap_index, cap_offset)) =
                self.get_capability_index_and_offset(offset - 0x40)
            {
                *value = self.capabilities[cap_index].read_u32(cap_offset);
                if cap_offset == 0 {
                    let next = if cap_index < self.capabilities.len() - 1 {
                        offset as u32 + self.capabilities[cap_index].len() as u32
                    } else {
                        0
                    };
                    assert!(*value & 0xff00 == 0);
                    *value |= next << 8;
                }
                CommonHeaderResult::Handled
            } else {
                tracelimit::warn_ratelimited!(offset, "unhandled config space read");
                CommonHeaderResult::Failed(IoError::InvalidRegister)
            }
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    /// Write to capabilities space. `offset` must be 32-bit aligned and >= 0x40.
    fn write_capabilities(&mut self, offset: u16, val: u32) -> CommonHeaderResult {
        if (0x40..0x100).contains(&offset) {
            if let Some((cap_index, cap_offset)) =
                self.get_capability_index_and_offset(offset - 0x40)
            {
                self.capabilities[cap_index].write_u32(cap_offset, val);
                CommonHeaderResult::Handled
            } else {
                tracelimit::warn_ratelimited!(offset, value = val, "unhandled config space write");
                CommonHeaderResult::Failed(IoError::InvalidRegister)
            }
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    /// Read from extended capabilities space (0x100-0x1000). `offset` must be 32-bit aligned.
    fn read_extended_capabilities(&self, offset: u16, value: &mut u32) -> CommonHeaderResult {
        if (0x100..0x1000).contains(&offset) {
            if self.is_pcie_device() {
                *value = 0xffffffff;
                CommonHeaderResult::Handled
            } else {
                tracelimit::warn_ratelimited!(offset, "unhandled extended config space read");
                CommonHeaderResult::Failed(IoError::InvalidRegister)
            }
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    /// Write to extended capabilities space (0x100-0x1000). `offset` must be 32-bit aligned.
    fn write_extended_capabilities(&mut self, offset: u16, val: u32) -> CommonHeaderResult {
        if (0x100..0x1000).contains(&offset) {
            if self.is_pcie_device() {
                // For now, just ignore writes to extended config space
                CommonHeaderResult::Handled
            } else {
                tracelimit::warn_ratelimited!(
                    offset,
                    value = val,
                    "unhandled extended config space write"
                );
                CommonHeaderResult::Failed(IoError::InvalidRegister)
            }
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    // ===== Utility and Query Functions =====

    /// Finds a BAR + offset by address.
    pub fn find_bar(&self, address: u64) -> Option<(u8, u16)> {
        self.active_bars.find(address)
    }

    /// Check if this device is a PCIe device by looking for the PCI Express capability.
    pub fn is_pcie_device(&self) -> bool {
        self.capabilities
            .iter()
            .any(|cap| cap.capability_id() == CapabilityId::PCI_EXPRESS)
    }

    /// Get capability index and offset for a given offset
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

    /// Check if an offset corresponds to a BAR register
    fn is_bar_offset(&self, offset: u16) -> bool {
        // Type 0: BAR0-BAR5 (0x10-0x27), Type 1: BAR0-BAR1 (0x10-0x17)
        let bar_start = cfg_space::HeaderType00::BAR0.0;
        let bar_end = bar_start + (N as u16) * 4;
        (bar_start..bar_end).contains(&offset) && offset.is_multiple_of(4)
    }

    /// Get the BAR index for a given offset
    fn get_bar_index(&self, offset: u16) -> usize {
        ((offset - cfg_space::HeaderType00::BAR0.0) / 4) as usize
    }

    /// Get BAR masks (for testing only)
    #[cfg(test)]
    pub fn bar_masks(&self) -> &[u32; N] {
        &self.bar_masks
    }
}

#[derive(Debug, Inspect)]
struct ConfigSpaceType0EmulatorState {
    /// A read/write register that doesn't matter in virtualized contexts
    latency_timer: u8,
}

impl ConfigSpaceType0EmulatorState {
    fn new() -> Self {
        Self { latency_timer: 0 }
    }
}

/// Emulator for the standard Type 0 PCI configuration space header.
#[derive(Inspect)]
pub struct ConfigSpaceType0Emulator {
    /// The common header emulator that handles shared functionality
    #[inspect(flatten)]
    common: ConfigSpaceCommonHeaderEmulatorType0,
    /// Type 0 specific state
    state: ConfigSpaceType0EmulatorState,
}

mod inspect_helpers {
    use super::*;

    pub(crate) fn bars_generic<const N: usize>(bars: &[u32; N]) -> impl Inspect + '_ {
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
        let common = ConfigSpaceCommonHeaderEmulator::new(hardware_ids, capabilities, bars);

        Self {
            common,
            state: ConfigSpaceType0EmulatorState::new(),
        }
    }

    /// If the device is multi-function, enable bit 7 in the Header register.
    pub fn with_multi_function_bit(mut self, bit: bool) -> Self {
        self.common = self.common.with_multi_function_bit(bit);
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
        self.common.set_interrupt_pin(pin, line)
    }

    /// Resets the configuration space state.
    pub fn reset(&mut self) {
        self.common.reset();
        self.state = ConfigSpaceType0EmulatorState::new();
    }

    /// Read from the config space. `offset` must be 32-bit aligned.
    pub fn read_u32(&self, offset: u16, value: &mut u32) -> IoResult {
        use cfg_space::HeaderType00;

        // First try to handle with common header emulator
        match self.common.read_u32(offset, value) {
            CommonHeaderResult::Handled => return IoResult::Ok,
            CommonHeaderResult::Failed(err) => return IoResult::Err(err),
            CommonHeaderResult::Unhandled => {
                // Continue with Type 0 specific handling
            }
        }

        // Handle Type 0 specific registers
        *value = match HeaderType00(offset) {
            HeaderType00::BIST_HEADER => {
                let mut v = (self.state.latency_timer as u32) << 8;
                if self.common.multi_function_bit() {
                    // enable top-most bit of the header register
                    v |= 0x80 << 16;
                }
                v
            }
            HeaderType00::CARDBUS_CIS_PTR => 0,
            HeaderType00::SUBSYSTEM_ID => {
                (self.common.hardware_ids().type0_sub_system_id as u32) << 16
                    | self.common.hardware_ids().type0_sub_vendor_id as u32
            }
            HeaderType00::EXPANSION_ROM_BASE => 0,
            HeaderType00::RESERVED => 0,
            HeaderType00::LATENCY_INTERRUPT => {
                // Bits 7-0: Interrupt Line, Bits 15-8: Interrupt Pin, Bits 31-16: Latency Timer
                (self.state.latency_timer as u32) << 16
                    | (self.common.interrupt_pin() as u32) << 8
                    | self.common.interrupt_line() as u32
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
        use cfg_space::HeaderType00;

        // First try to handle with common header emulator
        match self.common.write_u32(offset, val) {
            CommonHeaderResult::Handled => return IoResult::Ok,
            CommonHeaderResult::Failed(err) => return IoResult::Err(err),
            CommonHeaderResult::Unhandled => {
                // Continue with Type 0 specific handling
            }
        }

        // Handle Type 0 specific registers
        match HeaderType00(offset) {
            HeaderType00::BIST_HEADER => {
                // BIST_HEADER - Type 0 specific handling
                // For now, just ignore these writes (header type is read-only)
            }
            HeaderType00::LATENCY_INTERRUPT => {
                // Bits 7-0: Interrupt Line (read/write)
                // Bits 15-8: Interrupt Pin (read-only, ignore writes)
                // Bits 31-16: Latency Timer (read/write)
                self.common.set_interrupt_line((val & 0xff) as u8);
                self.state.latency_timer = (val >> 16) as u8;
            }
            // all other base regs are noops
            _ if offset < 0x40 && offset.is_multiple_of(4) => (),
            _ => {
                tracelimit::warn_ratelimited!(offset, value = val, "unexpected config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    /// Finds a BAR + offset by address.
    pub fn find_bar(&self, address: u64) -> Option<(u8, u16)> {
        self.common.find_bar(address)
    }

    /// Checks if this device is a PCIe device by looking for the PCI Express capability.
    pub fn is_pcie_device(&self) -> bool {
        self.common.is_pcie_device()
    }

    /// Set the presence detect state for a hotplug-capable slot.
    /// This method finds the PCIe Express capability and calls its set_presence_detect_state method.
    /// If the PCIe Express capability is not found, the call is silently ignored.
    ///
    /// # Arguments
    /// * `present` - true if a device is present in the slot, false if the slot is empty
    pub fn set_presence_detect_state(&mut self, present: bool) {
        for capability in self.common.capabilities_mut() {
            if let Some(pcie_cap) = capability.as_pci_express_mut() {
                pcie_cap.set_presence_detect_state(present);
                return;
            }
        }

        // PCIe Express capability not found - silently ignore
    }
}

#[derive(Debug, Inspect)]
struct ConfigSpaceType1EmulatorState {
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
    /// The bridge control register. Contains various control bits for bridge behavior
    /// such as secondary bus reset, VGA enable, etc.
    bridge_control: u16,
}

impl ConfigSpaceType1EmulatorState {
    fn new() -> Self {
        Self {
            subordinate_bus_number: 0,
            secondary_bus_number: 0,
            primary_bus_number: 0,
            memory_base: 0,
            memory_limit: 0,
            prefetch_base: 0,
            prefetch_limit: 0,
            prefetch_base_upper: 0,
            prefetch_limit_upper: 0,
            bridge_control: 0,
        }
    }
}

/// Emulator for the standard Type 1 PCI configuration space header.
#[derive(Inspect)]
pub struct ConfigSpaceType1Emulator {
    /// The common header emulator that handles shared functionality
    #[inspect(flatten)]
    common: ConfigSpaceCommonHeaderEmulatorType1,
    /// Type 1 specific state
    state: ConfigSpaceType1EmulatorState,
}

impl ConfigSpaceType1Emulator {
    /// Create a new [`ConfigSpaceType1Emulator`]
    pub fn new(hardware_ids: HardwareIds, capabilities: Vec<Box<dyn PciCapability>>) -> Self {
        let common =
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, capabilities, DeviceBars::new());

        Self {
            common,
            state: ConfigSpaceType1EmulatorState::new(),
        }
    }

    /// Resets the configuration space state.
    pub fn reset(&mut self) {
        self.common.reset();
        self.state = ConfigSpaceType1EmulatorState::new();
    }

    /// Set the multi-function bit for this device.
    pub fn with_multi_function_bit(mut self, multi_function: bool) -> Self {
        self.common = self.common.with_multi_function_bit(multi_function);
        self
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
        if self.common.command().mmio_enabled() && base_addr <= limit_addr {
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
        if self.common.command().mmio_enabled() && base_addr <= limit_addr {
            Some(base_addr..=limit_addr)
        } else {
            None
        }
    }

    /// Read from the config space. `offset` must be 32-bit aligned.
    pub fn read_u32(&self, offset: u16, value: &mut u32) -> IoResult {
        use cfg_space::HeaderType01;

        // First try to handle with common header emulator
        match self.common.read_u32(offset, value) {
            CommonHeaderResult::Handled => return IoResult::Ok,
            CommonHeaderResult::Failed(err) => return IoResult::Err(err),
            CommonHeaderResult::Unhandled => {
                // Continue with Type 1 specific handling
            }
        }

        // Handle Type 1 specific registers
        *value = match HeaderType01(offset) {
            HeaderType01::BIST_HEADER => {
                // Header type 01 with optional multi-function bit
                if self.common.multi_function_bit() {
                    0x00810000 // Header type 01 with multi-function bit (bit 23)
                } else {
                    0x00010000 // Header type 01 without multi-function bit
                }
            }
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
            HeaderType01::EXPANSION_ROM_BASE => 0,
            HeaderType01::BRDIGE_CTRL_INTERRUPT => {
                // Read interrupt line from common header and bridge control from state
                // Bits 7-0: Interrupt Line, Bits 15-8: Interrupt Pin (0), Bits 31-16: Bridge Control
                (self.state.bridge_control as u32) << 16 | self.common.interrupt_line() as u32
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

        // First try to handle with common header emulator
        match self.common.write_u32(offset, val) {
            CommonHeaderResult::Handled => return IoResult::Ok,
            CommonHeaderResult::Failed(err) => return IoResult::Err(err),
            CommonHeaderResult::Unhandled => {
                // Continue with Type 1 specific handling
            }
        }

        // Handle Type 1 specific registers
        match HeaderType01(offset) {
            HeaderType01::BIST_HEADER => {
                // BIST_HEADER - Type 1 specific handling
                // For now, just ignore these writes (latency timer would go here if supported)
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
            HeaderType01::BRDIGE_CTRL_INTERRUPT => {
                // Delegate interrupt line writes to common header and store bridge control
                // Bits 7-0: Interrupt Line, Bits 15-8: Interrupt Pin (ignored), Bits 31-16: Bridge Control
                self.common.set_interrupt_line((val & 0xff) as u8);
                self.state.bridge_control = (val >> 16) as u16;
            }
            // all other base regs are noops
            _ if offset < 0x40 && offset.is_multiple_of(4) => (),
            _ => {
                tracelimit::warn_ratelimited!(offset, value = val, "unexpected config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    /// Checks if this device is a PCIe device by looking for the PCI Express capability.
    pub fn is_pcie_device(&self) -> bool {
        self.common.is_pcie_device()
    }

    /// Set the presence detect state for the slot.
    /// This method finds the PCIe Express capability and calls its set_presence_detect_state method.
    /// If the PCIe Express capability is not found, the call is silently ignored.
    ///
    /// # Arguments
    /// * `present` - true if a device is present in the slot, false if the slot is empty
    pub fn set_presence_detect_state(&mut self, present: bool) {
        // Find the PCIe Express capability
        for cap in self.common.capabilities_mut() {
            if cap.capability_id() == CapabilityId::PCI_EXPRESS {
                // Downcast to PciExpressCapability and call set_presence_detect_state
                if let Some(pcie_cap) = cap.as_pci_express_mut() {
                    pcie_cap.set_presence_detect_state(present);
                    return;
                }
            }
        }
        // If no PCIe Express capability is found, silently ignore the call
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

    impl<const N: usize> SaveRestore for ConfigSpaceCommonHeaderEmulator<N> {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: starting save operation",
                N
            );

            let ConfigSpaceCommonHeaderEmulatorState {
                command,
                base_addresses,
                interrupt_line,
            } = self.state;

            // Convert to 6-element array, padding with zeros if needed
            let mut saved_base_addresses = [0u32; 6];
            for (i, &addr) in base_addresses.iter().enumerate() {
                if i < 6 {
                    saved_base_addresses[i] = addr;
                }
            }

            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: saving state - command={:#x}, interrupt_line={}, base_addresses={:?}",
                N,
                command.into_bits(),
                interrupt_line,
                saved_base_addresses
            );

            let saved_state = state::SavedState {
                command: command.into_bits(),
                base_addresses: saved_base_addresses,
                interrupt_line,
                latency_timer: 0, // Not used in common header, always 0
                capabilities: self
                    .capabilities
                    .iter_mut()
                    .map(|cap| {
                        let id = cap.label().to_owned();
                        Ok((id, cap.save()?))
                    })
                    .collect::<Result<_, _>>()?,
            };

            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: save operation completed successfully",
                N
            );
            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: starting restore operation",
                N
            );

            let state::SavedState {
                command,
                base_addresses,
                interrupt_line,
                latency_timer: _, // Ignore latency_timer field
                capabilities,
            } = state;

            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: restoring state - command={:#x}, interrupt_line={}, base_addresses={:?}",
                N,
                command,
                interrupt_line,
                base_addresses
            );

            // Convert from 6-element array, taking only what we need
            let mut restored_base_addresses = {
                const ZERO: u32 = 0;
                [ZERO; N]
            };
            for (i, &addr) in base_addresses.iter().enumerate() {
                if i < N {
                    restored_base_addresses[i] = addr;
                }
            }

            self.state = ConfigSpaceCommonHeaderEmulatorState {
                command: cfg_space::Command::from_bits(command),
                base_addresses: restored_base_addresses,
                interrupt_line,
            };

            if command & !SUPPORTED_COMMAND_BITS != 0 {
                tracing::warn!(
                    "ConfigSpaceCommonHeaderEmulator<{}>: invalid command bits found: {:#x}",
                    N,
                    command
                );
                return Err(RestoreError::InvalidSavedState(
                    ConfigSpaceRestoreError::InvalidConfigBits.into(),
                ));
            }

            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: syncing command register",
                N
            );
            self.sync_command_register(self.state.command);

            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: restoring {} capabilities",
                N,
                capabilities.len()
            );
            for (id, entry) in capabilities {
                tracing::debug!(
                    save_id = id.as_str(),
                    "restoring pci common header capability"
                );

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
                    tracing::error!(
                        "ConfigSpaceCommonHeaderEmulator<{}>: failed to find capability: {}",
                        N,
                        id
                    );
                    return Err(RestoreError::InvalidSavedState(
                        ConfigSpaceRestoreError::InvalidCap(id).into(),
                    ));
                }
            }

            tracing::info!(
                "ConfigSpaceCommonHeaderEmulator<{}>: restore operation completed successfully",
                N
            );
            Ok(())
        }
    }

    mod type0_state {
        use super::state;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.cfg_space_emu")]
        pub struct SavedType0State {
            #[mesh(1)]
            pub latency_timer: u8,
            #[mesh(2)]
            pub common_header: state::SavedState,
        }
    }

    impl SaveRestore for ConfigSpaceType0Emulator {
        type SavedState = type0_state::SavedType0State;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            tracing::info!("ConfigSpaceType0Emulator: starting save operation");

            let ConfigSpaceType0EmulatorState { latency_timer } = self.state;

            tracing::info!(
                "ConfigSpaceType0Emulator: saving latency_timer={}",
                latency_timer
            );

            let saved_state = type0_state::SavedType0State {
                latency_timer,
                common_header: self.common.save()?,
            };

            tracing::info!("ConfigSpaceType0Emulator: save operation completed successfully");
            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            tracing::info!("ConfigSpaceType0Emulator: starting restore operation");

            let type0_state::SavedType0State {
                latency_timer,
                common_header,
            } = state;

            tracing::info!(
                "ConfigSpaceType0Emulator: restoring latency_timer={}",
                latency_timer
            );

            self.state = ConfigSpaceType0EmulatorState { latency_timer };

            tracing::info!("ConfigSpaceType0Emulator: delegating to common header restore");
            self.common.restore(common_header)?;

            tracing::info!("ConfigSpaceType0Emulator: restore operation completed successfully");
            Ok(())
        }
    }

    mod type1_state {
        use super::state;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.cfg_space_emu")]
        pub struct SavedType1State {
            #[mesh(1)]
            pub subordinate_bus_number: u8,
            #[mesh(2)]
            pub secondary_bus_number: u8,
            #[mesh(3)]
            pub primary_bus_number: u8,
            #[mesh(4)]
            pub memory_base: u16,
            #[mesh(5)]
            pub memory_limit: u16,
            #[mesh(6)]
            pub prefetch_base: u16,
            #[mesh(7)]
            pub prefetch_limit: u16,
            #[mesh(8)]
            pub prefetch_base_upper: u32,
            #[mesh(9)]
            pub prefetch_limit_upper: u32,
            #[mesh(10)]
            pub bridge_control: u16,
            #[mesh(11)]
            pub common_header: state::SavedState,
        }
    }

    impl SaveRestore for ConfigSpaceType1Emulator {
        type SavedState = type1_state::SavedType1State;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let ConfigSpaceType1EmulatorState {
                subordinate_bus_number,
                secondary_bus_number,
                primary_bus_number,
                memory_base,
                memory_limit,
                prefetch_base,
                prefetch_limit,
                prefetch_base_upper,
                prefetch_limit_upper,
                bridge_control,
            } = self.state;

            let saved_state = type1_state::SavedType1State {
                subordinate_bus_number,
                secondary_bus_number,
                primary_bus_number,
                memory_base,
                memory_limit,
                prefetch_base,
                prefetch_limit,
                prefetch_base_upper,
                prefetch_limit_upper,
                bridge_control,
                common_header: self.common.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let type1_state::SavedType1State {
                subordinate_bus_number,
                secondary_bus_number,
                primary_bus_number,
                memory_base,
                memory_limit,
                prefetch_base,
                prefetch_limit,
                prefetch_base_upper,
                prefetch_limit_upper,
                bridge_control,
                common_header,
            } = state;

            self.state = ConfigSpaceType1EmulatorState {
                subordinate_bus_number,
                secondary_bus_number,
                primary_bus_number,
                memory_base,
                memory_limit,
                prefetch_base,
                prefetch_limit,
                prefetch_base_upper,
                prefetch_limit_upper,
                bridge_control,
            };

            self.common.restore(common_header)?;

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::pci_express::PciExpressCapability;
    use crate::capabilities::read_only::ReadOnlyCapability;
    use crate::spec::caps::pci_express::DevicePortType;
    use crate::spec::hwid::ClassCode;
    use crate::spec::hwid::ProgrammingInterface;
    use crate::spec::hwid::Subclass;

    fn create_type0_emulator(caps: Vec<Box<dyn PciCapability>>) -> ConfigSpaceType0Emulator {
        ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0x3333,
                type0_sub_system_id: 0x4444,
            },
            caps,
            DeviceBars::new(),
        )
    }

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

    #[test]
    fn test_type1_is_pcie_device() {
        // Test Type 1 device without PCIe capability
        let emu = create_type1_emulator(vec![Box::new(ReadOnlyCapability::new("foo", 0))]);
        assert!(!emu.is_pcie_device());

        // Test Type 1 device with PCIe capability
        let emu = create_type1_emulator(vec![Box::new(PciExpressCapability::new(
            DevicePortType::RootPort,
            None,
        ))]);
        assert!(emu.is_pcie_device());

        // Test Type 1 device with multiple capabilities including PCIe
        let emu = create_type1_emulator(vec![
            Box::new(ReadOnlyCapability::new("foo", 0)),
            Box::new(PciExpressCapability::new(DevicePortType::Endpoint, None)),
            Box::new(ReadOnlyCapability::new("bar", 0)),
        ]);
        assert!(emu.is_pcie_device());
    }

    #[test]
    fn test_type0_is_pcie_device() {
        // Test Type 0 device without PCIe capability
        let emu = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(ReadOnlyCapability::new("foo", 0))],
            DeviceBars::new(),
        );
        assert!(!emu.is_pcie_device());

        // Test Type 0 device with PCIe capability
        let emu = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(PciExpressCapability::new(
                DevicePortType::Endpoint,
                None,
            ))],
            DeviceBars::new(),
        );
        assert!(emu.is_pcie_device());

        // Test Type 0 device with multiple capabilities including PCIe
        let emu = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![
                Box::new(ReadOnlyCapability::new("foo", 0)),
                Box::new(PciExpressCapability::new(DevicePortType::Endpoint, None)),
                Box::new(ReadOnlyCapability::new("bar", 0)),
            ],
            DeviceBars::new(),
        );
        assert!(emu.is_pcie_device());

        // Test Type 0 device with no capabilities
        let emu = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![],
            DeviceBars::new(),
        );
        assert!(!emu.is_pcie_device());
    }

    #[test]
    fn test_capability_ids() {
        // Test that capabilities return the correct capability IDs
        let pcie_cap = PciExpressCapability::new(DevicePortType::Endpoint, None);
        assert_eq!(pcie_cap.capability_id(), CapabilityId::PCI_EXPRESS);

        let read_only_cap = ReadOnlyCapability::new("test", 0u32);
        assert_eq!(read_only_cap.capability_id(), CapabilityId::VENDOR_SPECIFIC);
    }

    #[test]
    fn test_common_header_emulator_type0() {
        // Test the common header emulator with Type 0 configuration (6 BARs)
        let hardware_ids = HardwareIds {
            vendor_id: 0x1111,
            device_id: 0x2222,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::NONE,
            base_class: ClassCode::UNCLASSIFIED,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let bars = DeviceBars::new().bar0(4096, BarMemoryKind::Dummy);

        let common_emu: ConfigSpaceCommonHeaderEmulatorType0 =
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], bars);

        assert_eq!(common_emu.hardware_ids().vendor_id, 0x1111);
        assert_eq!(common_emu.hardware_ids().device_id, 0x2222);
        assert!(!common_emu.multi_function_bit());
        assert!(!common_emu.is_pcie_device());
        assert_ne!(common_emu.bar_masks()[0], 0); // Should have a mask for BAR0
    }

    #[test]
    fn test_common_header_emulator_type1() {
        // Test the common header emulator with Type 1 configuration (2 BARs)
        let hardware_ids = HardwareIds {
            vendor_id: 0x3333,
            device_id: 0x4444,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let bars = DeviceBars::new().bar0(4096, BarMemoryKind::Dummy);

        let mut common_emu: ConfigSpaceCommonHeaderEmulatorType1 =
            ConfigSpaceCommonHeaderEmulator::new(
                hardware_ids,
                vec![Box::new(PciExpressCapability::new(
                    DevicePortType::RootPort,
                    None,
                ))],
                bars,
            )
            .with_multi_function_bit(true);

        assert_eq!(common_emu.hardware_ids().vendor_id, 0x3333);
        assert_eq!(common_emu.hardware_ids().device_id, 0x4444);
        assert!(common_emu.multi_function_bit());
        assert!(common_emu.is_pcie_device());
        assert_ne!(common_emu.bar_masks()[0], 0); // Should have a mask for BAR0
        assert_eq!(common_emu.bar_masks().len(), 2);

        // Test reset functionality
        common_emu.reset();
        assert_eq!(common_emu.capabilities().len(), 1); // capabilities should still be there
    }

    #[test]
    fn test_common_header_emulator_no_bars() {
        // Test the common header emulator with no BARs configured
        let hardware_ids = HardwareIds {
            vendor_id: 0x5555,
            device_id: 0x6666,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::NONE,
            base_class: ClassCode::UNCLASSIFIED,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        // Create bars with no BARs configured
        let bars = DeviceBars::new();

        let common_emu: ConfigSpaceCommonHeaderEmulatorType0 =
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], bars);

        assert_eq!(common_emu.hardware_ids().vendor_id, 0x5555);
        assert_eq!(common_emu.hardware_ids().device_id, 0x6666);

        // All BAR masks should be 0 when no BARs are configured
        for &mask in common_emu.bar_masks() {
            assert_eq!(mask, 0);
        }
    }

    #[test]
    fn test_common_header_emulator_type1_ignores_extra_bars() {
        // Test that Type 1 emulator ignores BARs beyond index 1 (only supports 2 BARs)
        let hardware_ids = HardwareIds {
            vendor_id: 0x7777,
            device_id: 0x8888,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        // Configure BARs 0, 2, and 4 - Type 1 should only use BAR0 (and BAR1 as upper 32 bits)
        let bars = DeviceBars::new()
            .bar0(4096, BarMemoryKind::Dummy)
            .bar2(8192, BarMemoryKind::Dummy)
            .bar4(16384, BarMemoryKind::Dummy);

        let common_emu: ConfigSpaceCommonHeaderEmulatorType1 =
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], bars);

        assert_eq!(common_emu.hardware_ids().vendor_id, 0x7777);
        assert_eq!(common_emu.hardware_ids().device_id, 0x8888);

        // Should have a mask for BAR0, and BAR1 should be the upper 32 bits (64-bit BAR)
        assert_ne!(common_emu.bar_masks()[0], 0); // BAR0 should be configured
        assert_ne!(common_emu.bar_masks()[1], 0); // BAR1 should be upper 32 bits of BAR0
        assert_eq!(common_emu.bar_masks().len(), 2); // Type 1 only has 2 BARs

        // BAR2 and higher should be ignored (not accessible in Type 1 with N=2)
        // This demonstrates that extra BARs in DeviceBars are properly ignored
    }

    #[test]
    fn test_common_header_extended_capabilities() {
        // Test common header emulator extended capabilities
        let mut common_emu_no_pcie = ConfigSpaceCommonHeaderEmulatorType0::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(ReadOnlyCapability::new("foo", 0))],
            DeviceBars::new(),
        );
        assert!(!common_emu_no_pcie.is_pcie_device());

        let mut common_emu_pcie = ConfigSpaceCommonHeaderEmulatorType0::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(PciExpressCapability::new(
                DevicePortType::Endpoint,
                None,
            ))],
            DeviceBars::new(),
        );
        assert!(common_emu_pcie.is_pcie_device());

        // Test reading extended capabilities - non-PCIe device should return error
        let mut value = 0;
        assert!(matches!(
            common_emu_no_pcie.read_extended_capabilities(0x100, &mut value),
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        ));

        // Test reading extended capabilities - PCIe device should return 0xffffffff
        let mut value = 0;
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(0x100, &mut value),
            CommonHeaderResult::Handled
        ));
        assert_eq!(value, 0xffffffff);

        // Test writing extended capabilities - non-PCIe device should return error
        assert!(matches!(
            common_emu_no_pcie.write_extended_capabilities(0x100, 0x1234),
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        ));

        // Test writing extended capabilities - PCIe device should accept writes
        assert!(matches!(
            common_emu_pcie.write_extended_capabilities(0x100, 0x1234),
            CommonHeaderResult::Handled
        ));

        // Test invalid offset ranges
        let mut value = 0;
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(0x99, &mut value),
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        ));
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(0x1000, &mut value),
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        ));
    }

    #[test]
    fn test_common_header_emulator_save_restore() {
        use vmcore::save_restore::SaveRestore;

        // Test Type 0 common header emulator save/restore
        let hardware_ids = HardwareIds {
            vendor_id: 0x1111,
            device_id: 0x2222,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::NONE,
            base_class: ClassCode::UNCLASSIFIED,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let bars = DeviceBars::new().bar0(4096, BarMemoryKind::Dummy);

        let mut common_emu: ConfigSpaceCommonHeaderEmulatorType0 =
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], bars);

        // Modify some state
        let mut test_val = 0u32;
        let result = common_emu.write_u32(0x04, 0x0007); // Enable some command bits
        assert_eq!(result, CommonHeaderResult::Handled);
        let result = common_emu.read_u32(0x04, &mut test_val);
        assert_eq!(result, CommonHeaderResult::Handled);
        assert_eq!(test_val & 0x0007, 0x0007);

        // Save the state
        let saved_state = common_emu.save().expect("save should succeed");

        // Reset the emulator
        common_emu.reset();
        let result = common_emu.read_u32(0x04, &mut test_val);
        assert_eq!(result, CommonHeaderResult::Handled);
        assert_eq!(test_val & 0x0007, 0x0000); // Should be reset

        // Restore the state
        common_emu
            .restore(saved_state)
            .expect("restore should succeed");
        let result = common_emu.read_u32(0x04, &mut test_val);
        assert_eq!(result, CommonHeaderResult::Handled);
        assert_eq!(test_val & 0x0007, 0x0007); // Should be restored

        // Test Type 1 common header emulator save/restore
        let hardware_ids = HardwareIds {
            vendor_id: 0x3333,
            device_id: 0x4444,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            sub_class: Subclass::BRIDGE_PCI_TO_PCI,
            base_class: ClassCode::BRIDGE,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let bars = DeviceBars::new(); // No BARs for Type 1

        let mut common_emu_type1: ConfigSpaceCommonHeaderEmulatorType1 =
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], bars);

        // Modify some state
        let result = common_emu_type1.write_u32(0x04, 0x0003); // Enable some command bits
        assert_eq!(result, CommonHeaderResult::Handled);
        let result = common_emu_type1.read_u32(0x04, &mut test_val);
        assert_eq!(result, CommonHeaderResult::Handled);
        assert_eq!(test_val & 0x0003, 0x0003);

        // Save the state
        let saved_state = common_emu_type1.save().expect("save should succeed");

        // Reset the emulator
        common_emu_type1.reset();
        let result = common_emu_type1.read_u32(0x04, &mut test_val);
        assert_eq!(result, CommonHeaderResult::Handled);
        assert_eq!(test_val & 0x0003, 0x0000); // Should be reset

        // Restore the state
        common_emu_type1
            .restore(saved_state)
            .expect("restore should succeed");
        let result = common_emu_type1.read_u32(0x04, &mut test_val);
        assert_eq!(result, CommonHeaderResult::Handled);
        assert_eq!(test_val & 0x0003, 0x0003); // Should be restored
    }

    #[test]
    fn test_config_space_type1_set_presence_detect_state() {
        // Test that ConfigSpaceType1Emulator can set presence detect state
        // when it has a PCIe Express capability with hotplug support

        // Create a PCIe Express capability with hotplug support
        let pcie_cap =
            PciExpressCapability::new(DevicePortType::RootPort, None).with_hotplug_support(1);

        let mut emulator = create_type1_emulator(vec![Box::new(pcie_cap)]);

        // Initially, presence detect state should be 0
        let mut slot_status_val = 0u32;
        let result = emulator.read_u32(0x58, &mut slot_status_val); // 0x40 (cap start) + 0x18 (slot control/status)
        assert!(matches!(result, IoResult::Ok));
        let initial_presence_detect = (slot_status_val >> 22) & 0x1; // presence_detect_state is bit 6 of slot status
        assert_eq!(
            initial_presence_detect, 0,
            "Initial presence detect state should be 0"
        );

        // Set device as present
        emulator.set_presence_detect_state(true);
        let result = emulator.read_u32(0x58, &mut slot_status_val);
        assert!(matches!(result, IoResult::Ok));
        let present_presence_detect = (slot_status_val >> 22) & 0x1;
        assert_eq!(
            present_presence_detect, 1,
            "Presence detect state should be 1 when device is present"
        );

        // Set device as not present
        emulator.set_presence_detect_state(false);
        let result = emulator.read_u32(0x58, &mut slot_status_val);
        assert!(matches!(result, IoResult::Ok));
        let absent_presence_detect = (slot_status_val >> 22) & 0x1;
        assert_eq!(
            absent_presence_detect, 0,
            "Presence detect state should be 0 when device is not present"
        );
    }

    #[test]
    fn test_config_space_type1_set_presence_detect_state_without_pcie() {
        // Test that ConfigSpaceType1Emulator silently ignores set_presence_detect_state
        // when there is no PCIe Express capability

        let mut emulator = create_type1_emulator(vec![]); // No capabilities

        // Should not panic and should be silently ignored
        emulator.set_presence_detect_state(true);
        emulator.set_presence_detect_state(false);
    }

    #[test]
    fn test_interrupt_pin_register() {
        use vmcore::line_interrupt::LineInterrupt;

        // Test Type 0 device with interrupt pin configured
        let mut emu = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![],
            DeviceBars::new(),
        );

        // Initially, no interrupt pin should be configured
        let mut val = 0u32;
        emu.read_u32(0x3C, &mut val).unwrap(); // LATENCY_INTERRUPT register
        assert_eq!(val & 0xFF00, 0); // Interrupt pin should be 0

        // Configure interrupt pin A
        let line_interrupt = LineInterrupt::detached();
        emu.set_interrupt_pin(PciInterruptPin::IntA, line_interrupt);

        // Read the register again
        emu.read_u32(0x3C, &mut val).unwrap();
        assert_eq!((val >> 8) & 0xFF, 1); // Interrupt pin should be 1 (INTA)

        // Set interrupt line to 0x42 and verify both pin and line are correct
        emu.write_u32(0x3C, 0x00110042).unwrap(); // Latency=0x11, pin=ignored, line=0x42
        emu.read_u32(0x3C, &mut val).unwrap();
        assert_eq!(val & 0xFF, 0x42); // Interrupt line should be 0x42
        assert_eq!((val >> 8) & 0xFF, 1); // Interrupt pin should still be 1 (writes ignored)
        assert_eq!((val >> 16) & 0xFF, 0x11); // Latency timer should be 0x11

        // Test with interrupt pin D
        let mut emu_d = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: 0x1111,
                device_id: 0x2222,
                revision_id: 1,
                prog_if: ProgrammingInterface::NONE,
                sub_class: Subclass::NONE,
                base_class: ClassCode::UNCLASSIFIED,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![],
            DeviceBars::new(),
        );

        let line_interrupt_d = LineInterrupt::detached();
        emu_d.set_interrupt_pin(PciInterruptPin::IntD, line_interrupt_d);

        emu_d.read_u32(0x3C, &mut val).unwrap();
        assert_eq!((val >> 8) & 0xFF, 4); // Interrupt pin should be 4 (INTD)
    }

    #[test]
    fn test_header_type_functionality() {
        // Test HeaderType enum values
        assert_eq!(HeaderType::Type0.bar_count(), 6);
        assert_eq!(HeaderType::Type1.bar_count(), 2);
        assert_eq!(usize::from(HeaderType::Type0), 6);
        assert_eq!(usize::from(HeaderType::Type1), 2);

        // Test constant values
        assert_eq!(header_type_consts::TYPE0_BAR_COUNT, 6);
        assert_eq!(header_type_consts::TYPE1_BAR_COUNT, 2);

        // Test Type 0 emulator
        let emu_type0 = create_type0_emulator(vec![]);
        assert_eq!(emu_type0.common.bar_count(), 6);
        assert_eq!(emu_type0.common.header_type(), HeaderType::Type0);
        assert!(emu_type0.common.validate_header_type(HeaderType::Type0));
        assert!(!emu_type0.common.validate_header_type(HeaderType::Type1));

        // Test Type 1 emulator
        let emu_type1 = create_type1_emulator(vec![]);
        assert_eq!(emu_type1.common.bar_count(), 2);
        assert_eq!(emu_type1.common.header_type(), HeaderType::Type1);
        assert!(emu_type1.common.validate_header_type(HeaderType::Type1));
        assert!(!emu_type1.common.validate_header_type(HeaderType::Type0));
    }
}
