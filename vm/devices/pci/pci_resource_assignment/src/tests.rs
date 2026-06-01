// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(test)]

use crate::AssignmentParams;
use crate::MmioAperture;
use crate::PciConfigAccess;
use crate::assign_pci_resources_inner;
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
    fn add_sriov(
        &self,
        bus: u8,
        device: u8,
        function: u8,
        total_vfs: u16,
        vf_offset: u16,
        vf_stride: u16,
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

        // Check if this is a BAR offset being probed.
        let is_bar_offset = (0x10..=0x24).contains(&offset) && (offset & 0x3) == 0;
        if is_bar_offset && inner.bar_masks.contains_key(&key) {
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

// ---- Tests ----

#[async_test]
async fn single_endpoint_32bit_bar() {
    let mock = MockConfigSpace::new();
    // Device on bus 0, device 0, function 0 with a 64KB 32-bit non-pref BAR at index 0.
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    assert_eq!(result.entries.len(), 1);
    let entry = &result.entries[0];
    assert_eq!(entry.bus, 0);
    assert_eq!(entry.device, 0);
    assert_eq!(entry.function, 0);
    assert_eq!(entry.bars.len(), 1);
    assert_eq!(entry.bars[0].address, 0x1000_0000);
    assert_eq!(entry.bars[0].size, 0x10000);
    assert!(!entry.bars[0].is_64bit);

    // Verify BAR was programmed in config space.
    let bar_val = mock.read_reg(0, 0, 0, 0x10);
    assert_eq!(bar_val, 0x1000_0000);
}

#[async_test]
async fn single_endpoint_64bit_bar() {
    let mock = MockConfigSpace::new();
    // 1 MB 64-bit prefetchable BAR at index 0.
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    assert_eq!(result.entries.len(), 1);
    let bar = &result.entries[0].bars[0];
    assert_eq!(bar.address, 0x1_0000_0000);
    assert_eq!(bar.size, 0x100000);
    assert!(bar.is_64bit);

    // Verify both BAR registers were programmed.
    let bar_lo = mock.read_reg(0, 1, 0, 0x10);
    let bar_hi = mock.read_reg(0, 1, 0, 0x14);
    assert_eq!(bar_lo, 0x0000_0000);
    assert_eq!(bar_hi, 0x0000_0001);
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // Should have 2 entries: bridge + endpoint.
    assert_eq!(result.entries.len(), 2);

    // Find the bridge entry.
    let bridge = result
        .entries
        .iter()
        .find(|e| e.bus == 0 && e.device == 0)
        .unwrap();
    assert_eq!(bridge.secondary_bus, Some(1));
    assert_eq!(bridge.subordinate_bus, Some(1));
    assert!(bridge.memory_base.is_some());
    assert!(bridge.memory_limit.is_some());

    // Find the endpoint entry.
    let endpoint = result
        .entries
        .iter()
        .find(|e| e.bus == 1 && e.device == 0)
        .unwrap();
    assert_eq!(endpoint.bars.len(), 1);
    assert_eq!(endpoint.bars[0].size, 0x10000);

    // Verify bus numbers were programmed on the bridge.
    let bus_reg = mock.read_reg(0, 0, 0, 0x18);
    assert_eq!(bus_reg & 0xFF, 0, "primary bus");
    assert_eq!((bus_reg >> 8) & 0xFF, 1, "secondary bus");
    assert_eq!((bus_reg >> 16) & 0xFF, 1, "subordinate bus");
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    assert_eq!(result.entries.len(), 2);

    // The 1MB BAR should be allocated first (sorted by size desc) and
    // aligned to 1MB.
    let dev1 = result.entries.iter().find(|e| e.device == 1).unwrap();
    assert_eq!(dev1.bars[0].address, 0x1000_0000);
    assert_eq!(dev1.bars[0].size, 0x100000);

    // The 4KB BAR should follow.
    let dev0 = result.entries.iter().find(|e| e.device == 0).unwrap();
    assert_eq!(dev0.bars[0].address, 0x1010_0000);
    assert_eq!(dev0.bars[0].size, 0x1000);
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // Should find both functions.
    assert_eq!(result.entries.len(), 2);
    let f0 = result.entries.iter().find(|e| e.function == 0).unwrap();
    let f1 = result.entries.iter().find(|e| e.function == 1).unwrap();
    assert_ne!(f0.bars[0].address, f1.bars[0].address);
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // Should have entries for: upstream bridge, 2 downstream bridges, 2 endpoints.
    assert!(result.entries.len() >= 4);

    // Verify bus numbers were assigned correctly.
    let upstream = result
        .entries
        .iter()
        .find(|e| e.bus == 0 && e.device == 0)
        .unwrap();
    assert_eq!(upstream.secondary_bus, Some(1));

    // Both endpoints should have BAR addresses assigned.
    let ep1 = result.entries.iter().find(|e| e.bus == 2).unwrap();
    let ep2 = result.entries.iter().find(|e| e.bus == 3).unwrap();
    assert_eq!(ep1.bars.len(), 1);
    assert_eq!(ep2.bars.len(), 1);
    // 32-bit BAR on bus 2 should be in low MMIO.
    assert!(ep1.bars[0].address >= 0x1000_0000);
    assert!(ep1.bars[0].address < 0x2000_0000);
    // 64-bit BAR on bus 3 should be in high MMIO.
    assert!(ep2.bars[0].address >= 0x1_0000_0000);

    // The downstream bridge for the 64-bit endpoint should have a
    // prefetchable window covering high MMIO.
    let ds_bridge_64 = result
        .entries
        .iter()
        .find(|e| e.bus == 1 && e.device == 1)
        .unwrap();
    assert!(
        ds_bridge_64.prefetchable_base.is_some(),
        "bridge behind 64-bit endpoint should have prefetchable window"
    );
    assert!(ds_bridge_64.prefetchable_base.unwrap() >= 0x1_0000_0000);

    // The downstream bridge for the 32-bit endpoint should have a
    // non-prefetchable window in low MMIO, and no prefetchable window.
    let ds_bridge_32 = result
        .entries
        .iter()
        .find(|e| e.bus == 1 && e.device == 0)
        .unwrap();
    assert!(ds_bridge_32.memory_base.is_some());
    assert!(ds_bridge_32.prefetchable_base.is_none());
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await;
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();
    assert!(result.entries.is_empty());
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await;
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await;
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // Bridge should have subordinate >= 2 to cover VF buses.
    let bridge = result
        .entries
        .iter()
        .find(|e| e.bus == 0 && e.device == 0)
        .unwrap();
    assert_eq!(bridge.secondary_bus, Some(1));
    assert!(
        bridge.subordinate_bus.unwrap() >= 2,
        "subordinate bus {} should be >= 2 to cover SR-IOV VFs",
        bridge.subordinate_bus.unwrap()
    );

    // Verify bus numbers programmed on the bridge.
    let bus_reg = mock.read_reg(0, 0, 0, 0x18);
    let subordinate = (bus_reg >> 16) & 0xFF;
    assert!(subordinate >= 2, "subordinate {subordinate} should be >= 2");
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // No extra buses reserved — subordinate should be 1.
    let bridge = result
        .entries
        .iter()
        .find(|e| e.bus == 0 && e.device == 0)
        .unwrap();
    assert_eq!(bridge.subordinate_bus, Some(1));
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
        low_mmio: None,
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000,
            len: 0x1_0000_0000,
        }),
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // The bridge should have a prefetchable window, not a non-prefetchable one.
    let bridge = result
        .entries
        .iter()
        .find(|e| e.bus == 0 && e.device == 0)
        .unwrap();
    assert!(
        bridge.memory_base.is_none(),
        "no non-prefetchable window expected"
    );
    assert!(
        bridge.prefetchable_base.is_some(),
        "prefetchable window expected"
    );
    assert!(bridge.prefetchable_base.unwrap() >= 0x1_0000_0000);

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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // Find bridge windows for the two downstream bridges.
    let bridge_a = result
        .entries
        .iter()
        .find(|e| e.bus == 1 && e.device == 0)
        .unwrap();
    let bridge_b = result
        .entries
        .iter()
        .find(|e| e.bus == 1 && e.device == 1)
        .unwrap();

    let a_base = bridge_a
        .memory_base
        .expect("bridge A should have memory window");
    let a_limit = bridge_a
        .memory_limit
        .expect("bridge A should have memory window");
    let b_base = bridge_b
        .memory_base
        .expect("bridge B should have memory window");
    let b_limit = bridge_b
        .memory_limit
        .expect("bridge B should have memory window");

    // Sibling bridge windows must not overlap — if they do, both bridges
    // will claim the same addresses on the upstream bus.
    assert!(
        a_limit < b_base || b_limit < a_base,
        "bridge windows overlap: A=[{a_base:#x}..={a_limit:#x}], B=[{b_base:#x}..={b_limit:#x}]"
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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,  // 256 MB — not 1 GB aligned
            len: 0x2_0000_0000, // 8 GB
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    let bridge = result
        .entries
        .iter()
        .find(|e| e.bus == 0 && e.device == 0)
        .unwrap();
    let ep = result
        .entries
        .iter()
        .find(|e| e.bus == 1 && e.device == 0)
        .unwrap();

    let window_base = bridge
        .memory_base
        .expect("bridge should have memory window");
    let window_limit = bridge
        .memory_limit
        .expect("bridge should have memory window");
    let bar_addr = ep.bars[0].address;
    let bar_end = bar_addr + ep.bars[0].size - 1;

    // The BAR must be naturally aligned.
    assert_eq!(
        bar_addr % 0x4000_0000,
        0,
        "1 GB BAR at {bar_addr:#x} is not 1 GB aligned"
    );

    // The BAR must fit within the bridge window.
    assert!(
        bar_addr >= window_base && bar_end <= window_limit,
        "BAR [{bar_addr:#x}..={bar_end:#x}] overflows bridge window [{window_base:#x}..={window_limit:#x}]"
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

    // 5 MB aperture. With alignment-first ordering (B then A), total is
    // exactly 5 MB: B at 0 (2 MB aligned, uses 2 MB), A at 2 MB (1 MB
    // aligned, uses 3 MB). With size-first ordering (A then B), A uses
    // 3 MB, then B needs 2 MB alignment → next 2 MB boundary is 4 MB,
    // total = 6 MB, which overflows.
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
    let result = assign_pci_resources_inner(&mut cfg, &params).await;

    assert!(
        result.is_ok(),
        "5 MB aperture should be sufficient for 3 MB + 2 MB: {}",
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
        low_mmio: Some(MmioAperture {
            base: 0x1020_0000, // 2 MB aligned, NOT 4 MB aligned
            len: 0x400000,     // 4 MB — exactly the BAR size, but not enough after alignment
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await;

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
        low_mmio: Some(MmioAperture {
            base: 0x1020_0000, // 2 MB past a 16 MB boundary
            len: 0x200000,     // 2 MB total
        }),
        high_mmio: None,
    };

    let mut cfg = mock.clone();
    let result = assign_pci_resources_inner(&mut cfg, &params).await;

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
        low_mmio: Some(MmioAperture {
            base: 0x1000_0000,
            len: 0x1000_0000,
        }),
        high_mmio: None,
    };

    let mut cfg = mock;
    let result = assign_pci_resources_inner(&mut cfg, &params).await;

    // SR-IOV VFs need bus 2, but end_bus is 1. The code returns
    // BusExhaustion because the VF bus range exceeds the allowed range.
    assert!(
        matches!(result, Err(crate::AssignmentError::BusExhaustion { .. })),
        "expected BusExhaustion when SR-IOV reservation exceeds end_bus, got {result:?}"
    );
}

/// Bug B: When low_mmio is None, 32-bit (non-prefetchable) BARs fall back
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
        low_mmio: None,
        high_mmio: Some(MmioAperture {
            base: 0x1_0000_0000, // Above 4 GB
            len: 0x1_0000_0000,
        }),
    };

    let mut cfg = mock;
    let result = assign_pci_resources_inner(&mut cfg, &params).await;

    // No sub-4GB aperture exists, so 32-bit BARs cannot be placed.
    // Must fail rather than silently placing them above 4 GB.
    assert!(
        result.is_err(),
        "expected error when only aperture is above 4 GB, but got assignments: {:#?}",
        result.unwrap().entries
    );
}

/// Bug C: When both mem32 and mem64 pools share the same aperture (only
/// one of low_mmio/high_mmio is provided), the mem64 base is computed
/// from `aperture.base + root_req.mem32` instead of from the actual
/// aligned mem32 end address. If aperture.base needs alignment for mem32,
/// mem64 can start inside the mem32 region, causing overlapping
/// assignments.
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
        low_mmio: Some(MmioAperture {
            base: 0x1020_0000,
            len: 0x0100_0000, // 16 MB — plenty of room
        }),
        high_mmio: None,
    };

    let mut cfg = mock;
    let result = assign_pci_resources_inner(&mut cfg, &params).await.unwrap();

    // Collect all BAR regions and verify no overlaps.
    let mut regions: Vec<(u64, u64, String)> = Vec::new();
    for entry in &result.entries {
        for bar in &entry.bars {
            regions.push((
                bar.address,
                bar.address + bar.size,
                format!(
                    "{:02x}:{:02x}.{} BAR{}",
                    entry.bus, entry.device, entry.function, bar.index
                ),
            ));
        }
    }

    for i in 0..regions.len() {
        for j in (i + 1)..regions.len() {
            let (a_start, a_end, a_name) = &regions[i];
            let (b_start, b_end, b_name) = &regions[j];
            assert!(
                a_end <= b_start || b_end <= a_start,
                "BAR regions overlap: {a_name} [{a_start:#x}..{a_end:#x}) vs \
                 {b_name} [{b_start:#x}..{b_end:#x})"
            );
        }
    }
}

/// Bug A: When bus 255 is consumed (by a bridge secondary or SR-IOV VF
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
    // PF routing ID = (1 << 8) | 0 = 0x100
    // Last VF routing ID = 0x100 + 0x100 + 255*1 = 0x2FF → bus 2
    // But we'll craft it so max_vf_bus = 255:
    // PF on bus 1, devfn 0. vf_offset = 0x700, stride = 1, total_vfs = 1.
    // First VF RID = 0x100 + 0x700 = 0x800 → bus 8? No...
    // We want last VF on bus 255. RID of last VF = 0xFF00..0xFFFF → bus 255.
    // PF RID = (1 << 8) | 0 = 0x100.
    // last_vf_rid = 0x100 + vf_offset + (total_vfs - 1) * vf_stride = 0xFF00
    // With total_vfs=1, stride=1: last_vf_rid = 0x100 + vf_offset = 0xFF00
    // → vf_offset = 0xFE00
    mock.add_endpoint(1, 0, 0, &[(0, 0x1000, false, false)]);
    mock.add_sriov(1, 0, 0, 1, 0xFE00, 1);

    // Second bridge on bus 0 → needs secondary bus, but next_bus wrapped to 0.
    mock.add_bridge(0, 1, 0);
    // Empty device behind it (bus 256 which doesn't exist).
    // The bridge just needs to exist to trigger the next bus allocation.

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
    let result = assign_pci_resources_inner(&mut cfg, &params).await;

    // The SR-IOV reservation consumes up through bus 255. The next bridge
    // should fail with BusExhaustion, not silently wrap to bus 0.
    assert!(
        matches!(result, Err(crate::AssignmentError::BusExhaustion { .. })),
        "expected BusExhaustion when next_bus wraps past 255, got {result:?}"
    );
}
