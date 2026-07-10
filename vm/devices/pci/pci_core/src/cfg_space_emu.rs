// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers that implement standardized PCI configuration space functionality.
//!
//! To be clear: PCI devices are not required to use these helpers, and may
//! choose to implement configuration space accesses manually.

use crate::PciInterruptPin;
use crate::bar_mapping::BarMappings;
use crate::capabilities::PciCapability;
use crate::capabilities::extended::PciExtendedCapability;
use crate::spec::caps::{COMMON_HEADER_END, CapabilityId, EXT_CAP_END, EXT_CAP_START};
use crate::spec::cfg_space;
use crate::spec::hwid::HardwareIds;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAddress;
use chipset_device::pci::PciConfigByteEnable;
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
    /// The bus number captured by this emulator.
    captured_bus_number: u8,
    /// The combined devfn (device << 3 | function) captured by this emulator.
    captured_devfn: u8,
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
            captured_bus_number: 0,
            captured_devfn: 0,
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
    #[inspect(with = "|x| inspect::iter_by_key(x.iter().map(|cap| (cap.label(), cap)))")]
    extended_capabilities: Vec<Box<dyn PciExtendedCapability>>,
    intx_interrupt: Option<Arc<IntxInterrupt>>,

    // Runtime book-keeping
    active_bars: BarMappings,

    // Volatile state
    state: ConfigSpaceCommonHeaderEmulatorState<N>,
}

impl<const N: usize> Drop for ConfigSpaceCommonHeaderEmulator<N> {
    fn drop(&mut self) {
        // Release any live BAR intercept registrations when the device's
        // config space is torn down (e.g. a PCIe hot-remove). The BAR intercept
        // controls are owned here in `mapped_memory`; without this, a removed
        // device's BAR ranges stay registered in the chipset's shared range
        // map, and a subsequent device that reuses the same GPA (a hot-add on
        // the same port) fails to install its intercept with an
        // IoRangeConflict, leaving its BAR undispatched (guest reads all-1s).
        for mapping in self.mapped_memory.iter_mut().flatten() {
            mapping.unmap_from_guest();
        }
    }
}

/// Type alias for Type 0 common header emulator (6 BARs)
pub type ConfigSpaceCommonHeaderEmulatorType0 =
    ConfigSpaceCommonHeaderEmulator<{ header_type_consts::TYPE0_BAR_COUNT }>;

/// Type alias for Type 1 common header emulator (2 BARs)
pub type ConfigSpaceCommonHeaderEmulatorType1 =
    ConfigSpaceCommonHeaderEmulator<{ header_type_consts::TYPE1_BAR_COUNT }>;

impl<const N: usize> ConfigSpaceCommonHeaderEmulator<N> {
    fn validated_extended_cap_len_bytes(cap: &dyn PciExtendedCapability) -> usize {
        let len = cap.len();
        assert!(
            len != 0,
            "extended capability '{}' len() must be non-zero",
            cap.label()
        );
        assert!(
            len.is_multiple_of(4),
            "extended capability '{}' len() must be 4-byte aligned, got {}",
            cap.label(),
            len
        );
        len
    }

    /// Create a new common header emulator
    pub fn new(
        hardware_ids: HardwareIds,
        capabilities: Vec<Box<dyn PciCapability>>,
        extended_capabilities: Vec<Box<dyn PciExtendedCapability>>,
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
                .with_prefetchable(true)
                .into_bits();
            if bar_index + 1 < N {
                bar_masks[bar_index + 1] = (mask64 >> 32) as u32;
            }
            mapped_memory[bar_index] = Some(mapped);
        }

        // Validate extended capability packing invariants so next-pointer
        // traversal remains correct.
        let mut cap_base = usize::from(EXT_CAP_START);
        for cap in &extended_capabilities {
            let len = Self::validated_extended_cap_len_bytes(cap.as_ref());

            cap_base = cap_base
                .checked_add(len)
                .expect("extended capability size overflow");
            assert!(
                cap_base <= usize::from(EXT_CAP_END),
                "extended capabilities exceed config space window {:#x}..{:#x} (exclusive end), cap_base={:#x}",
                EXT_CAP_START,
                EXT_CAP_END,
                cap_base
            );
        }

        Self {
            hardware_ids,
            extended_capabilities,
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
        tracing::debug!("ConfigSpaceCommonHeaderEmulator: resetting state");
        self.state = ConfigSpaceCommonHeaderEmulatorState::new();

        tracing::debug!("ConfigSpaceCommonHeaderEmulator: syncing command register after reset");
        self.sync_command_register(self.state.command);

        tracing::debug!(
            "ConfigSpaceCommonHeaderEmulator: resetting {} capabilities",
            self.capabilities.len()
        );
        for cap in &mut self.capabilities {
            cap.reset();
        }

        tracing::debug!(
            "ConfigSpaceCommonHeaderEmulator: resetting {} extended capabilities",
            self.extended_capabilities.len()
        );
        for cap in &mut self.extended_capabilities {
            cap.reset();
        }

        if let Some(intx) = &mut self.intx_interrupt {
            tracing::debug!("ConfigSpaceCommonHeaderEmulator: resetting interrupt level");
            intx.set_level(false);
        }
        tracing::debug!("ConfigSpaceCommonHeaderEmulator: reset completed");
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
        tracing::debug!(
            "ConfigSpaceCommonHeaderEmulator: syncing command register - intx_disable={}, mmio_enabled={}",
            command.intx_disable(),
            command.mmio_enabled()
        );
        self.update_intx_disable(command.intx_disable());
        self.update_mmio_enabled(command.mmio_enabled());
    }

    /// Update interrupt disable setting
    pub fn update_intx_disable(&mut self, disabled: bool) {
        tracing::debug!(
            "ConfigSpaceCommonHeaderEmulator: updating intx_disable={}",
            disabled
        );
        if let Some(intx_interrupt) = &self.intx_interrupt {
            intx_interrupt.set_disabled(disabled)
        }
    }

    /// Update MMIO enabled setting and handle BAR mapping
    pub fn update_mmio_enabled(&mut self, enabled: bool) {
        tracing::debug!(
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

    /// Returns the currently captured bus number.
    pub fn captured_bus_number(&self) -> u8 {
        self.state.captured_bus_number
    }

    /// Returns the currently captured devfn (device << 3 | function) number.
    pub fn captured_devfn(&self) -> u8 {
        self.state.captured_devfn
    }

    /// Overwrites the captured bus number.
    pub fn set_captured_bus_number(&mut self, bus_number: u8) {
        self.state.captured_bus_number = bus_number;
    }

    /// Overwrites the captured devfn (device << 3 | fn) number.
    pub fn set_captured_devfn(&mut self, devfn: u8) {
        self.state.captured_devfn = devfn;
    }

    // ===== Configuration Space Read/Write Functions =====

    /// Read from the config space.
    /// Returns CommonHeaderResult indicating if handled, unhandled, or failed.
    pub fn read(
        &self,
        address: PciConfigAddress,
        mut value: ByteEnabledDwordRead<'_>,
    ) -> CommonHeaderResult {
        use cfg_space::CommonHeader;
        let offset = address.byte_offset();

        tracing::trace!("ConfigSpaceCommonHeaderEmulator: read offset={:#x}", offset);

        match CommonHeader(offset) {
            CommonHeader::DEVICE_VENDOR => {
                value.set_low_high(self.hardware_ids.vendor_id, self.hardware_ids.device_id);
            }
            CommonHeader::STATUS_COMMAND => {
                let mut status =
                    cfg_space::Status::new().with_capabilities_list(!self.capabilities.is_empty());

                if let Some(intx_interrupt) = &self.intx_interrupt {
                    if intx_interrupt.interrupt_status.load(Ordering::SeqCst) {
                        status.set_interrupt_status(true);
                    }
                }

                value.set_low_high(self.state.command.into_bits(), status.into_bits());
            }
            CommonHeader::CLASS_REVISION => {
                value.set_bytes(
                    self.hardware_ids.revision_id,
                    u8::from(self.hardware_ids.prog_if),
                    u8::from(self.hardware_ids.sub_class),
                    u8::from(self.hardware_ids.base_class),
                );
            }
            CommonHeader::RESERVED_CAP_PTR => {
                value.set(if self.capabilities.is_empty() {
                    0
                } else {
                    COMMON_HEADER_END as u32
                });
            }
            // Capabilities space - handled by common emulator
            _ if (COMMON_HEADER_END..EXT_CAP_START).contains(&offset) => {
                return self.read_capabilities(offset, value);
            }
            // Extended capabilities space - handled by common emulator
            _ if (EXT_CAP_START..EXT_CAP_END).contains(&offset) => {
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
            ?value,
            "ConfigSpaceCommonHeaderEmulator: read offset={:#x}",
            offset,
        );
        // Handled access
        CommonHeaderResult::Handled
    }

    /// Write to the config space.
    /// Returns CommonHeaderResult indicating if handled, unhandled, or failed.
    pub fn write(
        &mut self,
        address: PciConfigAddress,
        val: ByteEnabledDwordWrite,
    ) -> CommonHeaderResult {
        use cfg_space::CommonHeader;
        let offset = address.byte_offset();

        tracing::trace!(
            ?val,
            "ConfigSpaceCommonHeaderEmulator: write offset={:#x}",
            offset,
        );

        // Capture the bus number as described in section 2.2.6.2.1 of the PCIe spec (Rev 7.0).
        // The spec recommends that functions only capture these values on successful handling of
        // the access, but we can't really tell that here from this shared emulation helper so we
        // instead capture unconditionally.
        if address.bus != self.state.captured_bus_number
            || address.devfn != self.state.captured_devfn
        {
            tracing::debug!(
                "ConfigSpaceCommonHeaderEmulator: capturing bdf {:x}:{:x}.{:x}",
                address.bus,
                address.device(),
                address.function(),
            );
        }
        self.state.captured_bus_number = address.bus;
        self.state.captured_devfn = address.devfn;

        match CommonHeader(offset) {
            CommonHeader::STATUS_COMMAND => {
                let mut command =
                    cfg_space::Command::from_bits(val.merge_low(self.state.command.into_bits()));
                if command.into_bits() & !SUPPORTED_COMMAND_BITS != 0 {
                    tracelimit::warn_ratelimited!(offset, ?val, "setting invalid command bits");
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
            _ if (COMMON_HEADER_END..EXT_CAP_START).contains(&offset) => {
                return self.write_capabilities(offset, val);
            }
            // Extended capabilities space - handled by common emulator
            _ if (EXT_CAP_START..EXT_CAP_END).contains(&offset) => {
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
    fn read_bar(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) -> CommonHeaderResult {
        if !self.is_bar_offset(offset) {
            return CommonHeaderResult::Unhandled;
        }

        let bar_index = self.get_bar_index(offset);
        value.set(if bar_index < N {
            self.state.base_addresses[bar_index]
        } else {
            0
        });
        CommonHeaderResult::Handled
    }

    /// Helper for writing BAR registers
    fn write_bar(&mut self, offset: u16, val: ByteEnabledDwordWrite) -> CommonHeaderResult {
        if !self.is_bar_offset(offset) {
            return CommonHeaderResult::Unhandled;
        }

        // Handle BAR writes - only allow when MMIO is disabled
        if !self.state.command.mmio_enabled() {
            let bar_index = self.get_bar_index(offset);
            if bar_index < N {
                let val = val.merge(self.state.base_addresses[bar_index]);
                let mut bar_value = val & self.bar_masks[bar_index];

                // Preserve BAR in-band attribute bits (low nibble) on the
                // low DWORD of mapped BARs. This applies to both 32-bit BARs
                // and the low DWORD of 64-bit BARs. Upper DWORDs are not
                // marked as mapped and therefore skip this path.
                if self.mapped_memory[bar_index].is_some() {
                    const BAR_ATTR_MASK: u32 = 0xF;
                    let attr_bits = self.bar_masks[bar_index] & BAR_ATTR_MASK;
                    bar_value = (bar_value & !BAR_ATTR_MASK) | attr_bits;
                }

                self.state.base_addresses[bar_index] = bar_value;
            }
        }
        CommonHeaderResult::Handled
    }

    /// Read from capabilities space. `offset` must be 32-bit aligned and >= COMMON_HEADER_END.
    fn read_capabilities(
        &self,
        offset: u16,
        mut value: ByteEnabledDwordRead<'_>,
    ) -> CommonHeaderResult {
        if (COMMON_HEADER_END..EXT_CAP_START).contains(&offset) {
            if let Some((cap_index, cap_offset)) =
                self.get_capability_index_and_offset(offset - COMMON_HEADER_END)
            {
                if cap_offset == 0 {
                    // Byte 1 of the first DWORD of the capability is the offset of the next
                    // capability (or 0).
                    if let Some(mut v) = value.restrict(PciConfigByteEnable::BYTE1) {
                        let next = if cap_index < self.capabilities.len() - 1 {
                            offset as u32 + self.capabilities[cap_index].len() as u32
                        } else {
                            0
                        };
                        v.set(next << 8);
                    }

                    if let Some(v) = value.exclude(PciConfigByteEnable::BYTE1) {
                        self.capabilities[cap_index].read(cap_offset, v);
                    }
                } else {
                    self.capabilities[cap_index].read(cap_offset, value);
                }
            } else {
                // Unimplemented registers in a present function read as 0.
                value.set(0);
            }
            CommonHeaderResult::Handled
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    /// Write to capabilities space. `offset` must be 32-bit aligned and >= COMMON_HEADER_END.
    fn write_capabilities(
        &mut self,
        offset: u16,
        val: ByteEnabledDwordWrite,
    ) -> CommonHeaderResult {
        if (COMMON_HEADER_END..EXT_CAP_START).contains(&offset) {
            if let Some((cap_index, cap_offset)) =
                self.get_capability_index_and_offset(offset - COMMON_HEADER_END)
            {
                self.capabilities[cap_index].write(cap_offset, val);
                CommonHeaderResult::Handled
            } else {
                // Writes to unimplemented registers in a present function are
                // dropped.
                CommonHeaderResult::Handled
            }
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    /// Read from extended capabilities space (EXT_CAP_START-EXT_CAP_END). `offset` must be 32-bit aligned.
    fn read_extended_capabilities(
        &self,
        offset: u16,
        mut value: ByteEnabledDwordRead<'_>,
    ) -> CommonHeaderResult {
        if (EXT_CAP_START..EXT_CAP_END).contains(&offset) {
            if self.is_pcie_device() {
                if let Some((cap_index, cap_offset, cap_base)) =
                    self.get_extended_capability_index_and_offset(offset)
                {
                    self.extended_capabilities[cap_index].read(cap_offset, value.reborrow());

                    if cap_offset == 0 {
                        let next = if cap_index < self.extended_capabilities.len() - 1 {
                            let cap_size = Self::validated_extended_cap_len_bytes(
                                self.extended_capabilities[cap_index].as_ref(),
                            ) as u16;
                            cap_base + cap_size
                        } else {
                            0
                        };

                        let mut cap_result = value.extract();
                        if let Some(mut v) = value.restrict(PciConfigByteEnable::HIGH_WORD) {
                            assert!(cap_result & 0xfff0_0000 == 0);
                            cap_result |= u32::from(next) << 20;
                            v.set(cap_result);
                        }
                    }
                } else {
                    // No more extended capabilities; the terminating header
                    // reads as 0.
                    value.set(0);
                }
            } else {
                // A conventional (non-PCIe) function has no extended
                // configuration space; the region reads as 0.
                value.set(0);
            };
            CommonHeaderResult::Handled
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    /// Write to extended capabilities space (EXT_CAP_START-EXT_CAP_END). `offset` must be 32-bit aligned.
    fn write_extended_capabilities(
        &mut self,
        offset: u16,
        val: ByteEnabledDwordWrite,
    ) -> CommonHeaderResult {
        if (EXT_CAP_START..EXT_CAP_END).contains(&offset) {
            if self.is_pcie_device() {
                if let Some((cap_index, cap_offset, _)) =
                    self.get_extended_capability_index_and_offset(offset)
                {
                    self.extended_capabilities[cap_index].write(cap_offset, val);
                }
            } else {
                // No extended configuration space on a conventional function;
                // writes to the region are dropped.
            }
            CommonHeaderResult::Handled
        } else {
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        }
    }

    // ===== Utility and Query Functions =====

    /// Finds a BAR + offset by address.
    pub fn find_bar(&self, address: u64) -> Option<(u8, u64)> {
        self.active_bars.find(address)
    }

    /// Gets the active base address for a specific BAR index, if mapped.
    pub fn bar_address(&self, bar: u8) -> Option<u64> {
        self.active_bars.get(bar)
    }

    /// Check if this device is a PCIe device by looking for the PCI Express capability.
    pub fn is_pcie_device(&self) -> bool {
        self.capabilities
            .iter()
            .any(|cap| cap.capability_id() == CapabilityId::PCI_EXPRESS)
    }

    /// Get extended capability index and offset for a given config offset.
    fn get_extended_capability_index_and_offset(&self, offset: u16) -> Option<(usize, u16, u16)> {
        let mut cap_base = EXT_CAP_START;
        for i in 0..self.extended_capabilities.len() {
            let cap_size =
                Self::validated_extended_cap_len_bytes(self.extended_capabilities[i].as_ref())
                    as u16;
            if offset < cap_base + cap_size {
                return Some((i, offset - cap_base, cap_base));
            }
            cap_base += cap_size;
            assert!(
                cap_base <= EXT_CAP_END,
                "extended capabilities exceed config space window {:#x}..{:#x} (exclusive end), cap_base={:#x}",
                EXT_CAP_START,
                EXT_CAP_END,
                cap_base
            );
        }
        None
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
            BarMemoryKind::Intercept(control) => {
                // Some `ControlMmioIntercept` implementations are not idempotent
                // and panic if `unmap()` is called while the region is not
                // mapped -- which happens when a device is torn down before the
                // guest ever enables memory space. Only unmap when mapped.
                if control.addr().is_some() {
                    control.unmap();
                }
            }
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
        extended_capabilities: Vec<Box<dyn PciExtendedCapability>>,
        bars: DeviceBars,
    ) -> Self {
        let common = ConfigSpaceCommonHeaderEmulator::new(
            hardware_ids,
            capabilities,
            extended_capabilities,
            bars,
        );

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

    /// Returns the currently captured bus number.
    pub fn captured_bus_number(&self) -> u8 {
        self.common.captured_bus_number()
    }

    /// Returns the currently captured devfn (device << 3 | function) number.
    pub fn captured_devfn(&self) -> u8 {
        self.common.captured_devfn()
    }

    /// Resets the configuration space state.
    pub fn reset(&mut self) {
        self.common.reset();
        self.state = ConfigSpaceType0EmulatorState::new();
    }

    /// Read from the config space.
    pub fn read(&self, address: PciConfigAddress, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        use cfg_space::HeaderType00;
        let offset = address.byte_offset();

        // First try to handle with common header emulator
        match self.common.read(address, value.reborrow()) {
            CommonHeaderResult::Handled => return IoResult::Ok,
            CommonHeaderResult::Failed(err) => return IoResult::Err(err),
            CommonHeaderResult::Unhandled => {
                // Continue with Type 0 specific handling
            }
        }

        // Handle Type 0 specific registers
        match HeaderType00(offset) {
            HeaderType00::BIST_HEADER => {
                let mut v = (self.state.latency_timer as u32) << 8;
                if self.common.multi_function_bit() {
                    // enable top-most bit of the header register
                    v |= 0x80 << 16;
                }
                value.set(v);
            }
            HeaderType00::CARDBUS_CIS_PTR => value.set(0),
            HeaderType00::SUBSYSTEM_ID => {
                value.set_low_high(
                    self.common.hardware_ids().type0_sub_vendor_id,
                    self.common.hardware_ids().type0_sub_system_id,
                );
            }
            HeaderType00::EXPANSION_ROM_BASE => value.set(0),
            HeaderType00::RESERVED => value.set(0),
            HeaderType00::LATENCY_INTERRUPT => {
                // Bits 7-0: Interrupt Line, Bits 15-8: Interrupt Pin, Bits 31-16: Latency Timer
                value.set(
                    (self.state.latency_timer as u32) << 16
                        | (self.common.interrupt_pin() as u32) << 8
                        | self.common.interrupt_line() as u32,
                );
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, "unexpected config space read");
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        IoResult::Ok
    }

    /// Read a byte-enabled DWORD from the config space. `offset` must be 32-bit aligned.
    pub fn read_byte_enabled(&self, offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        if !offset.is_multiple_of(4) {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        let Some(addr) = PciConfigAddress::new(0, 0, offset / 4) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        self.read(addr, value)
    }

    /// Write to the config space.
    pub fn write(&mut self, address: PciConfigAddress, val: ByteEnabledDwordWrite) -> IoResult {
        use cfg_space::HeaderType00;
        let offset = address.byte_offset();

        // First try to handle with common header emulator
        match self.common.write(address, val) {
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
                let low = val.merge_low(
                    (self.common.interrupt_pin() as u16) << 8 | self.common.interrupt_line() as u16,
                );
                self.common.set_interrupt_line(low as u8);
                self.state.latency_timer = val.merge_high(self.state.latency_timer as u16) as u8;
            }
            // all other base regs are noops
            _ if offset < COMMON_HEADER_END && offset.is_multiple_of(4) => (),
            _ => {
                tracelimit::warn_ratelimited!(offset, ?val, "unexpected config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    /// Write a byte-enabled DWORD from the config space. `offset` must be 32-bit aligned.
    pub fn write_byte_enabled(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        if !offset.is_multiple_of(4) {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        let Some(addr) = PciConfigAddress::new(0, 0, offset / 4) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        self.write(addr, value)
    }

    /// Finds a BAR + offset by address.
    pub fn find_bar(&self, address: u64) -> Option<(u8, u64)> {
        self.common.find_bar(address)
    }

    /// Gets the active base address for a specific BAR index, if mapped.
    pub fn bar_address(&self, bar: u8) -> Option<u64> {
        self.common.bar_address(bar)
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
    #[inspect(hex)]
    subordinate_bus_number: u8,
    /// The secondary bus number register. Software programs
    /// this register with the bus number assigned to the secondary
    /// side of the bridge.
    #[inspect(hex)]
    secondary_bus_number: u8,
    /// The primary bus number register. This is unused for PCI Express but
    /// is supposed to be read/write for compability with legacy software.
    #[inspect(hex)]
    primary_bus_number: u8,
    /// The memory base register. Software programs the upper 12 bits of this
    /// register with the upper 12 bits of a 32-bit base address of MMIO assigned
    /// to the hierarchy under the bridge (the lower 20 bits are assumed to be 0s).
    #[inspect(hex)]
    memory_base: u16,
    /// The memory limit register. Software programs the upper 12 bits of this
    /// register with the upper 12 bits of a 32-bit limit address of MMIO assigned
    /// to the hierarchy under the bridge (the lower 20 bits are assumed to be 1s).
    #[inspect(hex)]
    memory_limit: u16,
    /// The prefetchable memory base register. Software programs the upper 12 bits of
    /// this register with bits 20:31 of the base address of the prefetchable MMIO
    /// window assigned to the hierarchy under the bridge. Bits 0:19 are assumed to
    /// be 0s.
    #[inspect(hex)]
    prefetch_base: u16,
    /// The prefetchable memory limit register. Software programs the upper 12 bits of
    /// this register with bits 20:31 of the limit address of the prefetchable MMIO
    /// window assigned to the hierarchy under the bridge. Bits 0:19 are assumed to
    /// be 1s.
    #[inspect(hex)]
    prefetch_limit: u16,
    /// The prefetchable memory base upper 32 bits register. When the bridge supports
    /// 64-bit addressing for prefetchable memory, software programs this register
    /// with the upper 32 bits of the base address of the prefetchable MMIO window
    /// assigned to the hierarchy under the bridge.
    #[inspect(hex)]
    prefetch_base_upper: u32,
    /// The prefetchable memory limit upper 32 bits register. When the bridge supports
    /// 64-bit addressing for prefetchable memory, software programs this register
    /// with the upper 32 bits of the base address of the prefetchable MMIO window
    /// assigned to the hierarchy under the bridge.
    #[inspect(hex)]
    prefetch_limit_upper: u32,
    /// The bridge control register. Contains various control bits for bridge behavior
    /// such as secondary bus reset, VGA enable, etc.
    #[inspect(hex)]
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
    /// Shared bus range, synced automatically on writes, reset, and restore.
    #[inspect(skip)]
    bus_range: crate::bus_range::AssignedBusRange,
}

impl ConfigSpaceType1Emulator {
    /// Create a new [`ConfigSpaceType1Emulator`]
    pub fn new(
        hardware_ids: HardwareIds,
        capabilities: Vec<Box<dyn PciCapability>>,
        extended_capabilities: Vec<Box<dyn PciExtendedCapability>>,
    ) -> Self {
        Self::new_with_bars(
            hardware_ids,
            capabilities,
            extended_capabilities,
            DeviceBars::new(),
        )
    }

    /// Create a new [`ConfigSpaceType1Emulator`] with caller-specified BARs.
    pub fn new_with_bars(
        hardware_ids: HardwareIds,
        capabilities: Vec<Box<dyn PciCapability>>,
        extended_capabilities: Vec<Box<dyn PciExtendedCapability>>,
        bars: DeviceBars,
    ) -> Self {
        let common = ConfigSpaceCommonHeaderEmulator::new(
            hardware_ids,
            capabilities,
            extended_capabilities,
            bars,
        );

        Self {
            common,
            state: ConfigSpaceType1EmulatorState::new(),
            bus_range: crate::bus_range::AssignedBusRange::new(),
        }
    }

    /// Returns the currently captured bus number.
    pub fn captured_bus_number(&self) -> u8 {
        self.common.captured_bus_number()
    }

    /// Returns the currently captured devfn (device << 3 | function) number.
    pub fn captured_devfn(&self) -> u8 {
        self.common.captured_devfn()
    }

    /// Resets the configuration space state.
    pub fn reset(&mut self) {
        self.common.reset();
        self.state = ConfigSpaceType1EmulatorState::new();
        self.sync_bus_range();
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

    /// Returns a clone of the shared bus range.
    ///
    /// The returned handle shares the same underlying atomic — bus number
    /// changes from writes, resets, and restores are reflected automatically.
    pub fn bus_range(&self) -> crate::bus_range::AssignedBusRange {
        self.bus_range.clone()
    }

    /// Pushes the current secondary/subordinate bus numbers into the shared
    /// atomic so that consumers (ITS wrappers, SMMU) see the latest values.
    fn sync_bus_range(&self) {
        self.bus_range.set_bus_range(
            self.state.secondary_bus_number,
            self.state.subordinate_bus_number,
        );
    }

    fn decode_memory_range(&self, base_register: u16, limit_register: u16) -> (u32, u32) {
        let base_addr = u32::from(base_register) << 16;
        let limit_addr = (u32::from(limit_register) << 16) | 0xF_FFFF;
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

    /// Read from the config space.
    pub fn read(&self, address: PciConfigAddress, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        use cfg_space::HeaderType01;
        let offset = address.byte_offset();

        // First try to handle with common header emulator
        match self.common.read(address, value.reborrow()) {
            CommonHeaderResult::Handled => return IoResult::Ok,
            CommonHeaderResult::Failed(err) => return IoResult::Err(err),
            CommonHeaderResult::Unhandled => {
                // Continue with Type 1 specific handling
            }
        }

        // Handle Type 1 specific registers
        match HeaderType01(offset) {
            HeaderType01::BIST_HEADER => {
                // Header type 01 with optional multi-function bit
                value.set(if self.common.multi_function_bit() {
                    0x00810000 // Header type 01 with multi-function bit (bit 23)
                } else {
                    0x00010000 // Header type 01 without multi-function bit
                });
            }
            HeaderType01::LATENCY_BUS_NUMBERS => {
                value.set_bytes(
                    self.state.primary_bus_number,
                    self.state.secondary_bus_number,
                    self.state.subordinate_bus_number,
                    0,
                );
            }
            HeaderType01::SEC_STATUS_IO_RANGE => value.set(0),
            HeaderType01::MEMORY_RANGE => {
                value.set_low_high(self.state.memory_base, self.state.memory_limit)
            }
            HeaderType01::PREFETCH_RANGE => {
                // Set the low bit in both the limit and base registers to indicate
                // support for 64-bit addressing.
                value.set_low_high(
                    self.state.prefetch_base | cfg_space::PREFETCH_MEMORY_BASE_LIMIT_64BIT,
                    self.state.prefetch_limit | cfg_space::PREFETCH_MEMORY_BASE_LIMIT_64BIT,
                )
            }
            HeaderType01::PREFETCH_BASE_UPPER => value.set(self.state.prefetch_base_upper),
            HeaderType01::PREFETCH_LIMIT_UPPER => value.set(self.state.prefetch_limit_upper),
            HeaderType01::IO_RANGE_UPPER => value.set(0),
            HeaderType01::EXPANSION_ROM_BASE => value.set(0),
            HeaderType01::BRDIGE_CTRL_INTERRUPT => {
                // Read interrupt line from common header and bridge control from state
                // Bits 7-0: Interrupt Line, Bits 15-8: Interrupt Pin (0), Bits 31-16: Bridge Control
                value.set_low_high(
                    self.common.interrupt_line() as u16,
                    self.state.bridge_control,
                )
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, "unexpected config space read");
                return IoResult::Err(IoError::InvalidRegister);
            }
        };

        IoResult::Ok
    }

    /// Read a byte-enabled DWORD from the config space. `offset` must be 32-bit aligned.
    pub fn read_byte_enabled(&self, offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        if !offset.is_multiple_of(4) {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        let Some(addr) = PciConfigAddress::new(0, 0, offset / 4) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        self.read(addr, value)
    }

    /// Write to the config space.
    pub fn write(&mut self, address: PciConfigAddress, val: ByteEnabledDwordWrite) -> IoResult {
        use cfg_space::HeaderType01;
        let offset = address.byte_offset();

        // First try to handle with common header emulator
        match self.common.write(address, val) {
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
                let current = (self.state.subordinate_bus_number as u32) << 16
                    | (self.state.secondary_bus_number as u32) << 8
                    | self.state.primary_bus_number as u32;
                let val = val.merge(current);
                self.state.subordinate_bus_number = (val >> 16) as u8;
                self.state.secondary_bus_number = (val >> 8) as u8;
                self.state.primary_bus_number = val as u8;
                self.sync_bus_range();
            }
            HeaderType01::MEMORY_RANGE => {
                self.state.memory_base = val.merge_low(self.state.memory_base)
                    & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK;
                self.state.memory_limit = val.merge_high(self.state.memory_limit)
                    & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK;
            }
            HeaderType01::PREFETCH_RANGE => {
                self.state.prefetch_base = val.merge_low(self.state.prefetch_base)
                    & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK;
                self.state.prefetch_limit = val.merge_high(self.state.prefetch_limit)
                    & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK;
            }
            HeaderType01::PREFETCH_BASE_UPPER => {
                val.merge_into(&mut self.state.prefetch_base_upper);
            }
            HeaderType01::PREFETCH_LIMIT_UPPER => {
                val.merge_into(&mut self.state.prefetch_limit_upper);
            }
            HeaderType01::BRDIGE_CTRL_INTERRUPT => {
                // Delegate interrupt line writes to common header and store bridge control
                // Bits 7-0: Interrupt Line, Bits 15-8: Interrupt Pin (ignored), Bits 31-16: Bridge Control
                self.common
                    .set_interrupt_line(val.merge_low(self.common.interrupt_line() as u16) as u8);
                self.state.bridge_control = val.merge_high(self.state.bridge_control);
            }
            // all other base regs are noops
            _ if offset < COMMON_HEADER_END && offset.is_multiple_of(4) => (),
            _ => {
                tracelimit::warn_ratelimited!(offset, ?val, "unexpected config space write");
                return IoResult::Err(IoError::InvalidRegister);
            }
        }

        IoResult::Ok
    }

    /// Write a byte-enabled DWORD to the config space. `offset` must be 32-bit aligned.
    pub fn write_byte_enabled(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        if !offset.is_multiple_of(4) {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        let Some(addr) = PciConfigAddress::new(0, 0, offset / 4) else {
            return IoResult::Err(IoError::InvalidRegister);
        };

        self.write(addr, value)
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

    /// Get the list of PCI capabilities.
    pub fn capabilities(&self) -> &[Box<dyn PciCapability>] {
        self.common.capabilities()
    }

    /// Get the list of PCI capabilities (mutable).
    pub fn capabilities_mut(&mut self) -> &mut [Box<dyn PciCapability>] {
        self.common.capabilities_mut()
    }

    /// Finds a BAR + offset by address.
    pub fn find_bar(&self, address: u64) -> Option<(u8, u64)> {
        self.common.find_bar(address)
    }

    /// Gets the active base address for a specific BAR index, if mapped.
    pub fn bar_address(&self, bar: u8) -> Option<u64> {
        self.common.bar_address(bar)
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

        /// Unified saved state for both Type 0 and Type 1 PCI configuration space emulators.
        /// Type 1 specific fields (mesh indices 6-15) will be ignored when restoring Type 0 devices,
        /// and will have default values (0) when restoring old save state to Type 1 devices.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.cfg_space_emu")]
        pub struct SavedState {
            // Common fields (used by both Type 0 and Type 1)
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
            #[mesh(16)]
            pub extended_capabilities: Vec<(String, SavedStateBlob)>,
            #[mesh(17)]
            pub captured_bus_number: u8,
            #[mesh(18)]
            pub captured_devfn: u8,

            // Type 1 specific fields (bridge devices)
            // These fields default to 0 for backward compatibility with old save state
            #[mesh(6)]
            pub subordinate_bus_number: u8,
            #[mesh(7)]
            pub secondary_bus_number: u8,
            #[mesh(8)]
            pub primary_bus_number: u8,
            #[mesh(9)]
            pub memory_base: u16,
            #[mesh(10)]
            pub memory_limit: u16,
            #[mesh(11)]
            pub prefetch_base: u16,
            #[mesh(12)]
            pub prefetch_limit: u16,
            #[mesh(13)]
            pub prefetch_base_upper: u32,
            #[mesh(14)]
            pub prefetch_limit_upper: u32,
            #[mesh(15)]
            pub bridge_control: u16,
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
            let ConfigSpaceType0EmulatorState { latency_timer } = self.state;

            let saved_state = state::SavedState {
                command: self.common.command().into_bits(),
                base_addresses: *self.common.base_addresses(),
                interrupt_line: self.common.interrupt_line(),
                latency_timer,
                capabilities: self
                    .common
                    .capabilities_mut()
                    .iter_mut()
                    .map(|cap| {
                        let id = cap.label().to_owned();
                        Ok((id, cap.save()?))
                    })
                    .collect::<Result<_, _>>()?,
                extended_capabilities: self
                    .common
                    .extended_capabilities
                    .iter_mut()
                    .map(|cap| {
                        let id = cap.label().to_owned();
                        Ok((id, cap.save()?))
                    })
                    .collect::<Result<_, _>>()?,
                captured_bus_number: self.common.captured_bus_number(),
                captured_devfn: self.common.captured_devfn(),
                // Type 1 specific fields - not used for Type 0
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
                extended_capabilities,
                captured_bus_number,
                captured_devfn,
                // Type 1 specific fields - ignored for Type 0
                subordinate_bus_number: _,
                secondary_bus_number: _,
                primary_bus_number: _,
                memory_base: _,
                memory_limit: _,
                prefetch_base: _,
                prefetch_limit: _,
                prefetch_base_upper: _,
                prefetch_limit_upper: _,
                bridge_control: _,
            } = state;

            self.state = ConfigSpaceType0EmulatorState { latency_timer };

            self.common.set_base_addresses(&base_addresses);
            self.common.set_interrupt_line(interrupt_line);
            self.common
                .set_command(cfg_space::Command::from_bits(command));

            if command & !SUPPORTED_COMMAND_BITS != 0 {
                return Err(RestoreError::InvalidSavedState(
                    ConfigSpaceRestoreError::InvalidConfigBits.into(),
                ));
            }

            self.common.sync_command_register(self.common.command());

            for (id, entry) in capabilities {
                tracing::debug!(save_id = id.as_str(), "restoring pci capability");

                // yes, yes, this is O(n^2), but devices never have more than a
                // handful of caps, so it's totally fine.
                let mut restored = false;
                for cap in self.common.capabilities_mut() {
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

            for (id, entry) in extended_capabilities {
                tracing::debug!(save_id = id.as_str(), "restoring pci extended capability");

                let mut restored = false;
                for cap in &mut self.common.extended_capabilities {
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

            self.common.set_captured_bus_number(captured_bus_number);
            self.common.set_captured_devfn(captured_devfn);

            Ok(())
        }
    }

    impl SaveRestore for ConfigSpaceType1Emulator {
        type SavedState = state::SavedState;

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

            // Pad base_addresses to 6 elements for saved state (Type 1 uses 2 BARs)
            let type1_base_addresses = self.common.base_addresses();
            let mut saved_base_addresses = [0u32; 6];
            saved_base_addresses[0] = type1_base_addresses[0];
            saved_base_addresses[1] = type1_base_addresses[1];

            let saved_state = state::SavedState {
                command: self.common.command().into_bits(),
                base_addresses: saved_base_addresses,
                interrupt_line: self.common.interrupt_line(),
                latency_timer: 0, // Not used for Type 1
                capabilities: self
                    .common
                    .capabilities_mut()
                    .iter_mut()
                    .map(|cap| {
                        let id = cap.label().to_owned();
                        Ok((id, cap.save()?))
                    })
                    .collect::<Result<_, _>>()?,
                extended_capabilities: self
                    .common
                    .extended_capabilities
                    .iter_mut()
                    .map(|cap| {
                        let id = cap.label().to_owned();
                        Ok((id, cap.save()?))
                    })
                    .collect::<Result<_, _>>()?,
                captured_bus_number: self.common.captured_bus_number(),
                captured_devfn: self.common.captured_devfn(),
                // Type 1 specific fields
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

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                command,
                base_addresses,
                interrupt_line,
                latency_timer: _, // Not used for Type 1
                capabilities,
                extended_capabilities,
                captured_bus_number,
                captured_devfn,
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
            } = state;

            self.state = ConfigSpaceType1EmulatorState {
                subordinate_bus_number,
                secondary_bus_number,
                primary_bus_number,
                memory_base: memory_base & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK,
                memory_limit: memory_limit & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK,
                prefetch_base: prefetch_base & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK,
                prefetch_limit: prefetch_limit & cfg_space::MEMORY_BASE_LIMIT_ADDRESS_MASK,
                prefetch_base_upper,
                prefetch_limit_upper,
                bridge_control,
            };

            self.sync_bus_range();

            // Pad base_addresses to 6 elements for common header (Type 1 uses 2 BARs)
            let mut full_base_addresses = [0u32; 6];
            for (i, &addr) in base_addresses.iter().enumerate().take(2) {
                full_base_addresses[i] = addr;
            }
            self.common
                .set_base_addresses(&[full_base_addresses[0], full_base_addresses[1]]);
            self.common.set_interrupt_line(interrupt_line);
            self.common
                .set_command(cfg_space::Command::from_bits(command));

            if command & !SUPPORTED_COMMAND_BITS != 0 {
                return Err(RestoreError::InvalidSavedState(
                    ConfigSpaceRestoreError::InvalidConfigBits.into(),
                ));
            }

            self.common.sync_command_register(self.common.command());

            for (id, entry) in capabilities {
                tracing::debug!(save_id = id.as_str(), "restoring pci capability");

                let mut restored = false;
                for cap in self.common.capabilities_mut() {
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

            for (id, entry) in extended_capabilities {
                tracing::debug!(save_id = id.as_str(), "restoring pci extended capability");

                let mut restored = false;
                for cap in &mut self.common.extended_capabilities {
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

            self.common.set_captured_bus_number(captured_bus_number);
            self.common.set_captured_devfn(captured_devfn);

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::extended::acs::AcsExtendedCapability;
    use crate::capabilities::pci_express::PciExpressCapability;
    use crate::capabilities::read_only::ReadOnlyCapability;
    use crate::spec::caps::pci_express::DevicePortType;
    use crate::spec::hwid::ClassCode;
    use crate::spec::hwid::ProgrammingInterface;
    use crate::spec::hwid::Subclass;
    use crate::test_helpers::TestCfgAccess;
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigByteEnable;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use vmcore::save_restore::SaveRestore;

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
            vec![],
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
            vec![],
        )
    }

    #[test]
    fn test_type1_probe() {
        let emu = create_type1_emulator(vec![]);
        assert_eq!(emu.read_u32(0), 0x2222_1111);
        assert_eq!(emu.read_u32(4) & 0x10_0000, 0); // Capabilities pointer

        let emu = create_type1_emulator(vec![Box::new(ReadOnlyCapability::new("foo", 0))]);
        assert_eq!(emu.read_u32(0), 0x2222_1111);
        assert_eq!(emu.read_u32(4) & 0x10_0000, 0x10_0000); // Capabilities pointer
    }

    #[test]
    fn test_type1_bus_number_assignment() {
        let mut emu = create_type1_emulator(vec![]);

        // The bus number (and latency timer) registers are
        // all default 0.
        assert_eq!(emu.read_u32(0x18), 0);
        assert_eq!(emu.assigned_bus_range(), 0..=0);

        // The bus numbers can be programmed one by one,
        // and the range may not be valid during the middle
        // of allocation.
        emu.write_u32(0x18, 0x0000_1000);
        assert_eq!(emu.read_u32(0x18), 0x0000_1000);
        assert_eq!(emu.assigned_bus_range(), 0..=0);
        emu.write_u32(0x18, 0x0012_1000);
        assert_eq!(emu.read_u32(0x18), 0x0012_1000);
        assert_eq!(emu.assigned_bus_range(), 0x10..=0x12);

        // The primary bus number register is read/write for compatability
        // but unused.
        emu.write_u32(0x18, 0x0012_1033);
        assert_eq!(emu.read_u32(0x18), 0x0012_1033);
        assert_eq!(emu.assigned_bus_range(), 0x10..=0x12);

        // Software can also just write the entire 4byte value at once
        emu.write_u32(0x18, 0x0047_4411);
        assert_eq!(emu.read_u32(0x18), 0x0047_4411);
        assert_eq!(emu.assigned_bus_range(), 0x44..=0x47);

        // The subordinate bus number can equal the secondary bus number...
        emu.write_u32(0x18, 0x0088_8800);
        assert_eq!(emu.assigned_bus_range(), 0x88..=0x88);

        // ... but it cannot be less, that's a confused guest OS.
        emu.write_u32(0x18, 0x0087_8800);
        assert_eq!(emu.assigned_bus_range(), 0..=0);
    }

    #[test]
    fn test_type1_bus_number_byte_writes() {
        let mut emu = create_type1_emulator(vec![]);

        emu.write(
            PciConfigAddress::new(0, 0, 0x18 / 4).unwrap(),
            ByteEnabledDwordWrite::new(
                0x0000_0011,
                PciConfigByteEnable::from_offset_len(0x18, 1).unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(emu.read_u32(0x18), 0x0000_0011);
        assert_eq!(emu.assigned_bus_range(), 0..=0);

        emu.write(
            PciConfigAddress::new(0, 0, 0x18 / 4).unwrap(),
            ByteEnabledDwordWrite::new(
                0x0000_2200,
                PciConfigByteEnable::from_offset_len(0x19, 1).unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(emu.read_u32(0x18), 0x0000_2211);
        assert_eq!(emu.assigned_bus_range(), 0..=0);

        emu.write(
            PciConfigAddress::new(0, 0, 0x18 / 4).unwrap(),
            ByteEnabledDwordWrite::new(
                0x0033_0000,
                PciConfigByteEnable::from_offset_len(0x1a, 1).unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(emu.read_u32(0x18), 0x0033_2211);
        assert_eq!(emu.assigned_bus_range(), 0x22..=0x33);

        emu.write(
            PciConfigAddress::new(0, 0, 0x18 / 4).unwrap(),
            ByteEnabledDwordWrite::new(
                0xff00_0000,
                PciConfigByteEnable::from_offset_len(0x1b, 1).unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(emu.read_u32(0x18), 0x0033_2211);
        assert_eq!(emu.assigned_bus_range(), 0x22..=0x33);
    }

    #[test]
    fn test_type1_memory_assignment() {
        const MMIO_ENABLED: u32 = 0x0000_0002;
        const MMIO_DISABLED: u32 = 0x0000_0000;

        let mut emu = create_type1_emulator(vec![]);
        assert!(emu.assigned_memory_range().is_none());

        // The guest can write whatever it wants while MMIO
        // is disabled.
        emu.write_u32(0x20, 0xDEAD_BEEF);
        assert!(emu.assigned_memory_range().is_none());

        // The guest can program a valid resource assignment...
        emu.write_u32(0x20, 0xFFF0_FF00);
        assert!(emu.assigned_memory_range().is_none());
        // ... enable memory decoding...
        emu.write_u32(0x4, MMIO_ENABLED);
        assert_eq!(emu.assigned_memory_range(), Some(0xFF00_0000..=0xFFFF_FFFF));
        // ... then disable memory decoding it.
        emu.write_u32(0x4, MMIO_DISABLED);
        assert!(emu.assigned_memory_range().is_none());

        // Setting memory base equal to memory limit is a valid 1MB range.
        emu.write_u32(0x20, 0xBBB0_BBB0);
        emu.write_u32(0x4, MMIO_ENABLED);
        assert_eq!(emu.assigned_memory_range(), Some(0xBBB0_0000..=0xBBBF_FFFF));
        emu.write_u32(0x4, MMIO_DISABLED);
        assert!(emu.assigned_memory_range().is_none());

        // The guest can try to program an invalid assignment (base > limit), we
        // just won't decode it.
        emu.write_u32(0x20, 0xAA00_BB00);
        assert!(emu.assigned_memory_range().is_none());
        emu.write_u32(0x4, MMIO_ENABLED);
        assert!(emu.assigned_memory_range().is_none());
        emu.write_u32(0x4, MMIO_DISABLED);
        assert!(emu.assigned_memory_range().is_none());
    }

    #[test]
    fn test_type1_memory_range_register_masks_reserved_bits() {
        const MMIO_ENABLED: u32 = 0x0000_0002;

        let mut emu = create_type1_emulator(vec![]);

        emu.write_u32(0x20, 0x567f_123f);
        assert_eq!(emu.read_u32(0x20), 0x5670_1230);

        emu.write_u32(0x4, MMIO_ENABLED);
        assert_eq!(emu.assigned_memory_range(), Some(0x1230_0000..=0x567f_ffff));
    }

    #[test]
    fn test_type1_prefetch_assignment() {
        const MMIO_ENABLED: u32 = 0x0000_0002;
        const MMIO_DISABLED: u32 = 0x0000_0000;

        let mut emu = create_type1_emulator(vec![]);
        assert!(emu.assigned_prefetch_range().is_none());

        // The guest can program a valid prefetch range...
        emu.write_u32(0x24, 0xFFF0_FF00); // limit + base
        emu.write_u32(0x28, 0x00AA_BBCC); // base upper
        emu.write_u32(0x2C, 0x00DD_EEFF); // limit upper
        assert!(emu.assigned_prefetch_range().is_none());
        // ... enable memory decoding...
        emu.write_u32(0x4, MMIO_ENABLED);
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x00AA_BBCC_FF00_0000..=0x00DD_EEFF_FFFF_FFFF)
        );
        // ... then disable memory decoding it.
        emu.write_u32(0x4, MMIO_DISABLED);
        assert!(emu.assigned_prefetch_range().is_none());

        // The validity of the assignment is determined using the combined 64-bit
        // address, not the lower bits or the upper bits in isolation.

        // Lower bits of the limit are greater than the lower bits of the
        // base, but the upper bits make that valid.
        emu.write_u32(0x24, 0xFF00_FFF0); // limit + base
        emu.write_u32(0x28, 0x00AA_BBCC); // base upper
        emu.write_u32(0x2C, 0x00DD_EEFF); // limit upper
        assert!(emu.assigned_prefetch_range().is_none());
        emu.write_u32(0x4, MMIO_ENABLED);
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x00AA_BBCC_FFF0_0000..=0x00DD_EEFF_FF0F_FFFF)
        );
        emu.write_u32(0x4, MMIO_DISABLED);
        assert!(emu.assigned_prefetch_range().is_none());

        // The base can equal the limit, which is a valid 1MB range.
        emu.write_u32(0x24, 0xDD00_DD00); // limit + base
        emu.write_u32(0x28, 0x00AA_BBCC); // base upper
        emu.write_u32(0x2C, 0x00AA_BBCC); // limit upper
        assert!(emu.assigned_prefetch_range().is_none());
        emu.write_u32(0x4, MMIO_ENABLED);
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x00AA_BBCC_DD00_0000..=0x00AA_BBCC_DD0F_FFFF)
        );
        emu.write_u32(0x4, MMIO_DISABLED);
        assert!(emu.assigned_prefetch_range().is_none());
    }

    #[test]
    fn test_type1_prefetch_range_register_masks_reserved_bits_and_reports_64_bit() {
        const MMIO_ENABLED: u32 = 0x0000_0002;

        let mut emu = create_type1_emulator(vec![]);

        emu.write_u32(0x24, 0x567e_123e);
        assert_eq!(emu.read_u32(0x24), 0x5671_1231);

        emu.write_u32(0x4, MMIO_ENABLED);
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x1230_0000..=0x567f_ffff)
        );
    }

    #[test]
    fn test_type1_restore_masks_bridge_memory_range_reserved_bits() {
        const MMIO_ENABLED: u32 = 0x0000_0002;

        let mut source = create_type1_emulator(vec![]);
        source.write_u32(0x4, MMIO_ENABLED);
        source.state.memory_base = 0x123f;
        source.state.memory_limit = 0x567f;
        source.state.prefetch_base = 0x234e;
        source.state.prefetch_limit = 0x678e;

        let saved_state = source.save().expect("save should succeed");

        let mut emu = create_type1_emulator(vec![]);
        emu.restore(saved_state).expect("restore should succeed");

        assert_eq!(emu.read_u32(0x20), 0x5670_1230);
        assert_eq!(emu.read_u32(0x24), 0x6781_2341);
        assert_eq!(emu.assigned_memory_range(), Some(0x1230_0000..=0x567f_ffff));
        assert_eq!(
            emu.assigned_prefetch_range(),
            Some(0x2340_0000..=0x678f_ffff)
        );
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
            vec![],
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
            vec![],
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
            vec![],
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
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], vec![], bars);

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
                vec![],
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
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], vec![], bars);

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
            ConfigSpaceCommonHeaderEmulator::new(hardware_ids, vec![], vec![], bars);

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
            vec![],
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
            vec![],
            DeviceBars::new(),
        );
        assert!(common_emu_pcie.is_pcie_device());

        // A non-PCIe device has no extended configuration space, but the
        // function is present: in-range reads return 0 (no extended caps),
        // not all-ones.
        let mut value = 0xdead_beef;
        assert!(matches!(
            common_emu_no_pcie.read_extended_capabilities(
                EXT_CAP_START,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!(value, 0);

        // A PCIe device with no extended capabilities returns an all-zero
        // header, terminating the list.
        let mut value = 0xdead_beef;
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(
                EXT_CAP_START,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!(value, 0);

        // Writes to the (unimplemented) extended region on a non-PCIe device
        // are dropped silently rather than faulting.
        assert!(matches!(
            common_emu_no_pcie.write_extended_capabilities(
                EXT_CAP_START,
                ByteEnabledDwordWrite::with_all_bytes_enabled(0x1234)
            ),
            CommonHeaderResult::Handled
        ));

        // Test writing extended capabilities - PCIe device should accept writes
        assert!(matches!(
            common_emu_pcie.write_extended_capabilities(
                EXT_CAP_START,
                ByteEnabledDwordWrite::with_all_bytes_enabled(0x1234)
            ),
            CommonHeaderResult::Handled
        ));

        // Test invalid offset ranges
        let mut value = 0;
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(
                0x99,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        ));
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(
                EXT_CAP_END,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Failed(IoError::InvalidRegister)
        ));
    }

    #[test]
    fn test_unimplemented_capability_region_reads_zero() {
        // Unimplemented registers in the standard capability region of a
        // present function read as 0.
        let mut common_emu = ConfigSpaceCommonHeaderEmulatorType0::new(
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
            // A single small capability at the start of the region; everything
            // past it is unimplemented.
            vec![Box::new(ReadOnlyCapability::new("foo", 0))],
            vec![],
            DeviceBars::new(),
        );

        // An offset well past the implemented capability reads as 0.
        let mut value = 0xdead_beef;
        assert!(matches!(
            common_emu.read_capabilities(
                0x90,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!(value, 0);

        // Writes to the unimplemented region are dropped silently.
        assert!(matches!(
            common_emu
                .write_capabilities(0x90, ByteEnabledDwordWrite::with_all_bytes_enabled(0x1234)),
            CommonHeaderResult::Handled
        ));
    }

    #[test]
    fn test_type1_acs_extended_capability() {
        let mut common_emu_pcie = ConfigSpaceCommonHeaderEmulatorType1::new(
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
            vec![Box::new(PciExpressCapability::new(
                DevicePortType::RootPort,
                None,
            ))],
            vec![Box::new(AcsExtendedCapability::new())],
            DeviceBars::new(),
        );

        let mut value = 0;
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(
                EXT_CAP_START,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!(value, 0x0001_000d);

        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(
                0x104,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!(value as u16, 0x005f);
        assert_eq!((value >> 16) as u16, 0x0000);

        assert!(matches!(
            common_emu_pcie.write_extended_capabilities(
                0x104,
                ByteEnabledDwordWrite::with_all_bytes_enabled(0xffff_0000),
            ),
            CommonHeaderResult::Handled
        ));
        assert!(matches!(
            common_emu_pcie.read_extended_capabilities(
                0x104,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value)
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!((value >> 16) as u16, 0x005f);
    }

    #[test]
    fn test_type0_emulator_save_restore() {
        // Test Type 0 emulator save/restore
        let mut emu = create_type0_emulator(vec![]);

        // Modify some state by writing to command register
        emu.write_u32(0x04, 0x0007); // Enable some command bits

        // Read back and verify
        assert_eq!(emu.read_u32(0x04) & 0x0007, 0x0007);

        // Write to latency timer / interrupt register
        emu.write_u32(0x3C, 0x0040_0000); // Set latency_timer

        // Save the state
        let saved_state = emu.save().expect("save should succeed");

        // Reset the emulator
        emu.reset();

        // Verify state is reset
        assert_eq!(emu.read_u32(0x04) & 0x0007, 0x0000); // Should be reset

        // Restore the state
        emu.restore(saved_state).expect("restore should succeed");

        // Verify state is restored
        assert_eq!(emu.read_u32(0x04) & 0x0007, 0x0007); // Should be restored
    }

    #[test]
    fn test_type1_emulator_save_restore() {
        // Test Type 1 emulator save/restore
        let mut emu = create_type1_emulator(vec![]);

        // Modify some state
        emu.write_u32(0x04, 0x0003); // Enable command bits
        emu.write_u32(0x18, 0x0012_1000); // Set bus numbers
        emu.write_u32(0x20, 0xFFF0_FF00); // Set memory range
        emu.write_u32(0x24, 0xFFF0_FF00); // Set prefetch range
        emu.write_u32(0x28, 0x00AA_BBCC); // Set prefetch base upper
        emu.write_u32(0x2C, 0x00DD_EEFF); // Set prefetch limit upper
        emu.write_u32(0x3C, 0x0001_0000); // Set bridge control

        // Verify values
        assert_eq!(emu.read_u32(0x04) & 0x0003, 0x0003);
        assert_eq!(emu.read_u32(0x18), 0x0012_1000);
        assert_eq!(emu.read_u32(0x20), 0xFFF0_FF00);
        assert_eq!(emu.read_u32(0x28), 0x00AA_BBCC);
        assert_eq!(emu.read_u32(0x2C), 0x00DD_EEFF);
        assert_eq!(emu.read_u32(0x3C) >> 16, 0x0001); // bridge_control

        // Save the state
        let saved_state = emu.save().expect("save should succeed");

        // Reset the emulator
        emu.reset();

        // Verify state is reset
        let test_val = emu.read_u32(0x04);
        assert_eq!(test_val & 0x0003, 0x0000);
        let test_val = emu.read_u32(0x18);
        assert_eq!(test_val, 0x0000_0000);

        // Restore the state
        emu.restore(saved_state).expect("restore should succeed");

        // Verify state is restored
        assert_eq!(emu.read_u32(0x04) & 0x0003, 0x0003);
        assert_eq!(emu.read_u32(0x18), 0x0012_1000);
        assert_eq!(emu.read_u32(0x20), 0xFFF0_FF00);
        assert_eq!(emu.read_u32(0x28), 0x00AA_BBCC);
        assert_eq!(emu.read_u32(0x2C), 0x00DD_EEFF);
        assert_eq!(emu.read_u32(0x3C) >> 16, 0x0001); // bridge_control
    }

    #[test]
    fn test_type1_emulator_save_restore_with_extended_capabilities() {
        let mut emu = ConfigSpaceType1Emulator::new(
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
            vec![Box::new(PciExpressCapability::new(
                DevicePortType::RootPort,
                None,
            ))],
            vec![Box::new(AcsExtendedCapability::new())],
        );

        // Enable all supported ACS control bits.
        emu.write_u32(0x104, 0xffff_0000);

        assert_eq!((emu.read_u32(0x104) >> 16) as u16, 0x005f);

        let saved_state = emu.save().expect("save should succeed");

        emu.reset();
        assert_eq!((emu.read_u32(0x104) >> 16) as u16, 0);

        emu.restore(saved_state).expect("restore should succeed");
        assert_eq!((emu.read_u32(0x104) >> 16) as u16, 0x005f);
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
        let slot_status_val = emulator.read_u32(COMMON_HEADER_END + 0x18); // COMMON_HEADER_END (cap start) + 0x18 (slot control/status)
        let initial_presence_detect = (slot_status_val >> 22) & 0x1; // presence_detect_state is bit 6 of slot status
        assert_eq!(
            initial_presence_detect, 0,
            "Initial presence detect state should be 0"
        );

        // Set device as present
        emulator.set_presence_detect_state(true);
        let slot_status_val = emulator.read_u32(0x58);
        let present_presence_detect = (slot_status_val >> 22) & 0x1;
        assert_eq!(
            present_presence_detect, 1,
            "Presence detect state should be 1 when device is present"
        );

        // Set device as not present
        emulator.set_presence_detect_state(false);
        let slot_status_val = emulator.read_u32(0x58);
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
            vec![],
            DeviceBars::new(),
        );

        // Initially, no interrupt pin should be configured
        assert_eq!(emu.read_u32(0x3C) & 0xFF00, 0); // Interrupt pin should be 0

        // Configure interrupt pin A
        let line_interrupt = LineInterrupt::detached();
        emu.set_interrupt_pin(PciInterruptPin::IntA, line_interrupt);

        // Read the register again
        assert_eq!((emu.read_u32(0x3C) >> 8) & 0xFF, 1); // Interrupt pin should be 1 (INTA)

        // Set interrupt line to 0x42 and verify both pin and line are correct
        emu.write_u32(0x3C, 0x00110042); // Latency=0x11, pin=ignored, line=0x42
        let val = emu.read_u32(0x3C);
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
            vec![],
            DeviceBars::new(),
        );

        let line_interrupt_d = LineInterrupt::detached();
        emu_d.set_interrupt_pin(PciInterruptPin::IntD, line_interrupt_d);

        assert_eq!((emu_d.read_u32(0x3C) >> 8) & 0xFF, 4); // Interrupt pin should be 4 (INTD)
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

    /// Ensure that `find_bar` correctly returns a full `u64` offset for BARs
    /// larger than 64KiB, guarding against truncation back to `u16`.
    #[test]
    fn find_bar_returns_full_u64_offset_for_large_bar() {
        use crate::bar_mapping::BarMappings;

        // Set up a 64-bit BAR0 at base address 0x1_0000_0000 with size
        // 0x2_0000 (128KiB). The mask encodes the size via the complement:
        //   mask = !(size - 1) = !(0x1_FFFF) = 0xFFFF_FFFE_0000
        // Split across two 32-bit BAR registers (BAR0 low + BAR1 high).
        let bar_base: u64 = 0x1_0000_0000;
        let bar_size: u64 = 0x2_0000; // 128KiB — larger than u16::MAX
        let mask64 = !(bar_size - 1); // 0xFFFF_FFFE_0000

        let mut base_addresses = [0u32; 6];
        let mut bar_masks = [0u32; 6];

        // BAR0 low: set the 64-bit type bit in the mask and the base address.
        bar_masks[0] = cfg_space::BarEncodingBits::from_bits(mask64 as u32)
            .with_type_64_bit(true)
            .into_bits();
        bar_masks[1] = (mask64 >> 32) as u32;
        base_addresses[0] = bar_base as u32;
        base_addresses[1] = (bar_base >> 32) as u32;

        let bar_mappings = BarMappings::parse(&base_addresses, &bar_masks);

        // Query an address whose offset within BAR0 exceeds 0xFFFF.
        let expected_offset: u64 = 0x1_2345;
        let address: u64 = bar_base + expected_offset;

        let (found_bar, offset) = bar_mappings
            .find(address)
            .expect("address should resolve to BAR 0");
        assert_eq!(found_bar, 0);
        assert_eq!(offset, expected_offset);
    }

    #[test]
    fn test_odd_index_64bit_bar_preserves_attrs_only_on_lower_dword() {
        let mut bars = DeviceBars::new();
        bars.bars[1] = Some((4096, BarMemoryKind::Dummy));

        let mut common_emu = ConfigSpaceCommonHeaderEmulatorType0::new(
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
            vec![],
            bars,
        );

        // BAR1 is the lower dword of a 64-bit BAR and should preserve
        // encoding bits (type + prefetchable).
        assert!(matches!(
            common_emu.write(
                PciConfigAddress::new(0, 0, 0x14 / 4).unwrap(),
                ByteEnabledDwordWrite::with_all_bytes_enabled(0x1234_5000),
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!(common_emu.base_addresses()[1] & 0xF, 0xC);

        // BAR2 is the upper dword and must not be treated as encoding bits.
        assert!(matches!(
            common_emu.write(
                PciConfigAddress::new(0, 0, 0x18 / 4).unwrap(),
                ByteEnabledDwordWrite::with_all_bytes_enabled(0x89ab_cde5),
            ),
            CommonHeaderResult::Handled
        ));
        assert_eq!(common_emu.base_addresses()[2] & 0xF, 0x5);
    }

    #[test]
    fn test_32bit_bar_preserves_attr_bits_without_clobbering_address_bits() {
        let mut common_emu = ConfigSpaceCommonHeaderEmulatorType0::new(
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
            vec![],
            DeviceBars::new(),
        );

        // Force BAR0 to behave like a 32-bit mapped BAR with the prefetchable
        // bit set. This validates low-nibble preservation independent of
        // current DeviceBars construction defaults.
        common_emu.bar_masks[0] = 0xffff_fff0 | 0x8;
        common_emu.mapped_memory[0] = Some(BarMemoryKind::Dummy);

        assert!(matches!(
            common_emu.write(
                PciConfigAddress::new(0, 0, 0x10 / 4).unwrap(),
                ByteEnabledDwordWrite::with_all_bytes_enabled(0x1234_5670),
            ),
            CommonHeaderResult::Handled
        ));

        // Low nibble should retain BAR attribute bits from the mask while
        // higher address bits should come from the guest write and BAR mask.
        assert_eq!(common_emu.base_addresses()[0], 0x1234_5678);
    }

    // A `ControlMmioIntercept` test double that records map/unmap. Like some
    // real intercept implementations (e.g. the PCIe test intercept), its
    // `unmap()` panics if called while not mapped -- so these tests also verify
    // that teardown never unmaps a BAR that was never mapped.
    struct TrackingBar {
        len: u64,
        addr: Option<u64>,
        mapped: Arc<AtomicBool>,
    }

    impl ControlMmioIntercept for TrackingBar {
        fn region_name(&self) -> &str {
            "bar0"
        }
        fn map(&mut self, addr: u64) {
            self.addr = Some(addr);
            self.mapped.store(true, Ordering::SeqCst);
        }
        fn unmap(&mut self) {
            assert!(self.addr.is_some(), "unmap called while not mapped");
            self.addr = None;
            self.mapped.store(false, Ordering::SeqCst);
        }
        fn addr(&self) -> Option<u64> {
            self.addr
        }
        fn len(&self) -> u64 {
            self.len
        }
        fn offset_of(&self, addr: u64) -> Option<u64> {
            let base = self.addr?;
            (base..base + self.len).contains(&addr).then(|| addr - base)
        }
    }

    fn config_space_with_intercept_bar(
        mapped: Arc<AtomicBool>,
    ) -> ConfigSpaceCommonHeaderEmulatorType0 {
        let bars = DeviceBars::new().bar0(
            0x1000,
            BarMemoryKind::Intercept(Box::new(TrackingBar {
                len: 0x1000,
                addr: None,
                mapped,
            })),
        );
        ConfigSpaceCommonHeaderEmulatorType0::new(
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
            vec![],
            bars,
        )
    }

    // Regression test for a PCIe hot-add-after-remove failure: when a device's
    // config space is dropped (e.g. a hot-removed controller being torn down),
    // its BAR intercept registrations must be released. Otherwise the stale
    // range stays in the chipset's shared range map and a subsequent device
    // that reuses the same GPA fails to install its intercept, leaving its BAR
    // undispatched (guest reads all-1s -> stornvme FindAdapter reads CAP=~0).
    #[test]
    fn dropping_config_space_unmaps_bar_intercepts() {
        let mapped = Arc::new(AtomicBool::new(false));
        let mut common_emu = config_space_with_intercept_bar(mapped.clone());

        // Program BAR0's base address and enable memory space so the BAR
        // intercept is mapped into the chipset's range map.
        common_emu.set_base_addresses(&[0x2000_0000, 0, 0, 0, 0, 0]);
        common_emu.update_mmio_enabled(true);
        assert!(
            mapped.load(Ordering::SeqCst),
            "BAR intercept should be mapped once memory space is enabled"
        );

        // Dropping the config space (device teardown / hot-remove) must unmap
        // the BAR intercept so its range is released.
        drop(common_emu);
        assert!(
            !mapped.load(Ordering::SeqCst),
            "dropping config space must unmap its BAR intercepts"
        );
    }

    // A device can be torn down before the guest ever enables memory space (so
    // the BAR was never mapped). Dropping its config space must not attempt to
    // unmap the never-mapped intercept -- which would panic for intercept impls
    // whose `unmap()` is not idempotent (as `TrackingBar::unmap` asserts here).
    #[test]
    fn dropping_config_space_without_mmio_enabled_does_not_unmap() {
        let mapped = Arc::new(AtomicBool::new(false));
        let common_emu = config_space_with_intercept_bar(mapped.clone());

        // Never enabled memory space -> BAR never mapped. Dropping must be a
        // no-op for the intercept and must not panic.
        drop(common_emu);
        assert!(!mapped.load(Ordering::SeqCst));
    }

    #[test]
    fn test_type1_bdf_capturing() {
        // Test that the type1 config space emulator captures
        // the BDF of accesses it receives.
        let mut type1_emulator = create_type1_emulator(vec![]);

        // Initially, the captured BDF should be 0.
        assert_eq!(type1_emulator.captured_bus_number(), 0);
        assert_eq!(type1_emulator.captured_devfn(), 0);

        // Reads do not capture the BDF.
        let mut read_value = 0;
        let _ = type1_emulator.read(
            PciConfigAddress::new(1, 1, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut read_value),
        );
        assert_eq!(type1_emulator.captured_bus_number(), 0);
        assert_eq!(type1_emulator.captured_devfn(), 0);

        // Writes capture the BDF.
        let _ = type1_emulator.write(
            PciConfigAddress::new(1, 1, 0).unwrap(),
            ByteEnabledDwordWrite::with_all_bytes_enabled(0xdead_beef),
        );
        assert_eq!(type1_emulator.captured_bus_number(), 1);
        assert_eq!(type1_emulator.captured_devfn(), 1);

        // And writing a new BDF overwrites the old.
        let _ = type1_emulator.write(
            PciConfigAddress::new(4, 1, 0).unwrap(),
            ByteEnabledDwordWrite::with_all_bytes_enabled(0xdead_beef),
        );
        assert_eq!(type1_emulator.captured_bus_number(), 4);
        assert_eq!(type1_emulator.captured_devfn(), 1);

        // Save state with BDF captured.
        let saved_state = type1_emulator.save().expect("save should succeed");

        // Captured BDF should be cleared on reset.
        type1_emulator.reset();
        assert_eq!(type1_emulator.captured_bus_number(), 0);
        assert_eq!(type1_emulator.captured_devfn(), 0);

        // Restore the state, captured BDF should be restored.
        type1_emulator
            .restore(saved_state)
            .expect("restore should succeed");
        assert_eq!(type1_emulator.captured_bus_number(), 4);
        assert_eq!(type1_emulator.captured_devfn(), 1);
    }

    #[test]
    fn test_type0_bdf_capturing() {
        // Test that the type0 config space emulator captures
        // the BDF of accesses it receives.
        let mut type0_emulator = create_type0_emulator(vec![]);

        // Initially, the captured BDF should be 0.
        assert_eq!(type0_emulator.captured_bus_number(), 0);
        assert_eq!(type0_emulator.captured_devfn(), 0);

        // Reads do not capture the BDF.
        let mut read_value = 0;
        let _ = type0_emulator.read(
            PciConfigAddress::new(1, 1, 0).unwrap(),
            ByteEnabledDwordRead::with_all_bytes_enabled(&mut read_value),
        );
        assert_eq!(type0_emulator.captured_bus_number(), 0);
        assert_eq!(type0_emulator.captured_devfn(), 0);

        // Writes capture the BDF.
        let _ = type0_emulator.write(
            PciConfigAddress::new(1, 1, 0).unwrap(),
            ByteEnabledDwordWrite::with_all_bytes_enabled(0xdead_beef),
        );
        assert_eq!(type0_emulator.captured_bus_number(), 1);
        assert_eq!(type0_emulator.captured_devfn(), 1);

        // And writing a new BDF overwrites the old.
        let _ = type0_emulator.write(
            PciConfigAddress::new(4, 1, 0).unwrap(),
            ByteEnabledDwordWrite::with_all_bytes_enabled(0xdead_beef),
        );
        assert_eq!(type0_emulator.captured_bus_number(), 4);
        assert_eq!(type0_emulator.captured_devfn(), 1);

        // Save state with BDF captured.
        let saved_state = type0_emulator.save().expect("save should succeed");

        // Captured BDF should be cleared on reset.
        type0_emulator.reset();
        assert_eq!(type0_emulator.captured_bus_number(), 0);
        assert_eq!(type0_emulator.captured_devfn(), 0);

        // Restore the state, captured BDF should be restored.
        type0_emulator
            .restore(saved_state)
            .expect("restore should succeed");
        assert_eq!(type0_emulator.captured_bus_number(), 4);
        assert_eq!(type0_emulator.captured_devfn(), 1);
    }
}
