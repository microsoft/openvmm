// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::memory::VtlAccess;
use hvdef::HvRegisterVsmPartitionConfig;
use inspect::Inspect;
use parking_lot::RwLock;
use range_map_vec::RangeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use virt::LateMapVtl0MemoryPolicy;

/// Tracking state if an intercept from VTL0 should be forwarded to VTL2.
#[derive(Debug)]
pub(crate) struct Vtl2InterceptState {
    /// Bitmap representing all possible io ports.
    io_ports: AtomicBitmap,
    /// MSR intercepts are all or nothing.
    pub msr: AtomicBool,
    /// Indicates if calls to unknown synic connections should trap to VTL2.
    pub unknown_synic_connection: AtomicBool,
    /// Indicates if calls to retarget interrupts for unknown device IDs should
    /// trap to VTL2.
    pub retarget_unknown_device_id: AtomicBool,
    /// EOI intercepts are all or nothing.
    pub eoi: AtomicBool,
}

impl Vtl2InterceptState {
    fn new() -> Self {
        Self {
            io_ports: AtomicBitmap::new(u16::MAX as usize + 1),
            msr: false.into(),
            unknown_synic_connection: false.into(),
            retarget_unknown_device_id: false.into(),
            eoi: false.into(),
        }
    }

    /// Resets to boot state.
    fn reset(&self) {
        let Self {
            io_ports,
            msr,
            unknown_synic_connection,
            retarget_unknown_device_id,
            eoi,
        } = self;
        for v in &io_ports.0 {
            v.store(0, Ordering::Relaxed);
        }
        msr.store(false, Ordering::Relaxed);
        unknown_synic_connection.store(false, Ordering::Relaxed);
        retarget_unknown_device_id.store(false, Ordering::Relaxed);
        eoi.store(false, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum InterceptType {
    IoPort(u16),
    Msr,
    UnknownSynicConnection,
    RetargetUnknownDeviceId,
    Eoi,
}

#[derive(Debug)]
struct AtomicBitmap(Vec<AtomicU64>);

impl AtomicBitmap {
    fn new(n: usize) -> Self {
        Self((0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect())
    }

    fn set(&self, n: usize) -> bool {
        self.0[n / 64].fetch_or(1 << (n % 64), Ordering::Relaxed) & (1 << (n % 64)) != 0
    }

    fn clear(&self, n: usize) -> bool {
        self.0[n / 64].fetch_and(!(1 << (n % 64)), Ordering::Relaxed) & (1 << (n % 64)) != 0
    }

    fn is_set(&self, n: usize) -> bool {
        self.0[n / 64].load(Ordering::Relaxed) & (1 << (n % 64)) != 0
    }
}

impl Vtl2InterceptState {
    /// Install the given intercept. Returns true if the intercept was not previously installed.
    pub fn install(&self, intercept: InterceptType) -> bool {
        match intercept {
            InterceptType::IoPort(port) => !self.io_ports.set(port.into()),
            InterceptType::Msr => !self.msr.swap(true, Ordering::SeqCst), // TODO: ordering req?
            InterceptType::UnknownSynicConnection => {
                !self.unknown_synic_connection.swap(true, Ordering::SeqCst)
            }
            InterceptType::RetargetUnknownDeviceId => {
                !self.retarget_unknown_device_id.swap(true, Ordering::SeqCst)
            }
            InterceptType::Eoi => !self.eoi.swap(true, Ordering::SeqCst),
        }
    }

    /// Remove the given intercept. Returns true if the intercept was previously installed.
    pub fn remove(&self, intercept: InterceptType) -> bool {
        match intercept {
            InterceptType::IoPort(port) => self.io_ports.clear(port.into()),
            InterceptType::Msr => self.msr.swap(false, Ordering::SeqCst), // TODO: ordering req?
            InterceptType::UnknownSynicConnection => {
                self.unknown_synic_connection.swap(false, Ordering::SeqCst)
            }
            InterceptType::RetargetUnknownDeviceId => self
                .retarget_unknown_device_id
                .swap(false, Ordering::SeqCst),
            InterceptType::Eoi => self.eoi.swap(false, Ordering::SeqCst),
        }
    }

    /// Check if the given intercept is installed. Returns true if the intercept is installed.
    pub fn contains(&self, intercept: InterceptType) -> bool {
        match intercept {
            InterceptType::IoPort(port) => self.io_ports.is_set(port.into()),
            InterceptType::Msr => self.msr.load(Ordering::SeqCst),
            InterceptType::UnknownSynicConnection => {
                self.unknown_synic_connection.load(Ordering::SeqCst)
            }
            InterceptType::RetargetUnknownDeviceId => {
                self.retarget_unknown_device_id.load(Ordering::SeqCst)
            }
            InterceptType::Eoi => self.eoi.load(Ordering::SeqCst),
        }
    }
}

impl Inspect for Vtl2InterceptState {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .child("io_ports", |req| {
                let mut resp = req.respond();
                let mut low_set_base = None;
                for i in 0..=0xffff {
                    // Print contiguous ranges that have intercepts installed.
                    if self.io_ports.is_set(i) {
                        if low_set_base.is_none() {
                            low_set_base = Some(i);
                        }
                    } else if let Some(base) = low_set_base {
                        resp.field(&format!("{:04x}-{:04x}", base, i - 1), true);
                        low_set_base = None;
                    }
                }

                // Handle the case where 0xffff is set.
                match low_set_base {
                    Some(base) if self.io_ports.is_set(0xffff) => {
                        resp.field(&format!("{:04x}-{:04x}", base, 0xffff), true);
                    }
                    _ => {}
                }
            })
            .field("msr", self.msr.load(Ordering::Relaxed))
            .field(
                "unknown_synic_connection",
                self.unknown_synic_connection.load(Ordering::Relaxed),
            )
            .field("eoi", self.eoi.load(Ordering::Relaxed));
    }
}

/// Supporting state to implement VTL2 support.
#[derive(Inspect)]
pub(crate) struct Vtl2Emulation {
    /// State tracking which intercepts from VTL0 should be instead forwarded to
    /// VTL2.
    pub intercepts: Vtl2InterceptState,
    /// Raw u64 value of the vsm_config register set by the guest.
    #[inspect(with = "inspect_helpers::vsm_config_raw")]
    pub vsm_config_raw: AtomicU64,
    /// Which pages are being protected by VTL2. Today, this only supports no
    /// access from lower VTLs.
    #[inspect(with = "inspect_helpers::protected_pages")]
    pub protected_pages: RwLock<RangeMap<u64, VtlAccess>>,
    /// Policy for accessing deferred VTL0 ram.
    #[inspect(debug)]
    pub vtl0_deferred_policy: LateMapVtl0MemoryPolicy,
}

mod inspect_helpers {
    use super::*;

    pub(super) fn vsm_config_raw(raw: &AtomicU64) -> impl Inspect + use<> {
        let config = HvRegisterVsmPartitionConfig::from(raw.load(Ordering::Relaxed));
        inspect::AsDebug(config)
    }

    pub(super) fn protected_pages(pages: &RwLock<RangeMap<u64, VtlAccess>>) -> impl Inspect + '_ {
        let pages = pages.read();
        inspect::AsDebug(pages)
    }
}

impl Vtl2Emulation {
    pub fn new(vtl0_deferred_policy: LateMapVtl0MemoryPolicy) -> Self {
        Self {
            intercepts: Vtl2InterceptState::new(),
            vsm_config_raw: Default::default(),
            protected_pages: Default::default(),
            vtl0_deferred_policy,
        }
    }

    /// Get the vsm config register for this partition as the strong typed value instead of the raw u64.
    pub fn vsm_config(&self) -> HvRegisterVsmPartitionConfig {
        HvRegisterVsmPartitionConfig::from(self.vsm_config_raw.load(Ordering::Relaxed))
    }

    /// Reset the VTL2 state.
    ///
    /// Note that this resets VTL page protection tracking state if requested,
    /// but the corresponding reset call must be made to VtlPartition to restore
    /// mapping state to the original partition start state.
    pub fn reset(&self, reset_vtl_protections: bool) {
        let Self {
            intercepts,
            vsm_config_raw,
            protected_pages,
            vtl0_deferred_policy: _,
        } = self;
        intercepts.reset();
        vsm_config_raw.store(0, Ordering::SeqCst);

        if reset_vtl_protections {
            *protected_pages.write() = RangeMap::new();
        }
    }
}
