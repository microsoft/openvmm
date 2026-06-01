// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(test)]

use crate::AssignmentParams;
use crate::MmioAperture;
use crate::PciConfigAccess;
use crate::assign_pci_resources;
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

// ---- Tests ----

#[async_test]
async fn single_endpoint_32bit_bar() {
    let mock = MockConfigSpace::new();
    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // BAR0 should be assigned at the aperture base.
    assert_eq!(read_bar32(&mock, 0, 0, 0, 0), 0x1000_0000);
}

#[async_test]
async fn single_endpoint_64bit_bar() {
    let mock = MockConfigSpace::new();
    mock.add_endpoint(0, 1, 0, &[(0, 0x100000, true, true)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: None,
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000,
            len: 0x1_0000_0000,
        }),
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    assert_eq!(read_bar64(&mock, 0, 1, 0, 0), 0x1_0000_0000);
}

#[async_test]
async fn bridge_with_endpoint() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);
    mock.add_endpoint(1, 0, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
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

    // Device 0: 4KB BAR.
    mock.add_endpoint(0, 0, 0, &[(0, 0x1000, false, false)]);
    // Device 1: 1MB BAR.
    mock.add_endpoint(0, 1, 0, &[(0, 0x100000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // 1 MB BAR (dev 1) should be first, at the aperture base.
    assert_eq!(read_bar32(&mock, 0, 1, 0, 0), 0x1000_0000);
    // 4 KB BAR (dev 0) should follow.
    assert_eq!(read_bar32(&mock, 0, 0, 0, 0), 0x1010_0000);
}

#[async_test]
async fn multi_function_device() {
    let mock = MockConfigSpace::new();

    mock.add_endpoint(0, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_endpoint(0, 0, 1, &[(0, 0x1000, false, false)]);
    mock.set_multi_function(0, 0);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000,
            len: 0x1_0000_0000,
        }),
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // Upstream bridge.
    let (_, secondary, _) = read_bus_numbers(&mock, 0, 0, 0);
    assert_eq!(secondary, 1);

    // 32-bit BAR on bus 2 should be in low MMIO.
    let ep1_bar = read_bar32(&mock, 2, 0, 0, 0) as u64;
    assert!(ep1_bar >= 0x1000_0000);
    assert!(ep1_bar < 0x2000_0000);

    // 64-bit BAR on bus 3 should be in high MMIO.
    let ep2_bar = read_bar64(&mock, 3, 0, 0, 0);
    assert!(ep2_bar >= 0x1_0000_0000);

    // Bridge for 64-bit endpoint should have prefetchable window.
    let pref = read_prefetchable_window(&mock, 1, 1, 0);
    assert!(
        pref.is_some(),
        "bridge behind 64-bit endpoint should have prefetchable window"
    );
    assert!(pref.unwrap().0 >= 0x1_0000_0000);

    // Bridge for 32-bit endpoint should have non-pref window but no pref window.
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
        low_mmio: None,
        high_mmio: None,
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x20000, // 128KB — not enough for both
        }),
        high_mmio: None,
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
        low_mmio: None,
        high_mmio: None,
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

    mock.add_bridge(0, 0, 0);

    // Endpoint on bus 1 with SR-IOV: 256 VFs, offset=1, stride=1.
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 256, 1, 1);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

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

    mock.add_bridge(0, 0, 0);

    // Endpoint on bus 1 with SR-IOV but 0 VFs.
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 0, 1, 1);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let (_, _, subordinate) = read_bus_numbers(&mock, 0, 0, 0);
    assert_eq!(subordinate, 1);
}

#[async_test]
async fn bridge_prefetchable_window_programmed() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);
    mock.add_endpoint(1, 0, 0, &[(0, 0x100000, true, true)]); // 1 MB 64-bit pref

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: None,
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000,
            len: 0x1_0000_0000,
        }),
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    // No non-prefetchable window.
    assert!(
        read_memory_window(&mock, 0, 0, 0).is_none(),
        "no non-prefetchable window expected"
    );

    // Prefetchable window should exist.
    let pref = read_prefetchable_window(&mock, 0, 0, 0);
    assert!(pref.is_some(), "prefetchable window expected");
    assert!(pref.unwrap().0 >= 0x1_0000_0000);

    // Verify the prefetchable window registers were programmed.
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let a_window = read_memory_window(&mock, 1, 0, 0).expect("bridge A should have memory window");
    let b_window = read_memory_window(&mock, 1, 1, 0).expect("bridge B should have memory window");

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

    mock.add_bridge(0, 0, 0);

    // Endpoint behind bridge with a 1 GB BAR (needs 1 GB natural alignment).
    mock.add_endpoint(1, 0, 0, &[(0, 0x4000_0000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,  // 256 MB — not 1 GB aligned
            len: 0x2_0000_0000, // 8 GB
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

    let window = read_memory_window(&mock, 0, 0, 0).expect("bridge should have memory window");
    let bar_addr = read_bar32(&mock, 1, 0, 0, 0) as u64;

    // BAR must be naturally aligned.
    assert_eq!(
        bar_addr % 0x4000_0000,
        0,
        "1 GB BAR at {bar_addr:#x} is not 1 GB aligned"
    );

    // BAR must fit within bridge window.
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x500000, // 5 MB
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    assert!(
        result.is_ok(),
        "5 MB aperture should be sufficient: {}",
        result.unwrap_err()
    );
}

#[async_test]
async fn misaligned_aperture_does_not_overflow() {
    let mock = MockConfigSpace::new();

    mock.add_endpoint(0, 0, 0, &[(0, 0x400000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1020_0000, // 2 MB aligned, NOT 4 MB aligned
            len: 0x400000,     // 4 MB — exactly the BAR size, but not enough after alignment
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources(&mut cfg, &params).await;

    assert!(
        matches!(result, Err(crate::AssignmentError::MmioExhaustion { .. })),
        "expected MmioExhaustion for misaligned aperture, got {result:?}"
    );
}

#[async_test]
async fn alignment_exceeds_aperture_returns_error() {
    let mock = MockConfigSpace::new();

    mock.add_endpoint(0, 0, 0, &[(0, 0x1000000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1020_0000, // 2 MB past a 16 MB boundary
            len: 0x200000,     // 2 MB total
        }),
        high_mmio: None,
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

    mock.add_bridge(0, 0, 0);

    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 256, 1, 1);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 1, // Only buses 0 and 1 allowed.
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;

    assert!(
        matches!(result, Err(crate::AssignmentError::BusExhaustion { .. })),
        "expected BusExhaustion when SR-IOV reservation exceeds end_bus, got {result:?}"
    );
}

/// 32-bit BARs must never be placed above 4 GB.
#[async_test]
async fn mem32_bar_must_not_be_placed_above_4gb() {
    let mock = MockConfigSpace::new();

    mock.add_endpoint(0, 0, 0, &[(0, 0x10000, false, false)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: None,
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000, // Above 4 GB
            len: 0x1_0000_0000,
        }),
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;

    assert!(
        result.is_err(),
        "expected error when only aperture is above 4 GB"
    );
}

/// When both mem32 and mem64 pools share the same aperture, BAR regions
/// must not overlap.
#[async_test]
async fn shared_aperture_mem32_mem64_must_not_overlap() {
    let mock = MockConfigSpace::new();

    // Device 0: 4 MB 32-bit non-prefetchable BAR.
    mock.add_endpoint(0, 0, 0, &[(0, 0x40_0000, false, false)]);
    // Device 1: 1 MB 64-bit prefetchable BAR.
    mock.add_endpoint(0, 1, 0, &[(0, 0x10_0000, true, true)]);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1020_0000,
            len: 0x0100_0000, // 16 MB — plenty of room
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    assign_pci_resources(&mut cfg, &params).await.unwrap();

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

/// When bus 255 is consumed, next_bus must not wrap to 0.
#[async_test]
async fn bus_wrap_to_zero_must_return_exhaustion() {
    let mock = MockConfigSpace::new();

    mock.add_bridge(0, 0, 0);

    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 1, 0xFE00, 1);

    mock.add_bridge(0, 1, 0);

    let params = AssignmentParams {
        start_bus: 0,
        end_bus: 255,
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;

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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000,
            len: 0x1_0000_0000,
        }),
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000,
            len: 0x1_0000_0000,
        }),
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x20_0000, // Only 2 MB — not enough for 16 MB of VF BARs
        }),
        high_mmio: None,
    };

    let mut cfg = mock;
    let result = assign_pci_resources(&mut cfg, &params).await;
    assert!(
        result.is_err(),
        "should fail with MMIO exhaustion, got {result:?}"
    );
}
