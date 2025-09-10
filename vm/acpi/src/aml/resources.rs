// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Utilities for encoding various resource types into ACPI Machine Language (AML).

use super::objects::*;

/// An AML resource.
pub trait ResourceObject {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>);

    fn to_bytes(&self) -> Vec<u8> {
        let mut byte_stream = Vec::new();
        self.append_to_vec(&mut byte_stream);
        byte_stream
    }
}

/// A 32-bit, fixed-address AML memory resource.
pub struct Memory32Fixed {
    is_writeable: bool,
    base_address: u32,
    length: u32,
}

impl Memory32Fixed {
    /// Construct a new [`Memory32Fixed`].
    pub fn new(base_address: u32, length: u32, is_writeable: bool) -> Self {
        Self {
            is_writeable,
            base_address,
            length,
        }
    }
}

impl ResourceObject for Memory32Fixed {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.extend_from_slice(&[0x86, 9, 0]);
        byte_stream.push(if self.is_writeable { 1 } else { 0 });
        byte_stream.extend_from_slice(&self.base_address.to_le_bytes());
        byte_stream.extend_from_slice(&self.length.to_le_bytes());
    }
}

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
/// Attributes for AML memory resources.
pub enum MemoryAttribute {
    Memory = 0,
    _Reserved = 8,
    _Acpi = 0x10,
    _Nvs = 0x18,
}

#[repr(u8)]
#[derive(Copy, Clone, Debug)]
/// Cache types for AML memory resources.
pub enum MemoryCacheType {
    _NonCacheable = 0,
    Cacheable = 2,
    _CacheableWriteCombining = 4,
    _CacheableAndPrefetchable = 6,
}

/// A 32-bit AML memory resource.
pub struct DwordMemory {
    pub length: u32,
    pub translation_offset: u32,
    pub address_max: u32,
    pub address_min: u32,
    pub granularity: u32,
    pub attributes: MemoryAttribute,
    pub cacheability: MemoryCacheType,
    pub is_writeable: bool,
    pub is_max_address_fixed: bool,
    pub is_min_address_fixed: bool,
    pub is_subtractive_decode: bool,
}

impl ResourceObject for DwordMemory {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.extend_from_slice(&[0x87, 0x17, 0, 0]);
        byte_stream.push(
            if self.is_subtractive_decode { 2 } else { 0 }
                | if self.is_min_address_fixed { 4 } else { 0 }
                | if self.is_max_address_fixed { 8 } else { 0 },
        );
        byte_stream.push(
            if self.is_writeable { 1 } else { 0 } | self.cacheability as u8 | self.attributes as u8,
        );
        byte_stream.extend_from_slice(&self.granularity.to_le_bytes());
        byte_stream.extend_from_slice(&self.address_min.to_le_bytes());
        byte_stream.extend_from_slice(&self.address_max.to_le_bytes());
        byte_stream.extend_from_slice(&self.translation_offset.to_le_bytes());
        byte_stream.extend_from_slice(&self.length.to_le_bytes());
    }
}

impl DwordMemory {
    /// Construct a new [`DwordMemory`].
    pub fn new(address: u32, length: u32) -> Self {
        assert!(address as u64 + length as u64 - 1 <= u32::MAX as u64);
        Self {
            length,
            translation_offset: 0,
            address_max: address + (length - 1),
            address_min: address,
            granularity: 0,
            attributes: MemoryAttribute::Memory,
            cacheability: MemoryCacheType::Cacheable,
            is_writeable: true,
            is_max_address_fixed: true,
            is_min_address_fixed: true,
            is_subtractive_decode: false,
        }
    }
}

/// A 64-bit AML memory resource.
pub struct QwordMemory {
    pub is_io_backed: bool,
    pub attributes: MemoryAttribute,
    pub cacheability: MemoryCacheType,
    pub is_writeable: bool,
    pub min_address: u64,
    pub max_address: u64,
    pub length: u64,
}

impl QwordMemory {
    /// Construct a new [`QwordMemory`].
    pub fn new(address: u64, length: u64) -> Self {
        assert!(address as u128 + length as u128 - 1 <= u64::MAX as u128);
        Self {
            is_io_backed: false,
            attributes: MemoryAttribute::Memory,
            cacheability: MemoryCacheType::Cacheable,
            is_writeable: true,
            min_address: address,
            max_address: address + (length - 1),
            length,
        }
    }
}

impl ResourceObject for QwordMemory {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.extend_from_slice(&[0x8a, 0x2b, 0, 0, 0xc]);
        byte_stream.push(
            if self.is_io_backed { 0x20 } else { 0 }
                | self.attributes as u8
                | self.cacheability as u8
                | if self.is_writeable { 1 } else { 0 },
        );
        byte_stream.extend_from_slice(&(0_u64).to_le_bytes()); // granularity
        byte_stream.extend_from_slice(&self.min_address.to_le_bytes());
        byte_stream.extend_from_slice(&self.max_address.to_le_bytes());
        byte_stream.extend_from_slice(&(0_u64).to_le_bytes()); // translation offset
        byte_stream.extend_from_slice(&self.length.to_le_bytes());
    }
}

/// An ACPI bus number.
pub struct BusNumber {
    pub attributes: MemoryAttribute,
    pub min_address: u16,
    pub max_address: u16,
    pub length: u16,
}

impl BusNumber {
    /// Constructs a new bus number.
    pub fn new(address: u16, length: u16) -> Self {
        Self {
            attributes: MemoryAttribute::Memory,
            min_address: address,
            max_address: address + (length - 1),
            length,
        }
    }
}

impl ResourceObject for BusNumber {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.extend_from_slice(&[0x88, 0x0d, 0, 2, 0x0c]);
        byte_stream.push(0);
        byte_stream.extend_from_slice(&(0u16).to_le_bytes()); // granularity
        byte_stream.extend_from_slice(&self.min_address.to_le_bytes());
        byte_stream.extend_from_slice(&self.max_address.to_le_bytes());
        byte_stream.extend_from_slice(&(0u16).to_le_bytes()); // translation offset
        byte_stream.extend_from_slice(&self.length.to_le_bytes());
    }
}

/// An ACPI interrupt.
pub struct Interrupt {
    pub is_wake_capable: bool,
    pub is_shared: bool,
    pub is_low_polarity: bool,
    pub is_edge_triggered: bool,
    pub is_consumer: bool,
    number: u32,
}

impl Interrupt {
    /// Construct a new [`Interrupt`].
    pub fn new(number: u32) -> Self {
        Self {
            is_wake_capable: false,
            is_shared: false,
            is_low_polarity: false,
            is_edge_triggered: false,
            is_consumer: true,
            number,
        }
    }
}

impl ResourceObject for Interrupt {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.extend_from_slice(&[0x89, 6, 0]);
        byte_stream.push(
            if self.is_wake_capable { 0x10 } else { 0 }
                | if self.is_shared { 8 } else { 0 }
                | if self.is_low_polarity { 4 } else { 0 }
                | if self.is_edge_triggered { 2 } else { 0 }
                | if self.is_consumer { 1 } else { 0 },
        );
        byte_stream.push(1);
        byte_stream.extend_from_slice(&self.number.to_le_bytes());
    }
}

/// An ACPI IO port.
pub struct IoPort {
    pub is_16bit_aware: bool,
    pub base_address: u16,
    pub end_address: u16,
    pub alignment: u8,
    pub length: u8,
}

impl IoPort {
    /// Construct a new [`IoPort`].
    pub fn new(start: u16, end: u16, length: u8) -> Self {
        Self {
            is_16bit_aware: true,
            base_address: start,
            end_address: end,
            alignment: 1,
            length,
        }
    }
}

impl ResourceObject for IoPort {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        byte_stream.push(0x47);
        byte_stream.push(if self.is_16bit_aware { 1 } else { 0 });
        byte_stream.extend_from_slice(&self.base_address.to_le_bytes());
        byte_stream.extend_from_slice(&self.end_address.to_le_bytes());
        byte_stream.push(self.alignment);
        byte_stream.push(self.length);
    }
}

/// A group of current ACPI resources (CRS)
pub struct CurrentResourceSettings {
    resources: Vec<u8>,
}

impl CurrentResourceSettings {
    /// Construct a new [`CurrentResourceSettings`].
    pub fn new() -> Self {
        Self { resources: vec![] }
    }

    /// Add a resource to the collection.
    pub fn add_resource(&mut self, resource: &impl ResourceObject) {
        resource.append_to_vec(&mut self.resources);
    }
}

impl AmlObject for CurrentResourceSettings {
    fn append_to_vec(&self, byte_stream: &mut Vec<u8>) {
        let mut resource_bytes = self.resources.clone();
        // Add end of resource marker
        resource_bytes.extend_from_slice(&[0x79, 0]);

        // Generate _CRS buffer
        let nobj = NamedObject::new(b"_CRS", &Buffer(resource_bytes));
        nobj.append_to_vec(byte_stream);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aml::test_helpers::verify_expected_bytes;

    #[test]
    fn verify_memory_resource_object() {
        let memory = Memory32Fixed::new(0xfee00000, 0x1000, true);
        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&memory);
        let bytes = crs.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x08, b'_', b'C', b'R', b'S', 0x11, 17, 0x0a, 14, 0x86, 0x09, 0, 1, 0, 0, 0xe0,
                0xfe, 0, 0x10, 0, 0, 0x79, 0,
            ],
        );
    }

    #[test]
    fn verify_dword_memory_resource_object() {
        let memory = DwordMemory::new(0x10000000, 0x10000000);
        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&memory);
        let bytes = crs.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x08, b'_', b'C', b'R', b'S', 0x11, 0x1f, 0x0a, 0x1c, 0x87, 0x17, 0x00, 0x00, 0x0C,
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0xFF, 0xFF, 0xFF, 0x1F, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x79, 0x00,
            ],
        );
    }

    #[test]
    fn verify_qword_memory_resource_object() {
        let memory = QwordMemory::new(0x100000000, 0x100000000);
        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&memory);
        let bytes = crs.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x08, b'_', b'C', b'R', b'S', 0x11, 51, 0x0a, 48, 0x8A, 0x2B, 0x00, 0x00, 0x0C,
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
                0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x79,
                0x00,
            ],
        );
    }

    #[test]
    fn verify_ioport_resource_object() {
        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&IoPort::new(0x3f8, 0x3f8, 8));
        let bytes = crs.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x08, b'_', b'C', b'R', b'S', 0x11, 13, 0x0a, 10, 0x47, 0x01, 0xF8, 0x03, 0xF8,
                0x03, 0x01, 0x08, 0x79, 0x00,
            ],
        );
    }

    #[test]
    fn verify_interrupt_resource_object() {
        let mut crs = CurrentResourceSettings::new();
        let mut interrupt = Interrupt::new(4);
        interrupt.is_edge_triggered = true;
        crs.add_resource(&interrupt);
        let bytes = crs.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x08, b'_', b'C', b'R', b'S', 0x11, 14, 0x0a, 11, 0x89, 0x06, 0x00, 0x03, 0x01,
                0x04, 0x00, 0x00, 0x00, 0x79, 0x00,
            ],
        );
    }

    #[test]
    fn verify_resource_object_multi() {
        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&Memory32Fixed::new(0xfee00000, 0x1000, true));
        crs.add_resource(&Memory32Fixed::new(0xfec00000, 0x1000, true));
        let bytes = crs.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x08, b'_', b'C', b'R', b'S', 0x11, 0x1d, 0x0a, 0x1a, 0x86, 0x09, 0x00, 0x01, 0x00,
                0x00, 0xe0, 0xFE, 0x00, 0x10, 0x00, 0x00, 0x86, 0x09, 0x00, 0x01, 0x00, 0x00, 0xC0,
                0xFE, 0x00, 0x10, 0x00, 0x00, 0x79, 0x00,
            ],
        );
    }

    #[test]
    fn verify_resource_object_multi2() {
        let mut crs = CurrentResourceSettings::new();
        crs.add_resource(&IoPort::new(0x3f8, 0x3f8, 8));
        let mut interrupt = Interrupt::new(4);
        interrupt.is_edge_triggered = true;
        crs.add_resource(&interrupt);
        let bytes = crs.to_bytes();
        verify_expected_bytes(
            &bytes,
            &[
                0x08, 0x5F, 0x43, 0x52, 0x53, 0x11, 0x16, 0x0A, 0x13, 0x47, 0x01, 0xF8, 0x03, 0xF8,
                0x03, 0x01, 0x08, 0x89, 0x06, 0x00, 0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x79, 0x00,
            ],
        );
    }
}
