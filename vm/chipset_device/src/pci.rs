// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI configuration space access

use crate::ChipsetDevice;
use crate::io::IoError;
use crate::io::IoResult;
use inspect::Inspect;
use inspect::InspectMut;
use zerocopy::IntoBytes;

/// Byte enables for the four lanes of a PCI configuration DWORD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Inspect)]
pub struct PciConfigByteEnable(u8);

impl PciConfigByteEnable {
    /// All byte lanes enabled.
    pub const FULL: Self = Self(0b1111);

    /// Byte lane 0 enabled.
    pub const BYTE0: Self = Self(0b0001);
    /// Byte lane 1 enabled.
    pub const BYTE1: Self = Self(0b0010);
    /// Byte lane 2 enabled.
    pub const BYTE2: Self = Self(0b0100);
    /// Byte lane 3 enabled.
    pub const BYTE3: Self = Self(0b1000);
    /// Low word byte lanes enabled.
    pub const LOW_WORD: Self = Self(0b0011);
    /// High word byte lanes enabled.
    pub const HIGH_WORD: Self = Self(0b1100);

    /// Create byte enables from raw lane bits.
    pub const fn new(bits: u8) -> Option<Self> {
        if bits != 0 && bits <= 0xf {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Create byte enables for an access at `offset` with byte length `len`.
    pub const fn from_offset_len(offset: u16, len: usize) -> Result<Self, IoError> {
        let lane = (offset & 0x3) as u8;
        match len {
            1 => Ok(Self(1 << lane)),
            2 if lane & 1 == 0 && lane <= 2 => Ok(Self(0x3 << lane)),
            4 if lane == 0 => Ok(Self::FULL),
            2 | 4 => Err(IoError::UnalignedAccess),
            _ => Err(IoError::InvalidAccessSize),
        }
    }

    /// Returns the byte offset of the first enabled byte in the DWORD and the number of enabled bytes.
    pub const fn to_byte_offset_len(self) -> (u16, usize) {
        (self.0.trailing_zeros() as u16, self.0.count_ones() as usize)
    }

    /// Raw byte-lane bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns true if all byte lanes are enabled.
    pub const fn is_full(self) -> bool {
        self.0 == 0xf
    }

    /// `u32` mask corresponding to the enabled byte lanes.
    pub const fn mask(self) -> u32 {
        let mut mask = 0;
        let mut lane = 0;
        while lane < 4 {
            if self.0 & (1 << lane) != 0 {
                mask |= 0xff << (lane * 8);
            }
            lane += 1;
        }
        mask
    }

    /// Merge enabled byte lanes from `write_value` into `current_value`.
    pub const fn merge(self, current_value: u32, write_value: u32) -> u32 {
        let mask = self.mask();
        (current_value & !mask) | (write_value & mask)
    }

    /// Keep only enabled byte lanes from `value`.
    pub const fn extract(self, value: u32) -> u32 {
        value & self.mask()
    }

    /// Restrict the underlying byte enable to only include the provided bytes, or None
    /// when no bytes would be left enabled.
    pub const fn restrict(self, byte_enable: PciConfigByteEnable) -> Option<Self> {
        Self::new(self.0 & byte_enable.0)
    }

    /// Exclude the provided bytes from the underlying byte enable, returning None
    /// when no bytes would be left enabled.
    pub const fn exclude(self, byte_enable: PciConfigByteEnable) -> Option<Self> {
        Self::new(self.0 & !byte_enable.0)
    }
}

/// A DWORD value with byte enables for PCI configuration space write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Inspect)]
pub struct ByteEnabledDwordWrite {
    value: u32,
    byte_enable: PciConfigByteEnable,
}

impl ByteEnabledDwordWrite {
    /// Create a byte-enabled DWORD value.
    pub const fn new(value: u32, byte_enable: PciConfigByteEnable) -> Self {
        Self {
            value: byte_enable.merge(0, value),
            byte_enable,
        }
    }

    /// Create a full-DWORD value with all byte lanes enabled.
    pub const fn with_all_bytes_enabled(value: u32) -> Self {
        Self::new(value, PciConfigByteEnable::FULL)
    }

    /// Create a byte-enabled DWORD value from a slice of bytes.
    pub fn from_intercept_buffer(byte_enable: PciConfigByteEnable, buffer: &[u8]) -> Self {
        let mut temp: u32 = 0;
        let (byte_offset, len) = byte_enable.to_byte_offset_len();
        assert!(len <= buffer.len());
        let byte_offset = byte_offset as usize;
        temp.as_mut_bytes()[byte_offset..byte_offset + len].copy_from_slice(buffer);
        Self::new(temp, byte_enable)
    }

    /// Returns true if all byte lanes are enabled.
    pub const fn is_full(self) -> bool {
        self.byte_enable.is_full()
    }

    /// Get the mask of valid bytes.
    pub const fn valid_mask(self) -> u32 {
        self.byte_enable.mask()
    }

    /// Merge enabled byte lanes from this value into `current_value`.
    pub const fn merge(self, current_value: u32) -> u32 {
        self.byte_enable.merge(current_value, self.value)
    }

    /// Merge enabled byte lanes from this value into `current_value` (read-modify-write).
    pub const fn merge_into(self, current_value: &mut u32) {
        *current_value = self.merge(*current_value);
    }

    /// Merge enabled byte lanes from the low WORD of this value into `current_value`.
    pub const fn merge_low(self, current_value: u16) -> u16 {
        self.byte_enable.merge(current_value as u32, self.value) as u16
    }

    /// Merge enabled byte lanes from the high WORD of this value into `current_value`.
    pub const fn merge_high(self, current_value: u16) -> u16 {
        let shifted = (current_value as u32) << 16;
        let merged = self.byte_enable.merge(shifted, self.value);
        (merged >> 16) as u16
    }

    /// Keep only enabled byte lanes from this value.
    pub const fn extract(self) -> u32 {
        self.byte_enable.extract(self.value)
    }

    /// Keep only enabled byte lanes from the low WORD this value.
    pub const fn extract_low(self) -> u16 {
        self.extract() as u16
    }

    /// Keep only enabled byte lanes from the high WORD this value.
    pub const fn extract_high(self) -> u16 {
        (self.extract() >> 16) as u16
    }
}

/// A DWORD value with byte enables for PCI configuration space read.
#[derive(Debug, InspectMut)]
pub struct ByteEnabledDwordRead<'a> {
    value: &'a mut u32,
    byte_enable: PciConfigByteEnable,
}

impl<'a> ByteEnabledDwordRead<'a> {
    /// Create a byte-enabled DWORD value.
    pub const fn new(value: &'a mut u32, byte_enable: PciConfigByteEnable) -> Self {
        Self { value, byte_enable }
    }

    /// Create a full-DWORD value with all byte lanes enabled.
    pub const fn with_all_bytes_enabled(value: &'a mut u32) -> Self {
        Self::new(value, PciConfigByteEnable::FULL)
    }

    /// Retrieve the underlying byte enable.
    pub const fn byte_enable(&self) -> PciConfigByteEnable {
        self.byte_enable
    }

    /// Fill the intercept buffer with the enabled byte lanes of the DWORD.
    pub fn fill_intercept_buffer(self, buffer: &mut [u8]) {
        let (byte_offset, len) = self.byte_enable.to_byte_offset_len();
        let byte_offset = byte_offset as usize;
        let src = self.value.as_bytes()[byte_offset..byte_offset + len].as_ref();
        buffer.copy_from_slice(src);
    }

    /// Update the value of the DWORD, honoring byte enables.
    pub fn set(&mut self, value: u32) {
        *self.value = self.byte_enable.merge(*self.value, value);
    }

    /// Update the value of the DWORD, honoring byte enables.
    pub fn set_low_high(&mut self, low: u16, high: u16) {
        *self.value = self
            .byte_enable
            .merge(*self.value, (high as u32) << 16 | (low as u32));
    }

    /// Update the value of the DWORD, honoring byte enables.
    pub fn set_bytes(&mut self, byte0: u8, byte1: u8, byte2: u8, byte3: u8) {
        *self.value = self.byte_enable.merge(
            *self.value,
            (byte3 as u32) << 24 | (byte2 as u32) << 16 | (byte1 as u32) << 8 | (byte0 as u32),
        );
    }

    /// Keep only enabled byte lanes from this value.
    pub const fn extract(&self) -> u32 {
        self.byte_enable.extract(*self.value)
    }

    /// Keep only enabled byte lanes from the low WORD this value.
    pub const fn extract_low(self) -> u16 {
        self.extract() as u16
    }

    /// Keep only enabled byte lanes from the high WORD this value.
    pub const fn extract_high(self) -> u16 {
        (self.extract() >> 16) as u16
    }

    /// Reborrow the underlying value.
    pub fn reborrow(&mut self) -> ByteEnabledDwordRead<'_> {
        self.restrict(PciConfigByteEnable::FULL).unwrap()
    }

    /// Restrict the read to only the provided bytes.
    pub fn restrict(
        &mut self,
        byte_enable: PciConfigByteEnable,
    ) -> Option<ByteEnabledDwordRead<'_>> {
        let byte_enable = self.byte_enable.restrict(byte_enable)?;
        Some(ByteEnabledDwordRead::new(&mut *self.value, byte_enable))
    }

    /// Exclude the provided bytes from the read.
    pub fn exclude(
        &mut self,
        byte_enable: PciConfigByteEnable,
    ) -> Option<ByteEnabledDwordRead<'_>> {
        let byte_enable = self.byte_enable.exclude(byte_enable)?;
        Some(ByteEnabledDwordRead::new(&mut *self.value, byte_enable))
    }
}

/// A PCI configuration space request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Inspect)]
pub struct PciConfigAddress {
    /// Target bus number.
    pub bus: u8,
    /// Target packed device/function number (`device << 3 | function`).
    pub device_function: u8,
    /// Aligned DWORD register number in configuration space.
    dword_number: u16,
}

impl PciConfigAddress {
    /// Create a new PCI configuration-space request.
    pub const fn new(bus: u8, device_function: u8, dword_number: u16) -> Option<Self> {
        if dword_number >= 1024 {
            return None;
        }
        Some(Self {
            bus,
            device_function,
            dword_number,
        })
    }

    /// Target device number.
    pub const fn device(self) -> u8 {
        self.device_function >> 3
    }

    /// Target function number.
    pub const fn function(self) -> u8 {
        self.device_function & 0x7
    }

    /// Aligned byte offset of the addressed DWORD in configuration space.
    pub const fn byte_offset(self) -> u16 {
        self.dword_number * 4
    }
}

/// Implemented by devices which have a PCI config space.
pub trait PciConfigSpace: ChipsetDevice {
    /// Dispatch a PCI config space read to the device with the given address.
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult;
    /// Dispatch a PCI config space write to the device with the given address.
    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult;

    /// Handle a PCI configuration space read with full routing context.
    ///
    /// This method receives configuration space accesses with the target bus
    /// and function number. The interpretation of `function` depends on the
    /// bus topology: on a legacy PCI bus it carries packed device/function
    /// bits (0..=255), while downstream of a PCIe port the device number is
    /// always zero so all 8 bits represent functions within a single
    /// endpoint.
    ///
    /// A device can distinguish Type 0 (local) from Type 1 (forwarded)
    /// configuration cycles by comparing `target_bus` and `secondary_bus`:
    /// when they are equal the access targets this device directly (Type 0),
    /// otherwise it should be routed downstream (Type 1). An SR-IOV
    /// capable device can use `secondary_bus` together with `target_bus` and
    /// `function` to compute the VF number.
    ///
    /// The default implementation dispatches function 0 to
    /// [`pci_cfg_read`](Self::pci_cfg_read) and returns all-1s for other
    /// functions (the standard "no device present" response). Routing
    /// components (switches, bridges) and multi-function devices should
    /// override this method.
    ///
    /// # Parameters
    /// - `secondary_bus`: The secondary bus number of the downstream port
    ///   that forwarded this access
    /// - `target_bus`: The bus number targeted by the configuration access
    /// - `function`: Device/function identifier — packed device/function on
    ///   a legacy bus, or flat function number on PCIe
    /// - `offset`: Configuration space offset
    /// - `value`: Pointer to receive the read value
    fn pci_cfg_read_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: &mut u32,
    ) -> IoResult {
        if secondary_bus == target_bus && function == 0 {
            self.pci_cfg_read(offset, value)
        } else {
            *value = !0;
            IoResult::Ok
        }
    }

    /// Handle a PCI configuration space write with full routing context.
    ///
    /// This method receives configuration space accesses with the target bus
    /// and function number. The interpretation of `function` depends on the
    /// bus topology: on a legacy PCI bus it carries packed device/function
    /// bits (0..=255), while downstream of a PCIe port the device number is
    /// always zero so all 8 bits represent functions within a single
    /// endpoint.
    ///
    /// A device can distinguish Type 0 (local) from Type 1 (forwarded)
    /// configuration cycles by comparing `target_bus` and `secondary_bus`:
    /// when they are equal the access targets this device directly (Type 0),
    /// otherwise it should be routed downstream (Type 1). An SR-IOV
    /// capable device can use `secondary_bus` together with `target_bus` and
    /// `function` to compute the VF number.
    ///
    /// The default implementation dispatches function 0 to
    /// [`pci_cfg_write`](Self::pci_cfg_write) and silently drops writes to
    /// other functions. Routing components (switches, bridges) and
    /// multi-function devices should override this method.
    ///
    /// # Parameters
    /// - `secondary_bus`: The secondary bus number of the downstream port
    ///   that forwarded this access
    /// - `target_bus`: The bus number targeted by the configuration access
    /// - `function`: Device/function identifier — packed device/function on
    ///   a legacy bus, or flat function number on PCIe
    /// - `offset`: Configuration space offset
    /// - `value`: Value to write
    fn pci_cfg_write_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) -> IoResult {
        if secondary_bus == target_bus && function == 0 {
            self.pci_cfg_write(offset, value)
        } else {
            IoResult::Ok
        }
    }

    /// Check if the device has a suggested (bus, device, function) it expects
    /// to be located at.
    ///
    /// The term "suggested" is important here, as it's important to note that
    /// one of the major selling points of PCI was that PCI devices _shouldn't_
    /// need to care about about what PCI address they are initialized at. i.e:
    /// on a physical machine, it shouldn't matter that your fancy GTX 4090 is
    /// plugged into the first vs. second PCI slot.
    ///
    /// ..that said, there are some instances where it makes sense for an
    /// emulated device to declare its suggested PCI address:
    ///
    /// 1. Devices that emulate bespoke PCI devices part of a particular
    ///    system's chipset.
    ///   - e.g: the PIIX4 chipset includes several bespoke PCI devices that are
    ///     required to have specific PCI addresses. While it _would_ be
    ///     possible to relocate them to a different address, it may break OSes
    ///     that assume they exist at those spec-declared addresses.
    /// 2. Multi-function PCI devices
    ///   - In an unfortunate case of inverted responsibilities, there is a
    ///     single bit in the PCI configuration space's `Header` register that
    ///     denotes if a particular PCI card includes multiple functions.
    ///   - Since multi-function devices are pretty rare, `ChipsetDevice` opted
    ///     to model each function as its own device, which in turn implies that
    ///     in order to correctly init a multi-function PCI card, the
    ///     `ChipsetDevice` with function 0 _must_ report if there are other
    ///     functions at the same bus and device.
    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_enable_from_offset_len_rejects_invalid_accesses() {
        assert_eq!(
            PciConfigByteEnable::from_offset_len(0, 1).unwrap().bits(),
            0b0001
        );
        assert_eq!(
            PciConfigByteEnable::from_offset_len(1, 1).unwrap().bits(),
            0b0010
        );
        assert_eq!(
            PciConfigByteEnable::from_offset_len(2, 1).unwrap().bits(),
            0b0100
        );
        assert_eq!(
            PciConfigByteEnable::from_offset_len(3, 1).unwrap().bits(),
            0b1000
        );
        assert_eq!(
            PciConfigByteEnable::from_offset_len(0, 2).unwrap().bits(),
            0b0011
        );
        assert_eq!(
            PciConfigByteEnable::from_offset_len(2, 2).unwrap().bits(),
            0b1100
        );
        assert_eq!(
            PciConfigByteEnable::from_offset_len(0, 4).unwrap().bits(),
            0b1111
        );

        assert!(matches!(
            PciConfigByteEnable::from_offset_len(1, 2),
            Err(IoError::UnalignedAccess)
        ));
        assert!(matches!(
            PciConfigByteEnable::from_offset_len(3, 2),
            Err(IoError::UnalignedAccess)
        ));
        assert!(matches!(
            PciConfigByteEnable::from_offset_len(1, 4),
            Err(IoError::UnalignedAccess)
        ));
        assert!(matches!(
            PciConfigByteEnable::from_offset_len(0, 3),
            Err(IoError::InvalidAccessSize)
        ));
    }

    #[test]
    fn byte_enable_masks_and_merges_lanes() {
        let byte_enable = PciConfigByteEnable::from_offset_len(1, 1).unwrap();
        assert_eq!(byte_enable.bits(), 0b0010);
        assert_eq!(byte_enable.mask(), 0x0000_ff00);
        assert_eq!(byte_enable.extract(0x1234_5678), 0x0000_5600);
        assert_eq!(byte_enable.merge(0xaaaa_aaaa, 0x1234_5678), 0xaaaa_56aa);

        let byte_enable = PciConfigByteEnable::from_offset_len(2, 2).unwrap();
        assert_eq!(byte_enable.bits(), 0b1100);
        assert_eq!(byte_enable.mask(), 0xffff_0000);
        assert_eq!(byte_enable.extract(0x1234_5678), 0x1234_0000);
        assert_eq!(byte_enable.merge(0xaaaa_aaaa, 0x1234_5678), 0x1234_aaaa);

        let byte_enable = PciConfigByteEnable::from_offset_len(0, 4).unwrap();
        assert_eq!(byte_enable.bits(), 0b1111);
        assert_eq!(byte_enable.mask(), 0xffff_ffff);
        assert_eq!(byte_enable.extract(0x1234_5678), 0x1234_5678);
        assert_eq!(byte_enable.merge(0xaaaa_aaaa, 0x1234_5678), 0x1234_5678);
    }

    #[test]
    fn byte_enabled_intercept_buffers_copy_selected_lanes() {
        let write = ByteEnabledDwordWrite::from_intercept_buffer(
            PciConfigByteEnable::HIGH_WORD,
            &[0x22, 0x11],
        );
        assert_eq!(write.extract(), 0x1122_0000);
        assert_eq!(write.merge(0xaabb_ccdd), 0x1122_ccdd);

        let mut value = 0x5566_7788;
        let read = ByteEnabledDwordRead::new(&mut value, PciConfigByteEnable::HIGH_WORD);
        let mut buffer = [0; 2];
        read.fill_intercept_buffer(&mut buffer);
        assert_eq!(buffer, [0x66, 0x55]);
    }

    #[test]
    fn config_request_decodes_bdf() {
        let address = PciConfigAddress::new(0x12, 0x1d, 0x40).unwrap();

        assert_eq!(address.bus, 0x12);
        assert_eq!(address.device_function, 0x1d);
        assert_eq!(address.device(), 3);
        assert_eq!(address.function(), 5);
        assert_eq!(address.byte_offset(), 0x100);
    }
}
