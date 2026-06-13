// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe Single Root I/O Virtualization (SR-IOV) extended capability.

use super::PciExtendedCapability;
use crate::cfg_space_emu::BarMemoryKind;
use crate::spec::caps::ExtendedCapabilityId;
use crate::spec::caps::sriov::SRIOV_CAP_LEN;
use crate::spec::caps::sriov::SriovCapabilities;
use crate::spec::caps::sriov::SriovControl;
use crate::spec::caps::sriov::SriovExtendedCapabilityHeader;
use crate::spec::caps::sriov::SriovStatus;
use crate::spec::cfg_space::BarEncodingBits;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;

/// Configuration for a single VF BAR.
#[derive(Debug, Clone, Copy, Inspect)]
pub struct VfBarConfig {
    /// Size of the BAR in bytes. Must be a power of 2 and >= 16.
    /// Set to 0 to indicate the BAR is not implemented.
    #[inspect(hex)]
    pub size: u64,
    /// Whether the BAR is 64-bit (consumes two consecutive BAR slots).
    pub is_64bit: bool,
    /// Whether the BAR is prefetchable.
    pub prefetchable: bool,
}

/// Configuration parameters for the SR-IOV extended capability.
#[derive(Debug, Clone, Inspect)]
pub struct SriovConfig {
    /// Total number of VFs the PF can support (1..=255, or up to 7 without ARI).
    pub total_vfs: u16,
    /// PCI Device ID to report for all VFs.
    #[inspect(hex)]
    pub vf_device_id: u16,
    /// Function offset from the PF to the first VF.
    pub first_vf_offset: u16,
    /// Stride between consecutive VFs.
    pub vf_stride: u16,
    /// VF BAR configurations (up to 6).
    #[inspect(skip)]
    pub vf_bars: [Option<VfBarConfig>; 6],
}

/// PCIe SR-IOV extended capability emulator.
///
/// Implements the SR-IOV Extended Capability structure per PCIe Base
/// Specification Section 9.3.3. This is a generic, device-agnostic
/// implementation that can be attached to any PF's extended capability list.
#[derive(Inspect)]
pub struct SriovExtendedCapability {
    // -- Configuration (read-only after construction) --
    capabilities: SriovCapabilities,
    #[inspect(hex)]
    initial_vfs: u16,
    #[inspect(hex)]
    total_vfs: u16,
    #[inspect(hex)]
    vf_device_id: u16,
    #[inspect(hex)]
    first_vf_offset: u16,
    #[inspect(hex)]
    vf_stride: u16,
    /// Supported page sizes — bit N means page size 2^(N+12) is supported.
    #[inspect(hex)]
    supported_page_sizes: u32,

    // -- VF BAR configuration --
    /// Per-BAR size (0 = not implemented). Sizes are per-VF.
    #[inspect(iter_by_index)]
    vf_bar_sizes: [u64; 6],
    /// Per-BAR in-band encoding bits (64-bit / prefetchable), as stored in
    /// the low bits of each VF BAR register.
    #[inspect(with = "|x| inspect::iter_by_index(x.iter().map(|b| b.into_bits()))")]
    vf_bar_encoding: [BarEncodingBits; 6],

    // -- Mutable state (guest-writable) --
    control: SriovControl,
    status: SriovStatus,
    num_vfs: u16,
    /// System page size — bit N means page size 2^(N+12).
    system_page_size: u32,
    /// Current BAR register values (including probe state).
    #[inspect(with = r#"|x| inspect::iter_by_index(x).prefix("vf_bar")"#)]
    vf_bar_regs: [u32; 6],

    // -- VF MMIO management --
    #[inspect(skip)]
    vf_mmio: SriovVfMmio,
}

/// VF MMIO intercept handles and decode state managed by the SR-IOV
/// capability, mirroring how PF BARs are managed by
/// `ConfigSpaceType0Emulator`.
struct SriovVfMmio {
    /// One MMIO region per BAR, covering all VFs contiguously.
    /// `bars[bar_index]` spans `total_vfs * per_vf_size` bytes starting at
    /// VF0's base address. `None` for unimplemented BARs.
    bars: [Option<BarMemoryKind>; 6],
    /// Shared decode state — the device reads this for MMIO routing.
    decode: Arc<SriovBarDecode>,
}

/// Shared VF BAR decode state for MMIO routing.
///
/// The SR-IOV capability updates this when BAR addresses, MSE, or
/// VF_Enable change. The device reads it on every MMIO access to
/// route the access to the correct VF.
pub struct SriovBarDecode {
    inner: Mutex<SriovBarDecodeInner>,
}

struct SriovBarDecodeInner {
    bars: [SriovBarDecodeEntry; 6],
    /// Pending VF_Enable change. Written by the SR-IOV capability,
    /// consumed by the device after each config write.
    pending_vf_change: Option<SriovVfChange>,
}

/// A pending VF_Enable state change.
#[derive(Clone, Copy, Debug)]
pub struct SriovVfChange {
    /// Whether VFs are being enabled or disabled.
    pub enabled: bool,
    /// The NumVFs value at the time of the transition.
    pub num_vfs: u16,
}

#[derive(Clone, Copy, Default)]
struct SriovBarDecodeEntry {
    /// Decoded base address for VF 0 (None when VF MSE is off,
    /// VF_Enable is off, or BAR is not programmed).
    vf0_base: Option<u64>,
    /// log2(per_vf_bar_size) for fast address math.
    shift: u32,
}

impl SriovBarDecode {
    fn new(vf_bar_sizes: &[u64; 6]) -> Self {
        let mut bars = [SriovBarDecodeEntry::default(); 6];
        for (i, &size) in vf_bar_sizes.iter().enumerate() {
            if size > 0 {
                bars[i].shift = size.trailing_zeros();
            }
        }
        SriovBarDecode {
            inner: Mutex::new(SriovBarDecodeInner {
                bars,
                pending_vf_change: None,
            }),
        }
    }

    /// Try to decode an address as a VF BAR access.
    ///
    /// Returns `(vf_index, offset_within_bar)` if the address falls
    /// within a VF BAR region for the given `bar_index`.
    pub fn decode(&self, bar_index: usize, addr: u64) -> Option<(usize, u64)> {
        let inner = self.inner.lock();
        let entry = inner.bars.get(bar_index)?;
        let base = entry.vf0_base?;
        if addr < base {
            return None;
        }
        let rel = addr - base;
        let vf_idx = (rel >> entry.shift) as usize;
        let offset = rel & ((1u64 << entry.shift) - 1);
        Some((vf_idx, offset))
    }

    /// Take any pending VF_Enable change.
    ///
    /// The device calls this after each PCI config write to check if
    /// VF_Enable transitioned and VFs need to be created or destroyed.
    pub fn take_pending_vf_change(&self) -> Option<SriovVfChange> {
        self.inner.lock().pending_vf_change.take()
    }
}

impl SriovExtendedCapability {
    /// Creates a new SR-IOV extended capability.
    ///
    /// `config` specifies the PF's SR-IOV parameters. `vf_bars` provides
    /// one MMIO intercept region per BAR, each spanning all VFs
    /// contiguously (`total_vfs * per_vf_size` bytes) — the capability
    /// manages VF BAR mapping directly (like PF BARs). Returns the shared
    /// BAR decode state for MMIO routing.
    pub fn new(
        config: SriovConfig,
        vf_bars: [Option<BarMemoryKind>; 6],
    ) -> (Self, Arc<SriovBarDecode>) {
        assert!(config.total_vfs > 0, "total_vfs must be > 0");
        assert!(
            config.first_vf_offset > 0,
            "first_vf_offset must be > 0 (0 would collide with PF)"
        );

        let mut vf_bar_sizes = [0u64; 6];
        let mut vf_bar_encoding = [BarEncodingBits::new(); 6];

        let mut i = 0;
        while i < 6 {
            if let Some(bar) = &config.vf_bars[i] {
                assert!(
                    bar.size > 0 && bar.size.is_power_of_two() && bar.size >= 16,
                    "VF BAR{i} size must be a power of 2 and >= 16"
                );
                vf_bar_sizes[i] = bar.size;
                vf_bar_encoding[i] = BarEncodingBits::new()
                    .with_type_64_bit(bar.is_64bit)
                    .with_prefetchable(bar.prefetchable);

                if bar.is_64bit {
                    // 64-bit BAR consumes next slot too.
                    assert!(i + 1 < 6, "64-bit VF BAR{i} would overflow BAR slots");
                    assert!(
                        config.vf_bars[i + 1].is_none(),
                        "VF BAR{} must be None (consumed by 64-bit BAR{i})",
                        i + 1
                    );
                    i += 1; // skip next slot
                }
            }
            i += 1;
        }

        // Build initial bar register values from the encoding bits.
        let mut vf_bar_regs = [0u32; 6];
        {
            let mut idx = 0;
            while idx < 6 {
                if vf_bar_sizes[idx] > 0 {
                    vf_bar_regs[idx] = vf_bar_encoding[idx].into_bits();
                    if vf_bar_encoding[idx].type_64_bit() {
                        idx += 1; // skip upper 32-bit slot (stays 0)
                    }
                }
                idx += 1;
            }
        }

        // Default supported page size: 4K (bit 0).
        let supported_page_sizes = 1;
        let system_page_size = 1; // 4K default

        let decode = Arc::new(SriovBarDecode::new(&vf_bar_sizes));
        let vf_mmio = SriovVfMmio {
            bars: vf_bars,
            decode: decode.clone(),
        };

        (
            Self {
                capabilities: SriovCapabilities::new(),
                initial_vfs: config.total_vfs,
                total_vfs: config.total_vfs,
                vf_device_id: config.vf_device_id,
                first_vf_offset: config.first_vf_offset,
                vf_stride: config.vf_stride,
                supported_page_sizes,
                vf_bar_sizes,
                vf_bar_encoding,
                control: SriovControl::new(),
                status: SriovStatus::new(),
                num_vfs: 0,
                system_page_size,
                vf_bar_regs,
                vf_mmio,
            },
            decode,
        )
    }

    /// Returns the current NumVFs value.
    pub fn num_vfs(&self) -> u16 {
        self.num_vfs
    }

    /// Returns whether VF Enable is currently set.
    pub fn vf_enabled(&self) -> bool {
        self.control.vf_enable()
    }

    /// Computes the guest physical address for a given VF's BAR.
    ///
    /// `vf_index` is 0-based (VF 0 is the first VF). `bar_index` is the
    /// BAR slot (0..6). Returns `None` if the BAR is not implemented or if
    /// the address bits in the VF BAR register are all zero (not yet
    /// programmed by the guest).
    ///
    /// Per PCIe spec, the VF BAR in the SR-IOV capability defines the base
    /// address for VF 0's BAR, and each subsequent VF's BAR is at
    /// `base + vf_index * bar_size`.
    pub fn vf_bar_address(&self, vf_index: u16, bar_index: usize) -> Option<u64> {
        if bar_index >= 6 {
            return None;
        }
        let size = self.vf_bar_sizes[bar_index];
        if size == 0 {
            return None;
        }

        // Reconstruct the 64-bit base address from the VF BAR register(s).
        let is_64bit = self.vf_bar_encoding[bar_index].type_64_bit();
        let base_lo = self.vf_bar_regs[bar_index] & !0xF; // mask off type bits
        let base_hi = if is_64bit && bar_index + 1 < 6 {
            self.vf_bar_regs[bar_index + 1]
        } else {
            0
        };
        let base = (base_hi as u64) << 32 | base_lo as u64;
        if base == 0 {
            return None; // Not yet programmed.
        }

        // Each VF's BAR address = base + vf_index * bar_size.
        Some(base + vf_index as u64 * size)
    }

    /// Returns the per-VF size for the given BAR index, or 0 if not
    /// implemented.
    pub fn vf_bar_size(&self, bar_index: usize) -> u64 {
        if bar_index < 6 {
            self.vf_bar_sizes[bar_index]
        } else {
            0
        }
    }

    /// Read a VF BAR register, handling size probing.
    fn read_vf_bar(&self, bar_index: usize) -> u32 {
        self.vf_bar_regs[bar_index]
    }

    /// Write a VF BAR register, handling size probing (write all-1s, read back size mask).
    fn write_vf_bar(&mut self, bar_index: usize, val: u32) {
        if bar_index >= 6 || self.vf_bar_sizes[bar_index] == 0 {
            // Check if this is the upper half of a 64-bit BAR.
            if bar_index > 0 && bar_index < 6 && self.vf_bar_encoding[bar_index - 1].type_64_bit() {
                // Upper 32 bits of a 64-bit BAR.
                let size = self.vf_bar_sizes[bar_index - 1];
                let size_mask_hi = !((size - 1) >> 32) as u32;
                self.vf_bar_regs[bar_index] = val & size_mask_hi;
                return;
            }
            return;
        }

        let size = self.vf_bar_sizes[bar_index];

        // Low bits (type field) are read-only. Build the writable mask.
        let size_mask = !(size as u32 - 1);
        // The writable portion is the address bits; type bits are preserved.
        self.vf_bar_regs[bar_index] =
            (val & size_mask) | self.vf_bar_encoding[bar_index].into_bits();
    }

    /// Synchronize VF MMIO intercepts and the shared decode state with the
    /// current register values. Called after any write that can affect VF
    /// MMIO mapping (CONTROL, VF BAR registers).
    fn sync_vf_mmio(&mut self) {
        let vf_mmio = &mut self.vf_mmio;
        let vf_enable = self.control.vf_enable();
        let vf_mse = self.control.vf_mse();
        let mut decode = vf_mmio.decode.inner.lock();

        for bar_idx in 0..6 {
            let bar_size = self.vf_bar_sizes[bar_idx];
            if bar_size == 0 {
                continue;
            }

            // Compute VF0 base address inline (can't call self.vf_bar_address
            // while self.vf_mmio is mutably borrowed).
            let vf0_base = if vf_enable && vf_mse {
                let is_64bit = self.vf_bar_encoding[bar_idx].type_64_bit();
                let base_lo = self.vf_bar_regs[bar_idx] & !0xF;
                let base_hi = if is_64bit && bar_idx + 1 < 6 {
                    self.vf_bar_regs[bar_idx + 1]
                } else {
                    0
                };
                let base = (base_hi as u64) << 32 | base_lo as u64;
                if base != 0 { Some(base) } else { None }
            } else {
                None
            };
            decode.bars[bar_idx].vf0_base = vf0_base;

            // The BAR's single MMIO region covers all VFs contiguously,
            // starting at VF0's base. Map or unmap it as a whole.
            if let Some(mem) = &mut vf_mmio.bars[bar_idx] {
                mem.unmap_from_guest();
                if let Some(addr) = vf0_base {
                    if let Err(err) = mem.map_to_guest(addr) {
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            bar_idx,
                            addr,
                            "failed to map VF BAR",
                        );
                    }
                }
            }
        }
    }
}

impl PciExtendedCapability for SriovExtendedCapability {
    fn label(&self) -> &str {
        "sriov"
    }

    fn extended_capability_id(&self) -> u16 {
        ExtendedCapabilityId::SRIOV.0
    }

    fn capability_version(&self) -> u8 {
        1
    }

    fn len(&self) -> usize {
        SRIOV_CAP_LEN
    }

    fn read_u32(&self, offset: u16) -> u32 {
        match SriovExtendedCapabilityHeader(offset) {
            SriovExtendedCapabilityHeader::HEADER => {
                u32::from(self.extended_capability_id())
                    | (u32::from(self.capability_version()) << 16)
            }
            SriovExtendedCapabilityHeader::CAPABILITIES => self.capabilities.into_bits(),
            SriovExtendedCapabilityHeader::CONTROL_STATUS => {
                self.control.into_bits() as u32 | ((self.status.into_bits() as u32) << 16)
            }
            SriovExtendedCapabilityHeader::INITIAL_TOTAL_VFS => {
                self.initial_vfs as u32 | ((self.total_vfs as u32) << 16)
            }
            SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK => {
                // NumVFs in low 16, Function Dependency Link in high 16.
                // Dependency link = 0 (all VFs depend on function 0).
                self.num_vfs as u32
            }
            SriovExtendedCapabilityHeader::VF_OFFSET_STRIDE => {
                self.first_vf_offset as u32 | ((self.vf_stride as u32) << 16)
            }
            SriovExtendedCapabilityHeader::VF_DEVICE_ID => {
                // Low 16 = reserved (0), high 16 = VF Device ID.
                (self.vf_device_id as u32) << 16
            }
            SriovExtendedCapabilityHeader::SUPPORTED_PAGE_SIZE => self.supported_page_sizes,
            SriovExtendedCapabilityHeader::SYSTEM_PAGE_SIZE => self.system_page_size,
            SriovExtendedCapabilityHeader::VF_BAR0 => self.read_vf_bar(0),
            SriovExtendedCapabilityHeader::VF_BAR1 => self.read_vf_bar(1),
            SriovExtendedCapabilityHeader::VF_BAR2 => self.read_vf_bar(2),
            SriovExtendedCapabilityHeader::VF_BAR3 => self.read_vf_bar(3),
            SriovExtendedCapabilityHeader::VF_BAR4 => self.read_vf_bar(4),
            SriovExtendedCapabilityHeader::VF_BAR5 => self.read_vf_bar(5),
            SriovExtendedCapabilityHeader::RESERVED_PADDING => 0,
            _ => {
                tracelimit::warn_ratelimited!(offset, "unexpected SR-IOV extended capability read");
                0
            }
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        match SriovExtendedCapabilityHeader(offset) {
            SriovExtendedCapabilityHeader::HEADER
            | SriovExtendedCapabilityHeader::CAPABILITIES
            | SriovExtendedCapabilityHeader::INITIAL_TOTAL_VFS
            | SriovExtendedCapabilityHeader::VF_OFFSET_STRIDE
            | SriovExtendedCapabilityHeader::VF_DEVICE_ID
            | SriovExtendedCapabilityHeader::SUPPORTED_PAGE_SIZE => {
                tracelimit::warn_ratelimited!(
                    offset,
                    value = val,
                    "write to read-only SR-IOV extended capability register"
                );
            }
            SriovExtendedCapabilityHeader::CONTROL_STATUS => {
                let new_control = SriovControl::from_bits(val as u16);
                let old_vf_enable = self.control.vf_enable();
                let new_vf_enable = new_control.vf_enable();

                // Migration bits are deprecated, force to 0.
                self.control = new_control
                    .with_vf_migration_enable(false)
                    .with_vf_migration_interrupt_enable(false);

                if old_vf_enable != new_vf_enable {
                    self.vf_mmio.decode.inner.lock().pending_vf_change = Some(SriovVfChange {
                        enabled: new_vf_enable,
                        num_vfs: self.num_vfs,
                    });
                }

                // Status (upper 16) is read-only / RW1C — writes to
                // vf_migration_status clear it.
                if SriovStatus::from_bits((val >> 16) as u16).vf_migration_status() {
                    self.status = self.status.with_vf_migration_status(false);
                }

                self.sync_vf_mmio();
            }
            SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK => {
                // NumVFs is writable only when VF Enable is 0 per spec.
                if !self.control.vf_enable() {
                    let requested = val as u16;
                    self.num_vfs = requested.min(self.total_vfs);
                } else {
                    tracelimit::warn_ratelimited!(
                        value = val,
                        "write to NumVFs while VF Enable is set; ignoring"
                    );
                }
            }
            SriovExtendedCapabilityHeader::SYSTEM_PAGE_SIZE => {
                // Only writable when VF Enable is 0.
                if !self.control.vf_enable() {
                    // Must be a single bit set and within supported range.
                    let masked = val & self.supported_page_sizes;
                    if masked.count_ones() == 1 {
                        self.system_page_size = masked;
                    } else {
                        tracelimit::warn_ratelimited!(
                            value = val,
                            supported = self.supported_page_sizes,
                            "invalid System Page Size write; must set exactly one supported bit"
                        );
                    }
                } else {
                    tracelimit::warn_ratelimited!(
                        value = val,
                        "write to System Page Size while VF Enable is set; ignoring"
                    );
                }
            }
            SriovExtendedCapabilityHeader::VF_BAR0 => {
                self.write_vf_bar(0, val);
                self.sync_vf_mmio();
            }
            SriovExtendedCapabilityHeader::VF_BAR1 => {
                self.write_vf_bar(1, val);
                self.sync_vf_mmio();
            }
            SriovExtendedCapabilityHeader::VF_BAR2 => {
                self.write_vf_bar(2, val);
                self.sync_vf_mmio();
            }
            SriovExtendedCapabilityHeader::VF_BAR3 => {
                self.write_vf_bar(3, val);
                self.sync_vf_mmio();
            }
            SriovExtendedCapabilityHeader::VF_BAR4 => {
                self.write_vf_bar(4, val);
                self.sync_vf_mmio();
            }
            SriovExtendedCapabilityHeader::VF_BAR5 => {
                self.write_vf_bar(5, val);
                self.sync_vf_mmio();
            }
            SriovExtendedCapabilityHeader::RESERVED_PADDING => {}
            _ => {
                tracelimit::warn_ratelimited!(
                    offset,
                    value = val,
                    "unexpected SR-IOV extended capability write"
                );
            }
        }
    }

    fn reset(&mut self) {
        self.control = SriovControl::new();
        self.status = SriovStatus::new();
        self.num_vfs = 0;
        self.system_page_size = 1; // 4K default

        // Reset VF BAR registers to their type bits only.
        for i in 0..6 {
            if self.vf_bar_sizes[i] > 0 {
                self.vf_bar_regs[i] = self.vf_bar_encoding[i].into_bits();
            } else {
                self.vf_bar_regs[i] = 0;
            }
        }

        // Don't queue a pending VF change — the device handles its own
        // VF cleanup during reset. Just unmap the intercepts.
        self.sync_vf_mmio();
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Debug, Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.capabilities.extended.sriov")]
        pub struct SavedState {
            #[mesh(1)]
            pub control: u16,
            #[mesh(2)]
            pub status: u16,
            #[mesh(3)]
            pub num_vfs: u16,
            #[mesh(4)]
            pub system_page_size: u32,
            #[mesh(5)]
            pub vf_bar_regs: Vec<u32>,
        }
    }

    impl SaveRestore for SriovExtendedCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                control: self.control.into_bits(),
                status: self.status.into_bits(),
                num_vfs: self.num_vfs,
                system_page_size: self.system_page_size,
                vf_bar_regs: self.vf_bar_regs.to_vec(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            self.control = SriovControl::from_bits(state.control);
            self.status = SriovStatus::from_bits(state.status);
            self.num_vfs = state.num_vfs.min(self.total_vfs);

            // Validate system_page_size: must be exactly one supported bit.
            let masked_page_size = state.system_page_size & self.supported_page_sizes;
            if masked_page_size.count_ones() == 1 {
                self.system_page_size = masked_page_size;
            } else {
                self.system_page_size = 1; // Default to 4K.
            }

            if state.vf_bar_regs.len() != 6 {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "expected 6 VF BAR registers, got {}",
                    state.vf_bar_regs.len()
                )));
            }

            // Apply the same masking as write_vf_bar to preserve type-bit
            // invariants and enforce size alignment.
            for i in 0..6 {
                let val = state.vf_bar_regs[i];
                if self.vf_bar_sizes[i] > 0 {
                    let size = self.vf_bar_sizes[i];
                    let size_mask = !(size as u32 - 1);
                    self.vf_bar_regs[i] = (val & size_mask) | self.vf_bar_encoding[i].into_bits();
                } else if i > 0 && self.vf_bar_encoding[i - 1].type_64_bit() {
                    // Upper 32 bits of a 64-bit BAR.
                    let size = self.vf_bar_sizes[i - 1];
                    let size_mask_hi = !((size - 1) >> 32) as u32;
                    self.vf_bar_regs[i] = val & size_mask_hi;
                } else {
                    self.vf_bar_regs[i] = 0;
                }
            }

            // Signal pending VF change for restored state.
            if self.control.vf_enable() {
                self.vf_mmio.decode.inner.lock().pending_vf_change = Some(SriovVfChange {
                    enabled: true,
                    num_vfs: self.num_vfs,
                });
            }

            // Sync VF MMIO intercepts for the restored BAR/MSE state.
            self.sync_vf_mmio();

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::extended::assert_extended_header_contract;
    use vmcore::save_restore::SaveRestore;

    fn test_config() -> SriovConfig {
        let mut vf_bars = [None; 6];
        vf_bars[0] = Some(VfBarConfig {
            size: 16384, // 16 KiB
            is_64bit: true,
            prefetchable: true,
        });
        // BAR 1 consumed by 64-bit BAR 0.
        SriovConfig {
            total_vfs: 4,
            vf_device_id: 0x1234,
            first_vf_offset: 1,
            vf_stride: 1,
            vf_bars,
        }
    }

    fn new_test_cap(config: SriovConfig) -> (SriovExtendedCapability, Arc<SriovBarDecode>) {
        let vf_bars: [Option<BarMemoryKind>; 6] = std::array::from_fn(|bar_idx| {
            if config.vf_bars[bar_idx].is_some() {
                Some(BarMemoryKind::Dummy)
            } else {
                None
            }
        });
        SriovExtendedCapability::new(config, vf_bars)
    }

    #[test]
    fn test_sriov_defaults() {
        let (cap, _decode) = new_test_cap(test_config());

        assert_eq!(cap.label(), "sriov");
        assert_eq!(cap.extended_capability_id(), ExtendedCapabilityId::SRIOV.0);
        assert_eq!(cap.capability_version(), 1);
        assert_eq!(cap.len(), SRIOV_CAP_LEN);
        assert_extended_header_contract(&cap);
    }

    #[test]
    fn test_sriov_read_initial_total_vfs() {
        let (cap, _decode) = new_test_cap(test_config());
        let val = cap.read_u32(SriovExtendedCapabilityHeader::INITIAL_TOTAL_VFS.0);
        assert_eq!(val as u16, 4); // InitialVFs
        assert_eq!((val >> 16) as u16, 4); // TotalVFs
    }

    #[test]
    fn test_sriov_read_vf_device_id() {
        let (cap, _decode) = new_test_cap(test_config());
        let val = cap.read_u32(SriovExtendedCapabilityHeader::VF_DEVICE_ID.0);
        assert_eq!((val >> 16) as u16, 0x1234);
    }

    #[test]
    fn test_sriov_read_offset_stride() {
        let (cap, _decode) = new_test_cap(test_config());
        let val = cap.read_u32(SriovExtendedCapabilityHeader::VF_OFFSET_STRIDE.0);
        assert_eq!(val as u16, 1); // first_vf_offset
        assert_eq!((val >> 16) as u16, 1); // vf_stride
    }

    #[test]
    fn test_sriov_numvfs_write() {
        let (mut cap, _decode) = new_test_cap(test_config());

        // Write NumVFs = 3.
        cap.write_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0, 3);
        let val = cap.read_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0);
        assert_eq!(val as u16, 3);
    }

    #[test]
    fn test_sriov_numvfs_clamped_to_total() {
        let (mut cap, _decode) = new_test_cap(test_config());

        // Write NumVFs = 100 (exceeds total_vfs=4).
        cap.write_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0, 100);
        let val = cap.read_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0);
        assert_eq!(val as u16, 4); // Clamped to total_vfs.
    }

    #[test]
    fn test_sriov_numvfs_readonly_when_enabled() {
        let (mut cap, _decode) = new_test_cap(test_config());

        // Set NumVFs first.
        cap.write_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0, 2);
        // Enable VFs.
        cap.write_u32(
            SriovExtendedCapabilityHeader::CONTROL_STATUS.0,
            SriovControl::new().with_vf_enable(true).into_bits() as u32,
        );
        // Try to change NumVFs — should be ignored.
        cap.write_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0, 4);
        let val = cap.read_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0);
        assert_eq!(val as u16, 2); // Unchanged.
    }

    #[test]
    fn test_sriov_vf_enable_pending() {
        let (mut cap, decode) = new_test_cap(test_config());

        cap.write_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0, 2);
        cap.write_u32(
            SriovExtendedCapabilityHeader::CONTROL_STATUS.0,
            SriovControl::new().with_vf_enable(true).into_bits() as u32,
        );
        let change = decode
            .take_pending_vf_change()
            .expect("should have pending change");
        assert!(change.enabled);
        assert_eq!(change.num_vfs, 2);

        cap.write_u32(SriovExtendedCapabilityHeader::CONTROL_STATUS.0, 0);
        let change = decode
            .take_pending_vf_change()
            .expect("should have pending change");
        assert!(!change.enabled);
    }

    #[test]
    fn test_sriov_vf_bar_probe() {
        let (mut cap, _decode) = new_test_cap(test_config());

        // Write all 1s to VF BAR0 to probe size.
        cap.write_vf_bar(0, 0xFFFF_FFFF);
        let val = cap.read_vf_bar(0);
        // BAR0 is 16 KiB (0x4000), so size mask is 0xFFFF_C000.
        // Plus type bits: 64-bit (0b100) | prefetchable (0b1000) = 0xC.
        assert_eq!(val, 0xFFFF_C00C);
    }

    #[test]
    fn test_sriov_system_page_size() {
        let (mut cap, _decode) = new_test_cap(test_config());

        // Default is 4K (bit 0).
        let val = cap.read_u32(SriovExtendedCapabilityHeader::SYSTEM_PAGE_SIZE.0);
        assert_eq!(val, 1);

        // Can't set unsupported page size.
        cap.write_u32(SriovExtendedCapabilityHeader::SYSTEM_PAGE_SIZE.0, 0x4);
        let val = cap.read_u32(SriovExtendedCapabilityHeader::SYSTEM_PAGE_SIZE.0);
        assert_eq!(val, 1); // Unchanged — 0x4 not in supported_page_sizes.

        // Can't set multiple bits.
        cap.write_u32(SriovExtendedCapabilityHeader::SYSTEM_PAGE_SIZE.0, 0x3);
        let val = cap.read_u32(SriovExtendedCapabilityHeader::SYSTEM_PAGE_SIZE.0);
        assert_eq!(val, 1); // Unchanged.
    }

    #[test]
    fn test_sriov_reset_clears_state() {
        let (mut cap, _decode) = new_test_cap(test_config());

        cap.write_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0, 3);
        cap.write_u32(
            SriovExtendedCapabilityHeader::CONTROL_STATUS.0,
            SriovControl::new().with_vf_enable(true).into_bits() as u32,
        );

        cap.reset();

        let ctl = cap.read_u32(SriovExtendedCapabilityHeader::CONTROL_STATUS.0);
        assert_eq!(ctl, 0);
        let num = cap.read_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0);
        assert_eq!(num as u16, 0);
    }

    #[test]
    fn test_sriov_save_restore() {
        let (mut cap, _decode) = new_test_cap(test_config());

        cap.write_u32(SriovExtendedCapabilityHeader::NUM_VFS_DEP_LINK.0, 2);
        cap.write_u32(
            SriovExtendedCapabilityHeader::CONTROL_STATUS.0,
            SriovControl::new()
                .with_vf_enable(true)
                .with_vf_mse(true)
                .into_bits() as u32,
        );

        let saved = cap.save().expect("save should succeed");

        cap.reset();
        assert_eq!(
            cap.read_u32(SriovExtendedCapabilityHeader::CONTROL_STATUS.0),
            0
        );

        cap.restore(saved).expect("restore should succeed");
        let ctl = cap.read_u32(SriovExtendedCapabilityHeader::CONTROL_STATUS.0);
        let control = SriovControl::from_bits(ctl as u16);
        assert!(control.vf_enable());
        assert!(control.vf_mse());
        assert_eq!(cap.num_vfs(), 2);
    }
}
