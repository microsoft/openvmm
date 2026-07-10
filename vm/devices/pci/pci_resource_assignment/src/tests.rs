// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(test)]

use crate::AssignmentParams;
use crate::PciConfigAccess;
use crate::assign_pci_resources;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Mock PCI config space: a flat map from (bus, device, function, offset) → u32.
///
/// Supports BAR probing: when a BAR register is written with 0xFFFFFFFF, the
/// subsequent read returns the size mask. The mock handles this by tracking
/// a separate "bar_masks" map.
#[derive(Clone)]
struct MockConfigSpace {
    inner: Arc<Mutex<MockInner>>,
}

struct MockInner {
    /// Config space registers: (bus, dev, func, offset) → value.
    regs: BTreeMap<(u8, u8, u8, u16), u32>,
    /// BAR size masks: (bus, dev, func, bar_offset) → mask.
    /// When a BAR is written with 0xFFFFFFFF and MMIO is disabled,
    /// the next read returns (0xFFFFFFFF & mask) | encoding_bits.
    bar_masks: BTreeMap<(u8, u8, u8, u16), u32>,
    /// Track whether a BAR is in "probing" state (written with 0xFFFFFFFF).
    bar_probing: BTreeMap<(u8, u8, u8, u16), bool>,
}

impl MockConfigSpace {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MockInner {
                regs: BTreeMap::new(),
                bar_masks: BTreeMap::new(),
                bar_probing: BTreeMap::new(),
            })),
        }
    }

    /// Add a Type 0 endpoint at (bus, device, function) with the given BARs.
    fn add_endpoint(&self, bus: u8, device: u8, function: u8, bars: &[(u8, u64, bool, bool)]) {
        let mut inner = self.inner.lock();
        let key = |off: u16| (bus, device, function, off);

        // Vendor/Device ID (non-0xFFFF).
        inner.regs.insert(key(0x00), 0x1234_5678);
        // Class/Revision.
        inner.regs.insert(key(0x08), 0x0200_0000);
        // BIST/Header: Type 0, single function.
        inner.regs.insert(key(0x0C), 0x0000_0000);
        // Command: MMIO disabled initially.
        inner.regs.insert(key(0x04), 0x0000_0000);

        for &(bar_idx, size, is_64bit, prefetchable) in bars {
            let offset = 0x10 + (bar_idx as u16) * 4;
            // Initial BAR value: encoding bits only.
            let mut encoding: u32 = 0;
            if is_64bit {
                encoding |= 0x04; // 64-bit
            }
            if prefetchable {
                encoding |= 0x08;
            }
            inner.regs.insert(key(offset), encoding);

            // Compute mask: size must be power of 2, mask = !(size - 1).
            let mask = (!(size - 1)) as u32;
            inner.bar_masks.insert(key(offset), mask | encoding);

            if is_64bit {
                let upper_offset = offset + 4;
                let upper_mask = ((!(size - 1)) >> 32) as u32;
                inner.regs.insert(key(upper_offset), 0);
                inner.bar_masks.insert(key(upper_offset), upper_mask);
            }
        }
    }

    /// Add a Type 0 multi-function device (mark function 0 as multi-function).
    fn set_multi_function(&self, bus: u8, device: u8) {
        let mut inner = self.inner.lock();
        let key = (bus, device, 0, 0x0Cu16);
        let val = inner.regs.get(&key).copied().unwrap_or(0);
        inner.regs.insert(key, val | 0x0080_0000); // multi-function bit
    }

    /// Pre-program a BAR with an address (for testing preserve_bars).
    /// The address is written into the BAR register along with the existing
    /// encoding bits.
    fn set_bar_address(&self, bus: u8, device: u8, function: u8, bar_idx: u8, address: u64) {
        let mut inner = self.inner.lock();
        let offset = 0x10 + (bar_idx as u16) * 4;
        let key = (bus, device, function, offset);
        let encoding = inner.regs.get(&key).copied().unwrap_or(0) & 0xf;
        inner.regs.insert(key, (address as u32 & !0xf) | encoding);
        // If 64-bit, also set the upper DWORD.
        if encoding & 0x04 != 0 {
            let upper_key = (bus, device, function, offset + 4);
            inner.regs.insert(upper_key, (address >> 32) as u32);
        }
    }

    /// Add a Type 1 bridge at (bus, device, function).
    fn add_bridge(&self, bus: u8, device: u8, function: u8) {
        let mut inner = self.inner.lock();
        let key = |off: u16| (bus, device, function, off);

        // Vendor/Device ID.
        inner.regs.insert(key(0x00), 0xABCD_EF01);
        // Class/Revision: Bridge class (0x06), PCI-to-PCI subclass (0x04).
        inner.regs.insert(key(0x08), 0x0604_0000);
        // BIST/Header: Type 1.
        inner.regs.insert(key(0x0C), 0x0001_0000);
        // Command.
        inner.regs.insert(key(0x04), 0x0000_0000);
        // Bus numbers (will be programmed by enumeration).
        inner.regs.insert(key(0x18), 0x0000_0000);
        // Memory range (will be programmed).
        inner.regs.insert(key(0x20), 0x0000_0000);
        // Prefetchable range.
        inner.regs.insert(key(0x24), 0x0000_0000);
        inner.regs.insert(key(0x28), 0x0000_0000);
        inner.regs.insert(key(0x2C), 0x0000_0000);
    }

    /// Add an SR-IOV extended capability to a device.
    /// `total_vfs`: max VFs supported, `vf_offset`: routing ID offset to first VF,
    /// `vf_stride`: routing ID increment per VF.
    /// `vf_bars`: VF BAR definitions as (bar_index, size, is_64bit, prefetchable).
    fn add_sriov(
        &self,
        bus: u8,
        device: u8,
        function: u8,
        total_vfs: u16,
        vf_offset: u16,
        vf_stride: u16,
    ) {
        self.add_sriov_with_bars(bus, device, function, total_vfs, vf_offset, vf_stride, &[]);
    }

    fn add_sriov_with_bars(
        &self,
        bus: u8,
        device: u8,
        function: u8,
        total_vfs: u16,
        vf_offset: u16,
        vf_stride: u16,
        vf_bars: &[(u8, u64, bool, bool)],
    ) {
        let mut inner = self.inner.lock();
        let key = |off: u16| (bus, device, function, off);

        // Extended capability header at 0x100: SR-IOV cap ID (0x10), next = 0.
        let sriov_id: u16 = 0x10;
        inner.regs.insert(key(0x100), sriov_id as u32); // cap_id=0x10, version=0, next=0

        // InitialVFs (low u16) | TotalVFs (high u16) at cap + 0x0C.
        inner.regs.insert(
            key(0x100 + 0x0C),
            (total_vfs as u32) << 16 | total_vfs as u32,
        );

        // VF Offset (low u16) | VF Stride (high u16) at cap + 0x14.
        inner.regs.insert(
            key(0x100 + 0x14),
            (vf_stride as u32) << 16 | vf_offset as u32,
        );

        // VF BARs at cap + 0x24..0x38.
        for &(bar_idx, size, is_64bit, prefetchable) in vf_bars {
            let offset = 0x100 + 0x24 + (bar_idx as u16) * 4;
            let mut encoding: u32 = 0;
            if is_64bit {
                encoding |= 0x04;
            }
            if prefetchable {
                encoding |= 0x08;
            }
            inner.regs.insert(key(offset), encoding);
            let mask = (!(size - 1)) as u32;
            inner.bar_masks.insert(key(offset), mask | encoding);

            if is_64bit {
                let upper_offset = offset + 4;
                let upper_mask = ((!(size - 1)) >> 32) as u32;
                inner.regs.insert(key(upper_offset), 0);
                inner.bar_masks.insert(key(upper_offset), upper_mask);
            }
        }
    }

    fn read_reg(&self, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        let inner = self.inner.lock();
        let key = (bus, device, function, offset);

        // Check if this is a BAR in probing state.
        if inner.bar_probing.get(&key).copied().unwrap_or(false) {
            return inner.bar_masks.get(&key).copied().unwrap_or(0);
        }

        inner.regs.get(&key).copied().unwrap_or(!0u32)
    }

    fn write_reg(&self, bus: u8, device: u8, function: u8, offset: u16, value: u32) {
        let mut inner = self.inner.lock();
        let key = (bus, device, function, offset);

        // Check if this offset has a BAR mask (device BAR or VF BAR).
        if inner.bar_masks.contains_key(&key) {
            if value == !0u32 {
                inner.bar_probing.insert(key, true);
                return;
            } else {
                inner.bar_probing.insert(key, false);
            }
        }

        inner.regs.insert(key, value);
    }
}

impl PciConfigAccess for MockConfigSpace {
    async fn read_u32(&mut self, bus: u8, devfn: u8, offset: u16) -> u32 {
        self.read_reg(bus, devfn >> 3, devfn & 0x7, offset)
    }

    async fn write_u32(&mut self, bus: u8, devfn: u8, offset: u16, value: u32) {
        self.write_reg(bus, devfn >> 3, devfn & 0x7, offset, value);
    }
}

use pal_async::async_test;

// ---- Config space reading helpers ----

/// Read a 32-bit BAR address from mock config space.
fn read_bar32(mock: &MockConfigSpace, bus: u8, dev: u8, func: u8, bar_idx: u8) -> u32 {
    mock.read_reg(bus, dev, func, 0x10 + bar_idx as u16 * 4)
}

/// Read a 64-bit BAR address from mock config space (two consecutive DWORDs).
fn read_bar64(mock: &MockConfigSpace, bus: u8, dev: u8, func: u8, bar_idx: u8) -> u64 {
    let lo = mock.read_reg(bus, dev, func, 0x10 + bar_idx as u16 * 4) as u64;
    let hi = mock.read_reg(bus, dev, func, 0x10 + (bar_idx + 1) as u16 * 4) as u64;
    lo | (hi << 32)
}

/// Read bus numbers (primary, secondary, subordinate) from a bridge.
fn read_bus_numbers(mock: &MockConfigSpace, bus: u8, dev: u8, func: u8) -> (u8, u8, u8) {
    let reg = mock.read_reg(bus, dev, func, 0x18);
    (reg as u8, (reg >> 8) as u8, (reg >> 16) as u8)
}

/// Read the non-prefetchable memory window from a bridge.
/// Returns None if disabled (base > limit).
fn read_memory_window(mock: &MockConfigSpace, bus: u8, dev: u8, func: u8) -> Option<(u64, u64)> {
    let reg = mock.read_reg(bus, dev, func, 0x20);
    let base = ((reg as u64) & 0xFFF0) << 16;
    let limit = (((reg >> 16) as u64) & 0xFFF0) << 16 | 0xF_FFFF;
    if base <= limit {
        Some((base, limit))
    } else {
        None
    }
}

/// Read the prefetchable memory window from a bridge.
/// Returns None if disabled (base > limit).
fn read_prefetchable_window(
    mock: &MockConfigSpace,
    bus: u8,
    dev: u8,
    func: u8,
) -> Option<(u64, u64)> {
    let reg = mock.read_reg(bus, dev, func, 0x24);
    let base_lo = ((reg as u64) & 0xFFF0) << 16;
    let limit_lo = (((reg >> 16) as u64) & 0xFFF0) << 16 | 0xF_FFFF;
    let base_hi = mock.read_reg(bus, dev, func, 0x28) as u64;
    let limit_hi = mock.read_reg(bus, dev, func, 0x2C) as u64;
    let base = base_lo | (base_hi << 32);
    let limit = limit_lo | (limit_hi << 32);
    if base <= limit {
        Some((base, limit))
    } else {
        None
    }
}

/// Read a 32-bit VF BAR address from the SR-IOV capability in mock config space.
/// SR-IOV cap is at 0x100, VF BAR0 starts at cap + 0x24.
/// Masks off the low 4 encoding bits.
fn read_vf_bar32(mock: &MockConfigSpace, bus: u8, dev: u8, func: u8, bar_idx: u8) -> u32 {
    mock.read_reg(bus, dev, func, 0x100 + 0x24 + bar_idx as u16 * 4) & !0xF
}

/// Read a 64-bit VF BAR address from the SR-IOV capability in mock config space.
/// Masks off the low 4 encoding bits from the lower DWORD.
fn read_vf_bar64(mock: &MockConfigSpace, bus: u8, dev: u8, func: u8, bar_idx: u8) -> u64 {
    let lo = (mock.read_reg(bus, dev, func, 0x100 + 0x24 + bar_idx as u16 * 4) & !0xF) as u64;
    let hi = mock.read_reg(bus, dev, func, 0x100 + 0x24 + (bar_idx + 1) as u16 * 4) as u64;
    lo | (hi << 32)
}

// ---- Tests ----

#[async_test]
async fn single_endpoint_32bit_bar() {
    let mock = MockConfigSpace::new();
    // Device on bus 0, device 0, function 0 with a 64KB 32-bit non-pref BAR at index 0.
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // BAR0 should be assigned at the aperture base.
    assert_eq!(read_bar32(&mock, 0, 0, 0, 0), 0x1000_0000);
}

#[async_test]
async fn single_endpoint_64bit_bar() {
    let mock = MockConfigSpace::new();
    // 1 MB 64-bit prefetchable BAR at index 0.
    mock.add_endpoint(0, 1, 0, &[(0, 0x100000, true, true)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::EMPTY,
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000),
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Verify both BAR registers were programmed.
    assert_eq!(read_bar64(&mock, 0, 1, 0, 0), 0x1_0000_0000);
}

#[async_test]
async fn bridge_with_endpoint() {
    let mock = MockConfigSpace::new();

    // Bridge at bus 0, device 0, function 0.
    mock.add_bridge(0, 0, 0);
    // Endpoint behind bridge at bus 1, device 0, function 0.
    // 64KB non-prefetchable BAR.
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Bridge bus numbers.
    let (_, secondary, subordinate) = read_bus_numbers(&mock, 0, 0, 0);
    assert_eq!(secondary, 1);
    assert_eq!(subordinate, 1);

    // Bridge should have a non-prefetchable memory window.
    assert!(read_memory_window(&mock, 0, 0, 0).is_some());

    // Endpoint BAR should be programmed.
    let bar = read_bar32(&mock, 1, 0, 0, 0);
    assert!(bar >= 0x1000_0000);
}

#[async_test]
async fn multiple_endpoints_sorted_by_size() {
    let mock = MockConfigSpace::new();

    // Two devices with different BAR sizes.
    // Device 0: 4KB BAR.
    mock.add_endpoint(0, 0, 0, &[(0, 0x1000, false, false)]);
    // Device 1: 1MB BAR.
    mock.add_endpoint(0, 1, 0, &[(0, 0x100000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // The 1MB BAR should be allocated first (sorted by size desc) and
    // aligned to 1MB.
    assert_eq!(read_bar32(&mock, 0, 1, 0, 0), 0x1000_0000);
    // The 4KB BAR should follow.
    assert_eq!(read_bar32(&mock, 0, 0, 0, 0), 0x1010_0000);
}

#[async_test]
async fn multi_function_device() {
    let mock = MockConfigSpace::new();

    // Function 0: 4KB BAR.
    mock.add_endpoint(0, 0, 0, &[(0, 0x1000, false, false)]);
    // Function 1: 4KB BAR.
    mock.add_endpoint(0, 0, 1, &[(0, 0x1000, false, false)]);
    // Mark as multi-function.
    mock.set_multi_function(0, 0);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Should find both functions.
    let f0_bar = read_bar32(&mock, 0, 0, 0, 0);
    let f1_bar = read_bar32(&mock, 0, 0, 1, 0);
    assert_ne!(f0_bar, f1_bar);
}

#[async_test]
async fn switch_with_multiple_endpoints() {
    let mock = MockConfigSpace::new();

    // Upstream bridge on bus 0.
    mock.add_bridge(0, 0, 0);

    // Two downstream bridges on bus 1.
    mock.add_bridge(1, 0, 0);
    mock.add_bridge(1, 1, 0);

    // Endpoint behind first downstream bridge (bus 2).
    mock.add_endpoint(2, 0, 0, &[(0, 0x10000, false, false)]);

    // Endpoint behind second downstream bridge (bus 3).
    mock.add_endpoint(3, 0, 0, &[(0, 0x10000, true, true)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000),
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Verify bus numbers were assigned correctly.
    let (_, secondary, _) = read_bus_numbers(&mock, 0, 0, 0);
    assert_eq!(secondary, 1);

    // Both endpoints should have BAR addresses assigned.
    // 32-bit BAR on bus 2 should be in low MMIO.
    let ep1_bar = read_bar32(&mock, 2, 0, 0, 0) as u64;
    assert!(ep1_bar >= 0x1000_0000);
    assert!(ep1_bar < 0x2000_0000);

    // 64-bit BAR on bus 3 should be in high MMIO.
    let ep2_bar = read_bar64(&mock, 3, 0, 0, 0);
    assert!(ep2_bar >= 0x1_0000_0000);

    // The downstream bridge for the 64-bit endpoint should have a
    // prefetchable window covering high MMIO.
    let pref = read_prefetchable_window(&mock, 1, 1, 0);
    assert!(
        pref.is_some(),
        "bridge behind 64-bit endpoint should have prefetchable window"
    );
    assert!(pref.unwrap().0 >= 0x1_0000_0000);

    // The downstream bridge for the 32-bit endpoint should have a
    // non-prefetchable window in low MMIO, and no prefetchable window.
    assert!(read_memory_window(&mock, 1, 0, 0).is_some());
    assert!(read_prefetchable_window(&mock, 1, 0, 0).is_none());
}

#[async_test]
async fn bus_exhaustion_error() {
    let mock = MockConfigSpace::new();
    mock.add_bridge(0, 0, 0);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 0, // No room for secondary bus.
        low_mmio: MemoryRange::EMPTY,
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(result.is_err());
    assert!(
        matches!(
            result.unwrap_err(),
            crate::AssignmentError::BusExhaustion { .. }
        ),
        "expected BusExhaustion"
    );
}

#[async_test]
async fn no_devices_is_ok() {
    let mock = MockConfigSpace::new();

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock;
    assign_pci_resources(&mut cfg, &params).await.unwrap();
    // Success is the assertion — no devices means nothing to program.
}

#[async_test]
async fn mmio_exhaustion_error() {
    let mock = MockConfigSpace::new();

    // Two devices totaling 192KB — exceeds the 128KB aperture.
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]); // 64KB
    mock.add_endpoint(0, 1, 0, &[(0, 0x20000, false, false)]); // 128KB

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x20000), // 128KB — not enough for both
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(result.is_err());
    assert!(
        matches!(
            result.unwrap_err(),
            crate::AssignmentError::MmioExhaustion { .. }
        ),
        "expected MmioExhaustion"
    );
}

#[async_test]
async fn no_aperture_error() {
    let mock = MockConfigSpace::new();

    mock.add_endpoint(0, 0, 0, &[(0, 0x1000, false, false)]);

    // No MMIO apertures at all.
    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::EMPTY,
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(result.is_err());
    assert!(
        matches!(
            result.unwrap_err(),
            crate::AssignmentError::MmioExhaustion { .. }
        ),
        "expected MmioExhaustion"
    );
}

#[async_test]
async fn sriov_reserves_bus_numbers() {
    let mock = MockConfigSpace::new();

    // Bridge on bus 0.
    mock.add_bridge(0, 0, 0);

    // Endpoint on bus 1 with SR-IOV: 256 VFs, offset=1, stride=1.
    // PF routing ID = (1 << 8) | (0 << 3) | 0 = 0x100
    // Last VF routing ID = 0x100 + 1 + 255*1 = 0x200 → bus 2
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 256, 1, 1);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Bridge should have subordinate >= 2 to cover VF buses.
    let (_, secondary, subordinate) = read_bus_numbers(&mock, 0, 0, 0);
    assert_eq!(secondary, 1);
    assert!(
        subordinate >= 2,
        "subordinate bus {subordinate} should be >= 2 to cover SR-IOV VFs"
    );

    // Also verify from config space.
    let bus_reg = mock.read_reg(0, 0, 0, 0x18);
    let sub_from_reg = (bus_reg >> 16) & 0xFF;
    assert!(
        sub_from_reg >= 2,
        "subordinate {sub_from_reg} should be >= 2"
    );
}

#[async_test]
async fn sriov_no_vfs_no_reservation() {
    let mock = MockConfigSpace::new();

    // Bridge on bus 0.
    mock.add_bridge(0, 0, 0);

    // Endpoint on bus 1 with SR-IOV but 0 VFs.
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 0, 1, 1);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // No extra buses reserved — subordinate should be 1.
    let (_, _, subordinate) = read_bus_numbers(&mock, 0, 0, 0);
    assert_eq!(subordinate, 1);
}

#[async_test]
async fn bridge_prefetchable_window_programmed() {
    let mock = MockConfigSpace::new();

    // Bridge on bus 0 with a 64-bit endpoint behind it.
    mock.add_bridge(0, 0, 0);
    mock.add_endpoint(1, 0, 0, &[(0, 0x100000, true, true)]); // 1 MB 64-bit pref

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::EMPTY,
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000),
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // The bridge should have a prefetchable window, not a non-prefetchable one.
    assert!(
        read_memory_window(&mock, 0, 0, 0).is_none(),
        "no non-prefetchable window expected"
    );

    // Prefetchable window should exist.
    let pref = read_prefetchable_window(&mock, 0, 0, 0);
    assert!(pref.is_some(), "prefetchable window expected");
    assert!(pref.unwrap().0 >= 0x1_0000_0000);

    // Verify the prefetchable window registers were programmed.
    // Offset 0x24: prefetchable base/limit (lower 16 bits each, with 64-bit flag).
    let pf_range = mock.read_reg(0, 0, 0, 0x24);
    assert_ne!(
        pf_range, 0,
        "prefetchable range register should be programmed"
    );
    // Bit 0 of base should be 1 (64-bit indicator).
    assert_eq!(pf_range & 0x1, 0x1, "64-bit indicator should be set");

    // Offset 0x28: prefetchable base upper 32 bits.
    let pf_base_upper = mock.read_reg(0, 0, 0, 0x28);
    assert_eq!(
        pf_base_upper, 0x1,
        "upper base should be 0x1 for address >= 0x1_0000_0000"
    );

    // Offset 0x2C: prefetchable limit upper 32 bits.
    let pf_limit_upper = mock.read_reg(0, 0, 0, 0x2C);
    assert_eq!(pf_limit_upper, 0x1, "upper limit should be 0x1");
}

#[async_test]
async fn sibling_bridge_windows_must_not_overlap() {
    let mock = MockConfigSpace::new();

    // Upstream bridge on bus 0.
    mock.add_bridge(0, 0, 0);

    // Two downstream bridges on bus 1.
    mock.add_bridge(1, 0, 0); // Bridge A → bus 2
    mock.add_bridge(1, 1, 0); // Bridge B → bus 3

    // Bridge A has two BARs behind it: 1 MB + 4 KB.
    mock.add_endpoint(
        2,
        0,
        0,
        &[(0, 0x100000, false, false), (2, 0x1000, false, false)],
    );

    // Bridge B has one BAR behind it: 512 KB.
    mock.add_endpoint(3, 0, 0, &[(0, 0x80000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Find bridge windows for the two downstream bridges.
    let a_window = read_memory_window(&mock, 1, 0, 0).expect("bridge A should have memory window");
    let b_window = read_memory_window(&mock, 1, 1, 0).expect("bridge B should have memory window");

    // Sibling bridge windows must not overlap — if they do, both bridges
    // will claim the same addresses on the upstream bus.
    assert!(
        a_window.1 < b_window.0 || b_window.1 < a_window.0,
        "bridge windows overlap: A=[{:#x}..={:#x}], B=[{:#x}..={:#x}]",
        a_window.0,
        a_window.1,
        b_window.0,
        b_window.1
    );
}

#[async_test]
async fn large_bar_alignment_fits_in_bridge_window() {
    let mock = MockConfigSpace::new();

    // Bridge on bus 0.
    mock.add_bridge(0, 0, 0);

    // Endpoint behind bridge with a 1 GB BAR (needs 1 GB natural alignment).
    mock.add_endpoint(1, 0, 0, &[(0, 0x4000_0000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x2_0000_0000), // 8 GB (256 MB — not 1 GB aligned)
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let window = read_memory_window(&mock, 0, 0, 0).expect("bridge should have memory window");
    let bar_addr = read_bar32(&mock, 1, 0, 0, 0) as u64;

    // The BAR must be naturally aligned.
    assert_eq!(
        bar_addr % 0x4000_0000,
        0,
        "1 GB BAR at {bar_addr:#x} is not 1 GB aligned"
    );

    // The BAR must fit within the bridge window.
    let bar_end = bar_addr + 0x4000_0000 - 1;
    assert!(
        bar_addr >= window.0 && bar_end <= window.1,
        "BAR [{bar_addr:#x}..={bar_end:#x}] overflows bridge window [{:#x}..={:#x}]",
        window.0,
        window.1
    );
}

#[async_test]
async fn alignment_first_sort_avoids_wasted_padding() {
    let mock = MockConfigSpace::new();

    // Bridge A: three 1 MB BARs → subtree size = 3 MB, alignment = 1 MB.
    mock.add_bridge(0, 0, 0);
    mock.add_endpoint(1, 0, 0, &[(0, 0x100000, false, false)]);
    mock.add_endpoint(1, 1, 0, &[(0, 0x100000, false, false)]);
    mock.add_endpoint(1, 2, 0, &[(0, 0x100000, false, false)]);

    // Bridge B: one 2 MB BAR → subtree size = 2 MB, alignment = 2 MB.
    mock.add_bridge(0, 1, 0);
    mock.add_endpoint(2, 0, 0, &[(0, 0x200000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x500000), // 5 MB
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    // 5 MB aperture. With alignment-first ordering (B then A), total is
    // exactly 5 MB: B at 0 (2 MB aligned, uses 2 MB), A at 2 MB (1 MB
    // aligned, uses 3 MB). With size-first ordering (A then B), A uses
    // 3 MB, then B needs 2 MB alignment → next 2 MB boundary is 4 MB,
    // total = 6 MB, which overflows.
    assert!(
        result.is_ok(),
        "5 MB aperture should be sufficient: {}",
        result.unwrap_err()
    );
}

#[async_test]
async fn misaligned_aperture_does_not_overflow() {
    let mock = MockConfigSpace::new();

    // Single endpoint with a 4 MB BAR (needs 4 MB natural alignment).
    mock.add_endpoint(0, 0, 0, &[(0, 0x400000, false, false)]);

    // Aperture base is only 2 MB aligned, not 4 MB aligned.
    // The allocator must align up to the next 4 MB boundary, losing 2 MB,
    // so it needs at least 6 MB of aperture. Give it exactly 4 MB — this
    // should fail because the BAR won't fit after alignment, but must not
    // silently place the BAR past the end of the aperture.
    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1020_0000..0x1020_0000 + 0x400000), // 4 MB (2 MB aligned, NOT 4 MB aligned)
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    // The aperture is too small after alignment padding — this must fail.
    assert!(
        matches!(result, Err(crate::AssignmentError::MmioExhaustion { .. })),
        "expected MmioExhaustion for misaligned aperture, got {result:?}"
    );
}

#[async_test]
async fn alignment_exceeds_aperture_returns_error() {
    let mock = MockConfigSpace::new();

    // Single endpoint with a 16 MB BAR (needs 16 MB alignment).
    mock.add_endpoint(0, 0, 0, &[(0, 0x1000000, false, false)]);

    // Aperture is only 2 MB total. Alignment pushes base past the end,
    // which must produce MmioExhaustion rather than wrapping the
    // available-space calculation.
    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1020_0000..0x1020_0000 + 0x200000), // 2 MB total (2 MB past a 16 MB boundary)
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    assert!(
        matches!(result, Err(crate::AssignmentError::MmioExhaustion { .. })),
        "expected MmioExhaustion when alignment exceeds aperture, got {result:?}"
    );
}

#[async_test]
async fn sriov_bus_reservation_exceeding_end_bus_returns_error() {
    let mock = MockConfigSpace::new();

    // Bridge on bus 0.
    mock.add_bridge(0, 0, 0);

    // Endpoint on bus 1 with SR-IOV: 256 VFs, offset=1, stride=1.
    // PF routing ID = (1 << 8) | (0 << 3) | 0 = 0x100
    // Last VF routing ID = 0x100 + 1 + 255*1 = 0x200 → bus 2
    // But end_bus is 1, so VF bus 2 is out of range.
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 256, 1, 1);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 1, // Only buses 0 and 1 allowed.
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;

    // SR-IOV VFs need bus 2, but end_bus is 1. The code returns
    // BusExhaustion because the VF bus range exceeds the allowed range.
    assert!(
        matches!(result, Err(crate::AssignmentError::BusExhaustion { .. })),
        "expected BusExhaustion when SR-IOV reservation exceeds end_bus, got {result:?}"
    );
}

/// When low_mmio is None, 32-bit (non-prefetchable) BARs fall back
/// to high_mmio via `.or(params.high_mmio)`. If high_mmio is above 4 GB,
/// the BAR address is truncated by `bar.address as u32` and bridge memory
/// base/limit registers (inherently 32-bit) silently lose upper bits.
/// 32-bit BARs must never be placed above 4 GB.
#[async_test]
async fn mem32_bar_must_not_be_placed_above_4gb() {
    let mock = MockConfigSpace::new();

    // 32-bit non-prefetchable BAR.
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::EMPTY,
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000), // Above 4 GB
        preserve_bars: false,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;

    // No sub-4GB aperture exists, so 32-bit BARs cannot be placed.
    // Must fail rather than silently placing them above 4 GB.
    assert!(
        result.is_err(),
        "expected error when only aperture is above 4 GB"
    );
}

/// When both mem32 and mem64 pools share the same aperture (only
/// one of low_mmio/high_mmio is provided), the mem64 base is computed
/// from `aperture.base + root_req.mem32` instead of from the actual
/// aligned mem32 end address. If aperture.base needs alignment for mem32,
/// mem64 can start inside the mem32 region, causing overlapping
/// assignments. BAR regions must not overlap.
#[async_test]
async fn shared_aperture_mem32_mem64_must_not_overlap() {
    let mock = MockConfigSpace::new();

    // Device 0: 4 MB 32-bit non-prefetchable BAR (needs 4 MB alignment).
    mock.add_endpoint(0, 0, 0, &[(0, 0x40_0000, false, false)]);
    // Device 1: 1 MB 64-bit prefetchable BAR.
    mock.add_endpoint(0, 1, 0, &[(0, 0x10_0000, true, true)]);

    // Single aperture, misaligned so that mem32 alignment pushes its base
    // forward, creating a gap between aperture.base and mem32 start.
    // aperture.base = 0x1020_0000 (2 MB past a 4 MB boundary)
    // mem32 aligns up to 0x1040_0000, occupies [0x1040_0000, 0x1080_0000)
    // Bug: mem64 base = align_up(0x1020_0000 + 0x40_0000, 1MB)
    //                  = 0x1060_0000, which is INSIDE the mem32 region.
    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1020_0000..0x1020_0000 + 0x0100_0000), // 16 MB — plenty of room
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Collect all BAR regions and verify no overlaps.
    // Device 0: 4 MB 32-bit BAR.
    let bar0_addr = read_bar32(&mock, 0, 0, 0, 0) as u64;
    let bar0_end = bar0_addr + 0x40_0000;

    // Device 1: 1 MB 64-bit BAR (in low MMIO, so hi is 0).
    let bar1_addr = read_bar64(&mock, 0, 1, 0, 0);
    let bar1_end = bar1_addr + 0x10_0000;

    assert!(
        bar0_end <= bar1_addr || bar1_end <= bar0_addr,
        "BAR regions overlap: [{bar0_addr:#x}..{bar0_end:#x}) vs [{bar1_addr:#x}..{bar1_end:#x})"
    );
}

/// When bus 255 is consumed (by a bridge secondary or SR-IOV VF
/// range), `wrapping_add(1)` wraps `*next_bus` to 0. The subsequent
/// `*next_bus > end_bus` guard (0 > 255) is false, so the next bridge
/// gets secondary bus 0, colliding with the host bridge. Should return
/// BusExhaustion instead.
#[async_test]
async fn bus_wrap_to_zero_must_return_exhaustion() {
    let mock = MockConfigSpace::new();

    // Bridge 0 on bus 0 → secondary = 1.
    mock.add_bridge(0, 0, 0);

    // Endpoint on bus 1 with SR-IOV: 256 VFs, offset = 0x100, stride = 1.
    // PF routing ID = (1 << 8) | 0 = 0x100.
    // We want last VF on bus 255. RID of last VF = 0xFF00..0xFFFF → bus 255.
    // last_vf_rid = 0x100 + vf_offset + (total_vfs - 1) * vf_stride = 0xFF00
    // With total_vfs=1, stride=1: last_vf_rid = 0x100 + vf_offset = 0xFF00
    // → vf_offset = 0xFE00
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 1, 0xFE00, 1);

    // Second bridge on bus 0 → needs secondary bus, but next_bus wrapped to 0.
    // The bridge just needs to exist to trigger the next bus allocation.
    mock.add_bridge(0, 1, 0);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;

    // The SR-IOV reservation consumes up through bus 255. The next bridge
    // should fail with BusExhaustion, not silently wrap to bus 0.
    assert!(
        matches!(result, Err(crate::AssignmentError::BusExhaustion { .. })),
        "expected BusExhaustion when next_bus wraps past 255, got {result:?}"
    );
}

/// VF BARs from SR-IOV capability must be included in bridge window sizing.
#[async_test]
async fn sriov_vf_bars_included_in_bridge_window() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.set_multi_function(1, 0);
    mock.add_sriov_with_bars(
        1,
        0,
        0,
        4,
        1,
        1,
        &[(0, 0x1000, false, false)], // VF BAR0: 4 KB non-pref
    );

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let window = read_memory_window(&mock, 0, 0, 0).expect("bridge should have memory window");
    let window_size = window.1 - window.0 + 1;

    // PF BAR = 0x1000, VF BARs = 4 * 0x1000 = 0x4000, total = 0x5000.
    assert!(
        window_size >= 0x5000,
        "bridge window {window_size:#x} must be >= 0x5000"
    );

    // VF BAR0 should be programmed into the SR-IOV capability registers.
    let vf_bar = read_vf_bar32(&mock, 1, 0, 0, 0) as u64;
    assert!(vf_bar > 0, "VF BAR should be assigned, got {vf_bar:#x}");
    assert!(
        vf_bar >= window.0 && vf_bar + 0x4000 <= window.1 + 1,
        "VF BAR region {vf_bar:#x}..{:#x} must be within bridge window {:#x}..{:#x}",
        vf_bar + 0x4000,
        window.0,
        window.1 + 1,
    );
    // VF BAR region must not overlap PF BAR.
    let pf_bar = read_bar32(&mock, 1, 0, 0, 0) as u64;
    assert!(
        vf_bar + 0x4000 <= pf_bar || pf_bar + 0x1000 <= vf_bar,
        "VF BAR region must not overlap PF BAR"
    );
}

/// Non-power-of-two VF counts must not panic.
#[async_test]
async fn sriov_non_power_of_two_vf_count() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.set_multi_function(1, 0);
    mock.add_sriov_with_bars(
        1,
        0,
        0,
        3, // 3 VFs
        1,
        1,
        &[(0, 0x1000, false, false)],
    );

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let window = read_memory_window(&mock, 0, 0, 0).expect("bridge should have memory window");
    let window_size = window.1 - window.0 + 1;

    assert!(
        window_size >= 0x4000,
        "bridge window {window_size:#x} must be >= 0x4000"
    );
}

/// 64-bit prefetchable VF BARs should be placed in the high MMIO aperture.
#[async_test]
async fn sriov_vf_bars_64bit_prefetchable() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.set_multi_function(1, 0);
    mock.add_sriov_with_bars(
        1,
        0,
        0,
        4,
        1,
        1,
        &[(0, 0x10_0000, true, true)], // VF BAR0: 1 MB, 64-bit prefetchable
    );

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000),
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Non-pref window for PF's 32-bit BAR.
    let mem = read_memory_window(&mock, 0, 0, 0).expect("bridge should have non-pref window");
    let mem_size = mem.1 - mem.0 + 1;
    assert!(
        mem_size >= 0x1000,
        "non-pref window {mem_size:#x} must fit PF BAR (0x1000)"
    );

    // Pref window for VF BARs (4 * 1 MB).
    let pref = read_prefetchable_window(&mock, 0, 0, 0).expect("bridge should have pref window");
    let pref_size = pref.1 - pref.0 + 1;
    assert!(
        pref_size >= 0x40_0000,
        "pref window {pref_size:#x} must be >= 0x400000"
    );
    assert!(
        pref.0 >= 0x1_0000_0000,
        "pref window base {:#x} should be in high MMIO",
        pref.0
    );

    // VF BAR0 should be programmed as a 64-bit address in the high MMIO aperture.
    let vf_bar = read_vf_bar64(&mock, 1, 0, 0, 0);
    assert!(
        (0x1_0000_0000..0x2_0000_0000).contains(&vf_bar),
        "VF BAR {vf_bar:#x} should be in high MMIO"
    );
    assert!(
        vf_bar >= pref.0 && vf_bar + 0x40_0000 <= pref.1 + 1,
        "VF BAR region {vf_bar:#x}..{:#x} must be within pref window {:#x}..{:#x}",
        vf_bar + 0x40_0000,
        pref.0,
        pref.1 + 1,
    );
}

/// A PF with both 32-bit non-pref and 64-bit prefetchable VF BARs should
/// have space reserved in both bridge windows.
#[async_test]
async fn sriov_mixed_vf_bar_types() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    mock.add_endpoint(1, 0, 0, &[]);
    mock.set_multi_function(1, 0);
    mock.add_sriov_with_bars(
        1,
        0,
        0,
        2,
        1,
        1,
        &[
            (0, 0x1000, false, false), // VF BAR0: 4 KB non-pref
            (2, 0x1_0000, true, true), // VF BAR2: 64 KB 64-bit pref
        ],
    );

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000),
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let mem = read_memory_window(&mock, 0, 0, 0).expect("bridge should have non-pref window");
    let mem_size = mem.1 - mem.0 + 1;
    assert!(
        mem_size >= 0x2000,
        "non-pref window {mem_size:#x} must be >= 0x2000"
    );

    let pref = read_prefetchable_window(&mock, 0, 0, 0).expect("bridge should have pref window");
    let pref_size = pref.1 - pref.0 + 1;
    assert!(
        pref_size >= 0x2_0000,
        "pref window {pref_size:#x} must be >= 0x20000"
    );

    // VF BAR0 (non-pref) should be in the non-pref window.
    let vf_bar0 = read_vf_bar32(&mock, 1, 0, 0, 0) as u64;
    assert!(vf_bar0 > 0, "VF BAR0 should be assigned");
    assert!(
        vf_bar0 >= mem.0 && vf_bar0 + 0x2000 <= mem.1 + 1,
        "VF BAR0 region must be within non-pref window"
    );
    // VF BAR2 (64-bit pref) should be in the pref window.
    let vf_bar2 = read_vf_bar64(&mock, 1, 0, 0, 2);
    assert!(
        vf_bar2 >= pref.0 && vf_bar2 + 0x2_0000 <= pref.1 + 1,
        "VF BAR2 region must be within pref window"
    );
}

/// Top-level PF (no bridge) with VF BARs.
#[async_test]
async fn sriov_top_level_pf_no_bridge() {
    let mock = MockConfigSpace::new();

    mock.add_endpoint(0, 0, 0, &[(0, 0x1_0000, false, false)]);
    mock.set_multi_function(0, 0);
    mock.add_sriov_with_bars(
        0,
        0,
        0,
        4,
        1,
        1,
        &[(0, 0x1_0000, false, false)], // VF BAR0: 64 KB non-pref
    );

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // PF BAR should be programmed.
    let bar = read_bar32(&mock, 0, 0, 0, 0);
    assert!(bar > 0, "PF BAR should be assigned");
    let bar_end = bar as u64 + 0x1_0000;
    assert!(
        bar_end <= 0x2000_0000,
        "PF BAR must fit in low_mmio aperture"
    );

    // VF BAR0 should be programmed and not overlap the PF BAR.
    let vf_bar = read_vf_bar32(&mock, 0, 0, 0, 0) as u64;
    assert!(vf_bar > 0, "VF BAR should be assigned, got {vf_bar:#x}");
    assert!(
        vf_bar + 0x4_0000 <= 0x2000_0000,
        "VF BARs must fit in low_mmio aperture"
    );
    assert!(
        vf_bar + 0x4_0000 <= bar as u64 || bar_end <= vf_bar,
        "VF BAR region must not overlap PF BAR"
    );
}

/// Two PFs behind the same bridge, each with VF BARs.
#[async_test]
async fn sriov_multiple_pfs_behind_bridge() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // PF1 on bus 1, device 0: 4 KB device BAR, 2 VFs with 4 KB VF BAR.
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.set_multi_function(1, 0);
    mock.add_sriov_with_bars(1, 0, 0, 2, 1, 1, &[(0, 0x1000, false, false)]);

    // PF2 on bus 1, device 1: 8 KB device BAR, 3 VFs with 8 KB VF BAR.
    mock.add_endpoint(1, 1, 0, &[(0, 0x2000, false, false)]);
    mock.set_multi_function(1, 1);
    mock.add_sriov_with_bars(1, 1, 0, 3, 1, 1, &[(0, 0x2000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let window = read_memory_window(&mock, 0, 0, 0).expect("bridge should have memory window");
    let window_size = window.1 - window.0 + 1;

    // PF1: BAR=0x1000 + VF=2*0x1000=0x2000
    // PF2: BAR=0x2000 + VF=3*0x2000=0x6000
    // Total = 0x1000 + 0x2000 + 0x2000 + 0x6000 = 0xB000
    assert!(
        window_size >= 0xB000,
        "bridge window {window_size:#x} must be >= 0xB000"
    );

    // Both PF BARs should be programmed and not overlap.
    let pf1_bar = read_bar32(&mock, 1, 0, 0, 0) as u64;
    let pf2_bar = read_bar32(&mock, 1, 1, 0, 0) as u64;
    let pf1_size: u64 = 0x1000;
    let pf2_size: u64 = 0x2000;
    assert!(
        pf1_bar + pf1_size <= pf2_bar || pf2_bar + pf2_size <= pf1_bar,
        "PF BARs must not overlap"
    );

    // VF BARs should be programmed for both PFs.
    let vf1_bar = read_vf_bar32(&mock, 1, 0, 0, 0) as u64;
    let vf2_bar = read_vf_bar32(&mock, 1, 1, 0, 0) as u64;
    let vf1_size: u64 = 2 * 0x1000;
    let vf2_size: u64 = 3 * 0x2000;
    assert!(vf1_bar > 0, "PF1 VF BAR should be assigned");
    assert!(vf2_bar > 0, "PF2 VF BAR should be assigned");
    // VF BAR regions must not overlap each other.
    assert!(
        vf1_bar + vf1_size <= vf2_bar || vf2_bar + vf2_size <= vf1_bar,
        "VF BAR regions must not overlap each other"
    );
    // VF BAR regions must not overlap PF BARs.
    assert!(
        vf1_bar + vf1_size <= pf1_bar || pf1_bar + pf1_size <= vf1_bar,
        "PF1 VF BARs must not overlap PF1 BAR"
    );
    assert!(
        vf2_bar + vf2_size <= pf2_bar || pf2_bar + pf2_size <= vf2_bar,
        "PF2 VF BARs must not overlap PF2 BAR"
    );
}

/// VF BAR space contributing to MMIO exhaustion should produce an error.
#[async_test]
async fn sriov_vf_bars_cause_mmio_exhaustion() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.set_multi_function(1, 0);
    mock.add_sriov_with_bars(
        1,
        0,
        0,
        16,
        1,
        1,
        &[(0, 0x10_0000, false, false)], // VF BAR0: 1 MB each
    );

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x20_0000), // Only 2 MB — not enough for 16 MB of VF BARs
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        result.is_err(),
        "should fail with MMIO exhaustion, got {result:?}"
    );
}

// ---- Pinned BAR tests ----

/// Single device with one pinned BAR: the BAR should be assigned at the
/// pre-programmed address, not at the aperture base.
#[async_test]
async fn single_pinned_bar() {
    let mock = MockConfigSpace::new();
    // 64 KB 32-bit non-pref BAR at index 0.
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);
    // Pre-program BAR0 with a specific address.
    mock.set_bar_address(0, 0, 0, 0, 0x1050_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // BAR0 should be at the pinned address, not at the aperture base.
    assert_eq!(read_bar32(&mock, 0, 0, 0, 0), 0x1050_0000);
}

/// Two devices with pinned BARs behind a switch.
#[async_test]
async fn two_pinned_bars_behind_switch() {
    let mock = MockConfigSpace::new();

    // Upstream bridge.
    mock.add_bridge(0, 0, 0);
    // Downstream bridge A.
    mock.add_bridge(1, 0, 0);
    // Downstream bridge B.
    mock.add_bridge(1, 1, 0);

    // Endpoint A behind bridge A: 64 KB non-pref BAR, pinned.
    mock.add_endpoint(2, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(2, 0, 0, 0, 0x1080_0000);

    // Endpoint B behind bridge B: 64 KB non-pref BAR, pinned.
    mock.add_endpoint(3, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(3, 0, 0, 0, 0x1090_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Both BARs should be at their pinned addresses.
    assert_eq!(read_bar32(&mock, 2, 0, 0, 0), 0x1080_0000);
    assert_eq!(read_bar32(&mock, 3, 0, 0, 0), 0x1090_0000);

    // Bridge windows should encompass the pinned addresses.
    let win_a = read_memory_window(&mock, 1, 0, 0).expect("bridge A should have memory window");
    assert!(
        win_a.0 <= 0x1080_0000 && 0x1080_0000 + 0x10000 - 1 <= win_a.1,
        "bridge A window {:#x}..={:#x} must contain pinned BAR at 0x1080_0000",
        win_a.0,
        win_a.1
    );
    let win_b = read_memory_window(&mock, 1, 1, 0).expect("bridge B should have memory window");
    assert!(
        win_b.0 <= 0x1090_0000 && 0x1090_0000 + 0x10000 - 1 <= win_b.1,
        "bridge B window {:#x}..={:#x} must contain pinned BAR at 0x1090_0000",
        win_b.0,
        win_b.1
    );
}

/// Mixed pinned + dynamic BARs behind the same bridge.
#[async_test]
async fn mixed_pinned_and_dynamic_bars() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 64 KB non-pref BAR, pinned at 0x1020_0000.
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1020_0000);

    // Device B: 64 KB non-pref BAR, dynamic (not pinned).
    mock.add_endpoint(1, 1, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Pinned BAR at its fixed address.
    let bar_a = read_bar32(&mock, 1, 0, 0, 0) as u64;
    assert_eq!(bar_a, 0x1020_0000);

    // Dynamic BAR must not overlap the pinned BAR.
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;
    assert!(
        bar_a + 0x10000 <= bar_b || bar_b + 0x10000 <= bar_a,
        "BARs overlap: A={bar_a:#x}, B={bar_b:#x}"
    );
}

/// Pinned BAR misaligned to its size should produce an error.
#[async_test]
async fn pinned_bar_misaligned_error() {
    let mock = MockConfigSpace::new();
    // 64 KB BAR at index 0.
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);
    // Pre-program at a non-64KB-aligned address.
    mock.set_bar_address(0, 0, 0, 0, 0x1000_8000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        matches!(
            result,
            Err(crate::AssignmentError::PinnedBarMisaligned { .. })
        ),
        "expected PinnedBarMisaligned, got {result:?}"
    );
}

/// Overlapping pinned BARs should produce an error.
#[async_test]
async fn pinned_bar_overlap_error() {
    let mock = MockConfigSpace::new();
    // Two devices with overlapping pinned BARs.
    mock.add_endpoint(0, 0, 0, &[(0, 0x20000, false, false)]); // 128 KB
    mock.set_bar_address(0, 0, 0, 0, 0x1010_0000);

    mock.add_endpoint(0, 1, 0, &[(0, 0x10000, false, false)]); // 64 KB
    mock.set_bar_address(0, 1, 0, 0, 0x1011_0000); // overlaps with first

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        matches!(result, Err(crate::AssignmentError::PinnedBarOverlap { .. })),
        "expected PinnedBarOverlap, got {result:?}"
    );
}

/// Pinned BAR outside aperture should produce an error.
#[async_test]
async fn pinned_bar_out_of_aperture_error() {
    let mock = MockConfigSpace::new();
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);
    // Pinned at an address outside the low MMIO aperture.
    mock.set_bar_address(0, 0, 0, 0, 0x3000_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000), // ends at 0x2000_0000
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        matches!(
            result,
            Err(crate::AssignmentError::PinnedBarOutOfAperture { .. })
        ),
        "expected PinnedBarOutOfAperture, got {result:?}"
    );
}

/// A 64-bit non-prefetchable BAR pinned above 4 GB should be rejected.
/// The non-prefetchable bridge window is 32-bit only and cannot represent
/// addresses above 4 GB.
#[async_test]
async fn pinned_bar_64bit_non_prefetchable_above_4gb_rejected() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);
    // 1 MB 64-bit non-prefetchable BAR pinned above 4 GB.
    mock.add_endpoint(1, 0, 0, &[(0, 0x100000, true, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1_0010_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000),
        preserve_bars: true,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        matches!(
            result,
            Err(crate::AssignmentError::PinnedBarOutOfAperture { .. })
        ),
        "expected PinnedBarOutOfAperture, got {result:?}"
    );
}

/// Bridge window should be computed from pinned + dynamic children.
#[async_test]
async fn bridge_window_from_pinned_and_dynamic() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 1 MB non-pref BAR, pinned at 0x1020_0000.
    mock.add_endpoint(1, 0, 0, &[(0, 0x100000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1020_0000);

    // Device B: 1 MB non-pref BAR, dynamic.
    mock.add_endpoint(1, 1, 0, &[(0, 0x100000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let window = read_memory_window(&mock, 0, 0, 0).expect("bridge should have memory window");

    // Pinned BAR at 0x1020_0000 with size 1 MB → ends at 0x1030_0000.
    // Bridge window base should be align_down(0x1020_0000, 1 MB) = 0x1020_0000.
    assert!(
        window.0 <= 0x1020_0000,
        "window base {:#x} should be <= pinned BAR at 0x1020_0000",
        window.0
    );

    // Dynamic 1 MB BAR must not overlap pinned BAR.
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;
    assert!(
        bar_b + 0x100000 <= 0x1020_0000 || bar_b >= 0x1030_0000,
        "dynamic BAR {bar_b:#x} overlaps pinned BAR [0x1020_0000, 0x1030_0000)"
    );

    // Window should cover both BARs.
    assert!(
        window.0 <= bar_b && window.1 >= bar_b + 0x100000 - 1,
        "window {:#x}..={:#x} should cover dynamic BAR {bar_b:#x}..{:#x}",
        window.0,
        window.1,
        bar_b + 0x100000 - 1
    );
}

/// No-pin case with preserve_bars=true should produce identical results
/// to preserve_bars=false when no BARs are pre-programmed.
#[async_test]
async fn preserve_bars_no_pins_identical_to_dynamic() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.add_endpoint(1, 1, 0, &[(0, 0x20000, false, false)]);

    // Run with preserve_bars=false.
    let params_no_preserve = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg_a = mock.clone();
    assign_pci_resources(&mut cfg_a, &params_no_preserve)
        .await
        .unwrap();

    let bar_a0 = read_bar32(&mock, 1, 0, 0, 0);
    let bar_a1 = read_bar32(&mock, 1, 1, 0, 0);
    let window_a = read_memory_window(&mock, 0, 0, 0);

    // Re-create mock (reset BAR values).
    let mock2 = MockConfigSpace::new();
    mock2.add_bridge(0, 0, 0);
    mock2.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock2.add_endpoint(1, 1, 0, &[(0, 0x20000, false, false)]);

    // Run with preserve_bars=true but no pre-programmed addresses.
    let params_preserve = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg_b = mock2.clone();
    assign_pci_resources(&mut cfg_b, &params_preserve)
        .await
        .unwrap();

    let bar_b0 = read_bar32(&mock2, 1, 0, 0, 0);
    let bar_b1 = read_bar32(&mock2, 1, 1, 0, 0);
    let window_b = read_memory_window(&mock2, 0, 0, 0);

    // Results should be identical.
    assert_eq!(bar_a0, bar_b0, "BAR0 mismatch");
    assert_eq!(bar_a1, bar_b1, "BAR1 mismatch");
    assert_eq!(window_a, window_b, "bridge window mismatch");
}

/// 64-bit prefetchable pinned BAR should be placed in high MMIO at its
/// pre-programmed address.
#[async_test]
async fn pinned_bar_64bit_prefetchable() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);
    // 1 MB 64-bit prefetchable BAR at index 0.
    mock.add_endpoint(1, 0, 0, &[(0, 0x100000, true, true)]);
    mock.set_bar_address(1, 0, 0, 0, 0x3_8380_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::new(0x3_8380_0000..0x3_8380_0000 + 0x0100_0000),
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // BAR should be at the pinned address.
    assert_eq!(read_bar64(&mock, 1, 0, 0, 0), 0x3_8380_0000);

    // Bridge should have a prefetchable window, not a non-prefetchable one.
    assert!(
        read_memory_window(&mock, 0, 0, 0).is_none(),
        "no non-prefetchable window expected"
    );
    let pref = read_prefetchable_window(&mock, 0, 0, 0).expect("prefetchable window expected");
    assert!(
        pref.0 <= 0x3_8380_0000 && 0x3_8380_0000 + 0x100000 - 1 <= pref.1,
        "pref window {:#x}..={:#x} must contain pinned BAR",
        pref.0,
        pref.1
    );
}

/// Two pinned 64-bit prefetchable BARs: one below 4 GB (in the low MMIO
/// aperture) and one above 4 GB (in the high MMIO aperture). Pinned BARs
/// below 4 GB are treated as mem32 (prefetchable vs. non-prefetchable is
/// not a real distinction in virtualization), so the low BAR goes in the
/// non-prefetchable bridge window and the high BAR goes in the
/// prefetchable bridge window. Each window must stay within its aperture.
#[async_test]
async fn pinned_bar_64bit_prefetchable_in_low_mmio() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);
    // Device 0: 1 MB 64-bit prefetchable BAR pinned below 4 GB.
    mock.add_endpoint(1, 0, 0, &[(0, 0x100000, true, true)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1020_0000);
    // Device 1: 1 MB 64-bit prefetchable BAR pinned above 4 GB.
    mock.add_endpoint(1, 1, 0, &[(0, 0x100000, true, true)]);
    mock.set_bar_address(1, 1, 0, 0, 0x1_0010_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0x1_0000_0000),
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Both BARs should be at their pinned addresses.
    let bar_low = read_bar64(&mock, 1, 0, 0, 0);
    let bar_high = read_bar64(&mock, 1, 1, 0, 0);
    assert_eq!(bar_low, 0x1020_0000);
    assert_eq!(bar_high, 0x1_0010_0000);

    // Low BAR is treated as mem32, so it should be in the non-prefetchable window.
    let mem_win = read_memory_window(&mock, 0, 0, 0)
        .expect("bridge should have a non-prefetchable window for the low pinned BAR");
    assert!(
        mem_win.0 <= bar_low && bar_low + 0x100000 - 1 <= mem_win.1,
        "memory window {:#x}..={:#x} must contain low BAR [{:#x}..{:#x})",
        mem_win.0,
        mem_win.1,
        bar_low,
        bar_low + 0x100000
    );

    // High BAR stays in the prefetchable window.
    let pref_win = read_prefetchable_window(&mock, 0, 0, 0)
        .expect("bridge should have a prefetchable window for the high pinned BAR");
    assert!(
        pref_win.0 <= bar_high && bar_high + 0x100000 - 1 <= pref_win.1,
        "prefetchable window {:#x}..={:#x} must contain high BAR [{:#x}..{:#x})",
        pref_win.0,
        pref_win.1,
        bar_high,
        bar_high + 0x100000
    );

    // The two bridge windows must not overlap.
    assert!(
        mem_win.1 < pref_win.0 || pref_win.1 < mem_win.0,
        "bridge windows overlap: mem {:#x}..={:#x} vs pref {:#x}..={:#x}",
        mem_win.0,
        mem_win.1,
        pref_win.0,
        pref_win.1
    );
}

/// Two pinned BARs as siblings behind the same bridge. Both must be at
/// their pinned addresses and dynamic demands must follow.
#[async_test]
async fn two_pinned_bars_same_bridge() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 64 KB non-pref BAR, pinned.
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1020_0000);

    // Device B: 64 KB non-pref BAR, pinned.
    mock.add_endpoint(1, 1, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 1, 0, 0, 0x1030_0000);

    // Device C: 64 KB non-pref BAR, dynamic.
    mock.add_endpoint(1, 2, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let bar_a = read_bar32(&mock, 1, 0, 0, 0) as u64;
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;
    let bar_c = read_bar32(&mock, 1, 2, 0, 0) as u64;

    assert_eq!(bar_a, 0x1020_0000, "pinned BAR A");
    assert_eq!(bar_b, 0x1030_0000, "pinned BAR B");

    // Dynamic BAR should be placed in the gap between the two pinned
    // BARs (gap allocator), not after both of them.
    assert!(
        bar_c >= bar_a + 0x10000 && bar_c + 0x10000 <= bar_b,
        "dynamic BAR {bar_c:#x} should be between pinned A end {:#x} and B start {bar_b:#x}",
        bar_a + 0x10000,
    );

    // No overlaps.
    assert!(bar_a + 0x10000 <= bar_b, "A and B overlap");
    assert!(bar_a + 0x10000 <= bar_c, "A and C overlap");
    assert!(bar_c + 0x10000 <= bar_b, "C and B overlap");
}

/// Dynamic BAR with large alignment coexisting with a small pinned BAR.
/// The dynamic BAR's alignment must be honored even though the pinned
/// BAR occupies a lower address.
#[async_test]
async fn pinned_bar_with_large_alignment_dynamic() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 64 KB non-pref BAR, pinned at 0x1010_0000.
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1010_0000);

    // Device B: 1 MB non-pref BAR, dynamic (needs 1 MB alignment).
    mock.add_endpoint(1, 1, 0, &[(0, 0x100000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let bar_a = read_bar32(&mock, 1, 0, 0, 0) as u64;
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;

    assert_eq!(bar_a, 0x1010_0000, "pinned BAR should be at its address");

    // Dynamic 1 MB BAR must be 1 MB aligned.
    assert_eq!(
        bar_b % 0x100000,
        0,
        "dynamic 1 MB BAR at {bar_b:#x} is not 1 MB aligned"
    );

    // Must not overlap pinned BAR.
    assert!(
        bar_b + 0x100000 <= bar_a || bar_a + 0x10000 <= bar_b,
        "BARs overlap"
    );
}

/// Sibling bridge windows for pinned devices behind different downstream
/// bridges must not overlap.
#[async_test]
async fn pinned_sibling_bridge_windows_do_not_overlap() {
    let mock = MockConfigSpace::new();

    // Upstream bridge.
    mock.add_bridge(0, 0, 0);
    // Two downstream bridges.
    mock.add_bridge(1, 0, 0);
    mock.add_bridge(1, 1, 0);

    // Endpoint behind bridge A: 1 MB BAR pinned at 0x1010_0000.
    mock.add_endpoint(2, 0, 0, &[(0, 0x100000, false, false)]);
    mock.set_bar_address(2, 0, 0, 0, 0x1010_0000);

    // Endpoint behind bridge B: 1 MB BAR pinned at 0x1020_0000.
    mock.add_endpoint(3, 0, 0, &[(0, 0x100000, false, false)]);
    mock.set_bar_address(3, 0, 0, 0, 0x1020_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let win_a = read_memory_window(&mock, 1, 0, 0).expect("bridge A memory window");
    let win_b = read_memory_window(&mock, 1, 1, 0).expect("bridge B memory window");

    // Sibling windows must not overlap.
    assert!(
        win_a.1 < win_b.0 || win_b.1 < win_a.0,
        "sibling bridge windows overlap: A=[{:#x}..={:#x}], B=[{:#x}..={:#x}]",
        win_a.0,
        win_a.1,
        win_b.0,
        win_b.1
    );

    // Each window must contain its pinned BAR.
    assert!(
        win_a.0 <= 0x1010_0000 && 0x1010_0000 + 0x100000 - 1 <= win_a.1,
        "bridge A window must contain pinned BAR at 0x1010_0000"
    );
    assert!(
        win_b.0 <= 0x1020_0000 && 0x1020_0000 + 0x100000 - 1 <= win_b.1,
        "bridge B window must contain pinned BAR at 0x1020_0000"
    );
}

/// When preserve_bars=false, pre-programmed BAR values are ignored and
/// the BAR is dynamically allocated.
#[async_test]
async fn preserve_bars_false_ignores_preprogrammed() {
    let mock = MockConfigSpace::new();
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);
    // Pre-program BAR0 with a specific address.
    mock.set_bar_address(0, 0, 0, 0, 0x1050_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: false,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // BAR should be at the aperture base, not at the pre-programmed address.
    assert_eq!(
        read_bar32(&mock, 0, 0, 0, 0),
        0x1000_0000,
        "with preserve_bars=false, pre-programmed address should be ignored"
    );
}

/// When a pinned BAR leaves a gap between the bridge window base and the
/// pinned address, the gap allocator places dynamic demands in that gap
/// instead of wasting the space and extending the window.
#[async_test]
async fn gap_allocator_uses_space_before_pinned_bar() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 256 KB non-pref BAR, pinned at 0x1024_0000.
    // Bridge window base = align_down(0x1024_0000, 1 MB) = 0x1020_0000.
    // This leaves a 256 KB gap from 0x1020_0000..0x1024_0000.
    mock.add_endpoint(1, 0, 0, &[(0, 0x40000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1024_0000);

    // Device B: 64 KB non-pref BAR, dynamic. Small enough to fit in the
    // 256 KB gap before the pinned BAR.
    mock.add_endpoint(1, 1, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let bar_a = read_bar32(&mock, 1, 0, 0, 0) as u64;
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;

    // Pinned BAR stays at its fixed address.
    assert_eq!(bar_a, 0x1024_0000, "pinned BAR should be at its address");

    // Dynamic BAR should be placed in the gap *before* the pinned BAR.
    assert!(
        bar_b >= 0x1020_0000 && bar_b + 0x10000 <= 0x1024_0000,
        "dynamic BAR {bar_b:#x} should be in the gap [0x1020_0000, 0x1024_0000)"
    );
}

/// Dynamic BAR fits in a gap between two pinned BARs instead of being
/// placed after both of them.
#[async_test]
async fn gap_allocator_uses_space_between_pinned_bars() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 64 KB non-pref BAR, pinned at 0x1020_0000.
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1020_0000);

    // Device B: 64 KB non-pref BAR, pinned at 0x1040_0000.
    // Gap between A's end and B's start: [0x1030_0000, 0x1040_0000).
    mock.add_endpoint(1, 1, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 1, 0, 0, 0x1040_0000);

    // Device C: 64 KB non-pref BAR, dynamic.
    mock.add_endpoint(1, 2, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x1000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let bar_a = read_bar32(&mock, 1, 0, 0, 0) as u64;
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;
    let bar_c = read_bar32(&mock, 1, 2, 0, 0) as u64;

    assert_eq!(bar_a, 0x1020_0000, "pinned BAR A");
    assert_eq!(bar_b, 0x1040_0000, "pinned BAR B");

    // Dynamic BAR should be placed in the gap between the two pinned BARs.
    assert!(
        bar_c >= bar_a + 0x10000 && bar_c + 0x10000 <= bar_b,
        "dynamic BAR {bar_c:#x} should be between pinned A end {:#x} and pinned B start {:#x}",
        bar_a + 0x10000,
        bar_b,
    );
}

/// Sizing must account for gaps within the pinned span. A dynamic BAR
/// that fits in the gap before a pinned BAR should not inflate the
/// bridge window beyond the pinned span.
#[async_test]
async fn sizing_accounts_for_gap_before_pinned_bar() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 512 KB non-pref BAR, pinned at 0x1018_0000.
    // constrained_base = align_down(0x1018_0000, 1 MB) = 0x1010_0000.
    // Pinned span = 0x1010_0000..0x1020_0000 = 1 MB.
    // Gap before pin: [0x1010_0000, 0x1018_0000) = 512 KB.
    mock.add_endpoint(1, 0, 0, &[(0, 0x80000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1018_0000);

    // Device B: 256 KB non-pref BAR, dynamic. Fits in the 512 KB gap.
    mock.add_endpoint(1, 1, 0, &[(0, 0x40000, false, false)]);

    // Aperture is exactly 1 MB at the constrained base. With gap-aware
    // sizing the bridge window is 1 MB and fits. Without gap-aware
    // sizing the window inflates to 2 MB and overflows.
    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1010_0000..0x1010_0000 + 0x10_0000), // 1 MB
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        result.is_ok(),
        "1 MB aperture should suffice when dynamic BAR fits in gap: {result:?}"
    );

    // Verify placement.
    let bar_a = read_bar32(&mock, 1, 0, 0, 0) as u64;
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;
    assert_eq!(bar_a, 0x1018_0000, "pinned BAR");
    assert!(
        bar_b >= 0x1010_0000 && bar_b + 0x40000 <= 0x1018_0000,
        "dynamic BAR {bar_b:#x} should be in the gap [0x1010_0000, 0x1018_0000)"
    );
}

/// Sizing must account for gaps between pinned BARs. A dynamic BAR
/// that fits between two pinned BARs should not extend the window.
#[async_test]
async fn sizing_accounts_for_gap_between_pinned_bars() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 64 KB non-pref BAR, pinned at 0x1020_0000.
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1020_0000);

    // Device B: 64 KB non-pref BAR, pinned at 0x1110_0000.
    // Pinned span base = 0x1020_0000, end = 0x1120_0000 = 16 MB.
    // Gap between: [0x1030_0000, 0x1110_0000) = 14 MB.
    mock.add_endpoint(1, 1, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 1, 0, 0, 0x1110_0000);

    // Device C: 1 MB non-pref BAR, dynamic. Fits in the 14 MB gap.
    mock.add_endpoint(1, 2, 0, &[(0, 0x100000, false, false)]);

    // Aperture = 16 MB starting at constrained base. Gap-aware sizing
    // keeps the window at 16 MB; naive sizing adds 1 MB → 17 MB → overflow.
    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1020_0000..0x1020_0000 + 0x100_0000), // 16 MB
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        result.is_ok(),
        "16 MB aperture should suffice when dynamic BAR fits in gap: {result:?}"
    );

    // Dynamic BAR should be between the two pinned BARs.
    let bar_c = read_bar32(&mock, 1, 2, 0, 0) as u64;
    assert!(
        bar_c >= 0x1030_0000 && bar_c + 0x100000 <= 0x1110_0000,
        "dynamic BAR {bar_c:#x} should be in the gap between pinned BARs"
    );
}

/// Sizing underestimate when constrained_base is not aligned to a large
/// dynamic demand's alignment.
///
/// The trial overflow path in compute_subtree_state uses relative offsets
/// for alignment padding:
///     pool.mem = align_up(pool.mem, alignment) + size
///
/// But the real allocator in assign_subtree uses absolute addresses.
/// When the pinned span is an exact multiple of the dynamic BAR's
/// alignment but constrained_base is not, the trial computes zero
/// alignment padding while the real allocation needs up to
/// (alignment - 1) bytes of padding, causing the gap to be too small.
#[async_test]
async fn sizing_underestimate_constrained_base_misaligned() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Device A: 64 KB non-pref BAR, pinned at 0x1010_0000.
    // constrained_base = align_down(0x1010_0000, 1 MB) = 0x1010_0000.
    // This is NOT 16 MB aligned (0x1010_0000 % 0x100_0000 = 0x10_0000).
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1010_0000);

    // Device B: 1 MB non-pref BAR, pinned at 0x1100_0000.
    // Pinned span: [0x1010_0000, 0x1110_0000) = 16 MB (0x100_0000).
    // 16 MB is an exact multiple of the dynamic BAR's alignment (16 MB),
    // so align_up(16 MB, 16 MB) = 16 MB — the trial adds zero padding.
    mock.add_endpoint(1, 1, 0, &[(0, 0x100000, false, false)]);
    mock.set_bar_address(1, 1, 0, 0, 0x1100_0000);

    // Device C: 16 MB non-pref BAR, dynamic (not pinned).
    //
    // Trial overflow path:
    //   pool.mem = 0x100_0000 (pinned span = 16 MB)
    //   align_up(0x100_0000, 0x100_0000) = 0x100_0000 (no padding!)
    //   pool.mem = 0x100_0000 + 0x100_0000 = 0x200_0000 (32 MB)
    //   Window: [0x1010_0000, 0x1210_0000).
    //
    // Real allocation: tail gap = [0x1110_0000, 0x1210_0000) = 16 MB.
    //   align_up(0x1110_0000, 0x100_0000) = 0x1200_0000
    //   0x1200_0000 + 0x100_0000 = 0x1300_0000 > 0x1210_0000 → panic!
    mock.add_endpoint(1, 2, 0, &[(0, 0x1000000, false, false)]);

    // Aperture is generous (1 GB). The issue is that the bridge window
    // is undersized, not that the aperture is too small.
    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1000_0000..0x1000_0000 + 0x4000_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    // This should succeed — there is more than enough aperture space.
    // BUG: the sizing pass underestimates the bridge window, causing
    // assign_subtree to panic ("demand must fit").
    assert!(
        result.is_ok(),
        "should succeed with generous aperture: {result:?}"
    );

    // Verify pinned BARs are at their expected addresses.
    let bar_a = read_bar32(&mock, 1, 0, 0, 0) as u64;
    let bar_b = read_bar32(&mock, 1, 1, 0, 0) as u64;
    let bar_c = read_bar32(&mock, 1, 2, 0, 0) as u64;
    assert_eq!(bar_a, 0x1010_0000, "pinned BAR A");
    assert_eq!(bar_b, 0x1100_0000, "pinned BAR B");

    // Dynamic 16 MB BAR must be 16 MB aligned and not overlap pinned BARs.
    assert_eq!(
        bar_c % 0x100_0000,
        0,
        "dynamic 16 MB BAR at {bar_c:#x} must be 16 MB aligned"
    );
    assert!(
        bar_c + 0x100_0000 <= bar_a || bar_c >= 0x1110_0000,
        "dynamic BAR at {bar_c:#x} must not overlap pinned BARs"
    );
}

/// Constrained bridge window can start before the aperture when the
/// aperture base is not 1 MB aligned. The pinned BAR is inside the
/// aperture, but align_down to bridge granularity pushes the window
/// base before it. Dynamic BARs placed in that pre-aperture gap would
/// be outside the aperture, triggering an assertion failure.
#[async_test]
async fn constrained_base_before_aperture_returns_error() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    // Pinned BAR at 0x1008_0000 (64 KB) — inside the aperture.
    // constrained_base = align_down(0x1008_0000, 1 MB) = 0x1000_0000,
    // which is BEFORE the aperture base of 0x1008_0000.
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);
    mock.set_bar_address(1, 0, 0, 0, 0x1008_0000);

    // Dynamic BAR (64 KB) — would be placed in the gap
    // [0x1000_0000, 0x1008_0000) which is outside the aperture.
    mock.add_endpoint(1, 1, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::new(0x1008_0000..0x1008_0000 + 0x100_0000),
        high_mmio: MemoryRange::EMPTY,
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    // Should return an error, not panic with an assertion failure.
    assert!(
        result.is_err(),
        "should fail when constrained bridge window starts before aperture"
    );
}

/// A pinned BAR address near u64::MAX must not cause arithmetic overflow
/// in the overlap or aperture containment checks. Without saturating_add,
/// `addr + size` wraps to a small value that passes the `<= aperture_end`
/// check, causing the BAR to be incorrectly accepted.
#[async_test]
async fn pinned_bar_near_u64_max_no_overflow() {
    let mock = MockConfigSpace::new();

    // 64-bit prefetchable 1 MB BAR, pinned at an address where
    // addr + size wraps: 0xFFFF_FFFF_FFF0_0000 + 0x10_0000 = 0x0.
    // With wrapping, bar_end=0 < aperture_end, so it looks like it fits.
    mock.add_endpoint(0, 0, 0, &[(0, 0x10_0000, true, true)]);
    mock.set_bar_address(0, 0, 0, 0, 0xFFFF_FFFF_FFF0_0000);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: MemoryRange::EMPTY,
        high_mmio: MemoryRange::new(0x1_0000_0000..0x1_0000_0000 + 0xFFFF_FFFE_0000_0000),
        preserve_bars: true,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    // Must return PinnedBarOutOfAperture. Without saturating_add, the
    // wrapped bar_end (0x0) passes the containment check and the BAR
    // is incorrectly accepted.
    assert!(
        result.is_err(),
        "near-u64::MAX pinned BAR should be rejected, not overflow"
    );
    let err = result.unwrap_err();
    assert!(
        matches!(err, crate::AssignmentError::PinnedBarOutOfAperture { .. }),
        "expected PinnedBarOutOfAperture, got {err}"
    );
}
