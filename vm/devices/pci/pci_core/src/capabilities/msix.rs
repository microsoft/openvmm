// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSI-X Capability.

use super::PciCapability;
use crate::msi::MsiTarget;
use crate::spec::caps::CapabilityId;
use crate::spec::caps::msix::MsixCapabilityHeader;
use crate::spec::caps::msix::MsixTableEntryIdx;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use std::fmt::Debug;
use std::sync::Arc;
use vmcore::interrupt::Interrupt;
use vmcore::irqfd::IrqFd;
use vmcore::irqfd::IrqFdRoute;

#[derive(Debug, Inspect)]
struct MsiTableLocation {
    #[inspect(hex)]
    // MSI-X table offsets are, per spec, no larger than 32 bits.
    offset: u32,
    bar: u8,
}

impl MsiTableLocation {
    fn new(bar: u8, offset: u32) -> Self {
        assert!(bar < 6);
        assert!(offset & 7 == 0);
        Self { offset, bar }
    }

    fn read_u32(&self) -> u32 {
        self.offset | self.bar as u32
    }
}

#[derive(Inspect)]
struct MsixCapability {
    count: u16,
    #[inspect(with = "|x| inspect::adhoc(|req| x.lock().inspect_mut(req))")]
    state: Arc<Mutex<MsixState>>,
    config_table_location: MsiTableLocation,
    pending_bits_location: MsiTableLocation,
}

impl PciCapability for MsixCapability {
    fn label(&self) -> &str {
        "msi-x"
    }

    fn capability_id(&self) -> CapabilityId {
        CapabilityId::MSIX
    }

    fn len(&self) -> usize {
        12
    }

    fn read_u32(&self, offset: u16) -> u32 {
        match MsixCapabilityHeader(offset) {
            MsixCapabilityHeader::CONTROL_CAPS => {
                CapabilityId::MSIX.0 as u32
                    | ((self.count as u32 - 1) | if self.state.lock().enabled { 0x8000 } else { 0 })
                        << 16
            }
            MsixCapabilityHeader::OFFSET_TABLE => self.config_table_location.read_u32(),
            MsixCapabilityHeader::OFFSET_PBA => self.pending_bits_location.read_u32(),
            _ => panic!("Unreachable read offset {}", offset),
        }
    }

    fn write_u32(&mut self, offset: u16, val: u32) {
        match MsixCapabilityHeader(offset) {
            MsixCapabilityHeader::CONTROL_CAPS => {
                let enabled = val & 0x80000000 != 0;
                let mut state = self.state.lock();
                let was_enabled = state.enabled;
                state.enabled = enabled;
                if was_enabled && !enabled {
                    for entry in &mut state.vectors {
                        if entry.is_enabled(true) {
                            entry.msi.disable();
                        }
                    }
                } else if enabled && !was_enabled {
                    for entry in &mut state.vectors {
                        if entry.is_enabled(true) {
                            entry.msi.enable(
                                entry.state.address,
                                entry.state.data,
                                entry.state.is_pending,
                            );
                            entry.state.is_pending = false;
                        }
                    }
                }
            }
            MsixCapabilityHeader::OFFSET_TABLE | MsixCapabilityHeader::OFFSET_PBA => {
                tracelimit::warn_ratelimited!(
                    "Unexpected write offset {:?}",
                    MsixCapabilityHeader(offset)
                )
            }
            _ => panic!("Unreachable write offset {}", offset),
        }
    }

    fn reset(&mut self) {
        let mut state = self.state.lock();
        state.enabled = false;
        for vector in &mut state.vectors {
            vector.msi.disable();
            vector.state = EntryState::new();
        }
    }
}

#[derive(Clone, Inspect, Debug)]
pub(crate) struct MsiInterrupt(#[inspect(flatten)] Arc<Mutex<MsiInterruptInner>>);

#[derive(Inspect)]
struct MsiInterruptInner {
    target: MsiTarget,
    /// Optional kernel-mediated route for fast interrupt delivery.
    /// When present, `enable()`/`disable()` automatically program the
    /// kernel's MSI routing alongside the userspace `MsiTarget` path.
    #[inspect(skip)]
    route: Option<Box<dyn IrqFdRoute>>,
    pending: bool,
    enabled: bool,
    address: u64,
    data: u32,
}

impl Debug for MsiInterruptInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsiInterruptInner")
            .field("pending", &self.pending)
            .field("enabled", &self.enabled)
            .field("address", &self.address)
            .field("data", &self.data)
            .field("has_route", &self.route.is_some())
            .finish()
    }
}

impl MsiInterrupt {
    pub fn new(target: MsiTarget) -> Self {
        Self(Arc::new(Mutex::new(MsiInterruptInner {
            target,
            route: None,
            pending: false,
            enabled: false,
            address: 0,
            data: 0,
        })))
    }

    pub fn enable(&self, address: u64, data: u32, set_pending: bool) {
        let mut state = self.0.lock();
        state.pending |= set_pending;
        state.address = address;
        state.data = data;
        state.enabled = true;

        // Program the kernel route if present.
        if state.route.is_some() {
            let route = state.route.as_ref().unwrap();
            if route.consume_pending() {
                state.pending = true;
            }
        }
        if let Some(route) = &state.route {
            if address != 0 || data != 0 {
                if let Err(e) = route.set_msi(address, data) {
                    tracelimit::warn_ratelimited!(
                        error = ?e,
                        "failed to program MSI-X route on enable"
                    );
                }
            } else if let Err(e) = route.clear_msi() {
                tracelimit::warn_ratelimited!(
                    error = ?e,
                    "failed to clear MSI-X route on enable"
                );
            }
        }

        if state.pending {
            state.target.signal_msi(0, address, data);
            state.pending = false;
        }
    }

    pub fn disable(&self) {
        let mut state = self.0.lock();
        state.enabled = false;
        if let Some(route) = &state.route {
            if let Err(e) = route.mask() {
                tracelimit::warn_ratelimited!(
                    error = ?e,
                    "failed to mask MSI-X route on disable"
                );
            }
        }
    }

    pub fn drain_pending(&self) -> bool {
        let mut state = self.0.lock();
        if let Some(route) = &state.route {
            state.pending |= route.consume_pending();
        }
        let was_pending = state.pending;
        state.pending = false;
        was_pending
    }

    /// Install a kernel-mediated route for fast interrupt delivery.
    pub(crate) fn set_route(&self, route: Box<dyn IrqFdRoute>) {
        self.0.lock().route = Some(route);
    }

    /// Remove the kernel-mediated route.
    pub(crate) fn clear_route(&self) {
        self.0.lock().route = None;
    }

    pub fn interrupt(&self) -> Interrupt {
        let state = self.0.clone();
        Interrupt::from_fn(move || {
            let mut state = state.lock();
            if state.enabled {
                state.target.signal_msi(0, state.address, state.data);
            } else {
                state.pending = true;
            }
        })
    }
}

struct MsixMessageTableEntry {
    msi: MsiInterrupt,
    state: EntryState,
}

impl InspectMut for MsixMessageTableEntry {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .hex("address", self.state.address)
            .hex("data", self.state.data)
            .hex("control", self.state.control)
            .field("enabled", self.state.control & 1 == 0)
            .field("is_pending", self.check_is_pending(true));
    }
}

#[derive(Debug)]
struct EntryState {
    address: u64,
    data: u32,
    control: u32,
    is_pending: bool,
}

impl EntryState {
    fn new() -> Self {
        Self {
            address: 0,
            data: 0,
            control: 1,
            is_pending: false,
        }
    }
}

impl MsixMessageTableEntry {
    fn new(msi: MsiInterrupt) -> Self {
        Self {
            msi,
            state: EntryState::new(),
        }
    }

    fn read_u32(&self, offset: u64) -> u32 {
        match MsixTableEntryIdx(offset) {
            MsixTableEntryIdx::MSG_ADDR_LO => self.state.address as u32,
            MsixTableEntryIdx::MSG_ADDR_HI => (self.state.address >> 32) as u32,
            MsixTableEntryIdx::MSG_DATA => self.state.data,
            MsixTableEntryIdx::VECTOR_CTL => self.state.control,
            _ => panic!("Unexpected read offset {}", offset),
        }
    }

    fn write_u32(&mut self, offset: u64, val: u32) {
        match MsixTableEntryIdx(offset) {
            MsixTableEntryIdx::MSG_ADDR_LO => {
                self.state.address = (self.state.address & 0xffffffff00000000) | val as u64
            }
            MsixTableEntryIdx::MSG_ADDR_HI => {
                self.state.address = (val as u64) << 32 | self.state.address & 0xffffffff
            }
            MsixTableEntryIdx::MSG_DATA => self.state.data = val,
            MsixTableEntryIdx::VECTOR_CTL => self.state.control = val,
            _ => panic!("Unexpected write offset {}", offset),
        }
    }

    fn is_enabled(&self, global_enabled: bool) -> bool {
        global_enabled && self.state.control & 1 == 0
    }

    fn check_is_pending(&mut self, global_enabled: bool) -> bool {
        if !self.state.is_pending && !self.is_enabled(global_enabled) {
            self.state.is_pending = self.msi.drain_pending();
        }
        self.state.is_pending
    }
}

#[derive(InspectMut)]
struct MsixState {
    enabled: bool,
    #[inspect(mut, with = "inspect_entries")]
    vectors: Vec<MsixMessageTableEntry>,
}

fn inspect_entries(entries: &mut [MsixMessageTableEntry]) -> impl '_ + InspectMut {
    inspect::adhoc_mut(|req| {
        let mut resp = req.respond();
        for (i, entry) in entries.iter_mut().enumerate() {
            resp.field_mut(&i.to_string(), entry);
        }
    })
}

/// Emulator for the hardware-level interface required to configure and trigger
/// MSI-X interrupts on a PCI device.
#[derive(Clone)]
pub struct MsixEmulator {
    state: Arc<Mutex<MsixState>>,
    // PBA offsets, per spec, are no larger than 32 bits.
    pending_bits_offset: u32,
    pending_bits_dword_count: u16,
}

impl MsixEmulator {
    /// Create a new [`MsixEmulator`] instance, along with with its associated
    /// [`PciCapability`] structure.
    ///
    /// This implementation of MSI-X expects a dedicated BAR to store the vector
    /// and pending tables.
    ///
    /// * * *
    ///
    /// DEVNOTE: This current implementation of MSI-X isn't particularly
    /// "flexible" with respect to the various ways the PCI spec allows MSI-X to
    /// be implemented. e.g: it uses a shared BAR for the table and BPA, with
    /// fixed offsets into the BAR for both of those tables. It would be nice to
    /// re-visit this code and make it more flexible.
    pub fn new(bar: u8, count: u16, msi_target: &MsiTarget) -> (Self, impl PciCapability + use<>) {
        let state = MsixState {
            enabled: false,
            vectors: (0..count)
                .map(|_| MsixMessageTableEntry::new(MsiInterrupt::new(msi_target.clone())))
                .collect(),
        };
        let state = Arc::new(Mutex::new(state));
        let pending_bits_offset = count as u32 * 16;
        (
            Self {
                state: state.clone(),
                pending_bits_offset,
                pending_bits_dword_count: count.div_ceil(32),
            },
            MsixCapability {
                count,
                state,
                config_table_location: MsiTableLocation::new(bar, 0),
                pending_bits_location: MsiTableLocation::new(bar, pending_bits_offset),
            },
        )
    }

    /// Return the total length of the MSI-X BAR
    /// (Actually, the notion that there is an "MSI-X BAR" is an issue to fix sometime.
    /// MSI-X tables are often in the same bar as other things.)
    pub fn bar_len(&self) -> u64 {
        self.pending_bits_offset as u64 + self.pending_bits_dword_count as u64 * 4
    }

    /// Read a `u32` from the MSI-X BAR at the given offset.
    pub fn read_u32(&self, offset: u64) -> u32 {
        let mut state = self.state.lock();
        let state: &mut MsixState = &mut state;
        if offset < self.pending_bits_offset as u64 {
            let index = offset / 16;
            if let Some(entry) = state.vectors.get(index as usize) {
                return entry.read_u32(offset & 0xf);
            }
        } else {
            let dword = (offset - self.pending_bits_offset as u64) / 4;
            let start = dword as usize * 32;
            if start < state.vectors.len() {
                let end = (start + 32).min(state.vectors.len());
                let mut val = 0u32;
                for (i, entry) in state.vectors[start..end].iter_mut().enumerate() {
                    if entry.check_is_pending(state.enabled) {
                        val |= 1 << i;
                    }
                }
                return val;
            }
        }
        tracelimit::warn_ratelimited!(offset, "Unexpected read offset");
        0
    }

    /// Write a `u32` to the MSI-X BAR at the given offset.
    pub fn write_u32(&mut self, offset: u64, val: u32) {
        let mut state = self.state.lock();
        if offset < self.pending_bits_offset as u64 {
            let index = offset / 16;
            let global = state.enabled;
            if let Some(entry) = state.vectors.get_mut(index as usize) {
                let was_enabled = entry.is_enabled(global);
                entry.write_u32(offset & 0xf, val);
                let is_enabled = entry.is_enabled(global);
                if is_enabled && !was_enabled {
                    // Vector just unmasked.
                    entry.msi.enable(
                        entry.state.address,
                        entry.state.data,
                        entry.state.is_pending,
                    );
                    entry.state.is_pending = false;
                } else if was_enabled && !is_enabled {
                    // Vector just masked.
                    entry.msi.disable();
                } else if is_enabled {
                    // Still enabled. addr/data may have changed — enable()
                    // will update the route if present.
                    entry
                        .msi
                        .enable(entry.state.address, entry.state.data, false);
                }
                return;
            }
        } else if offset - (self.pending_bits_offset as u64)
            < self.pending_bits_dword_count as u64 * 4
        {
            return;
        }
        tracelimit::warn_ratelimited!(offset, "Unexpected write offset");
    }

    /// Return an [`Interrupt`] associated with the particular MSI-X vector, or
    /// `None` if the index is out of bounds.
    pub fn interrupt(&self, index: u16) -> Option<Interrupt> {
        Some(
            self.state
                .lock()
                .vectors
                .get_mut(index as usize)?
                .msi
                .interrupt(),
        )
    }

    #[cfg(test)]
    fn clear_pending_bit(&self, index: u8) {
        let mut state = self.state.lock();
        state.vectors[index as usize].state.is_pending = false;
    }

    /// Sets the pending bit for the given vector index.
    ///
    /// Used by device passthrough (e.g., VFIO with irqfd) to record that an
    /// interrupt arrived while the vector was masked, so PBA reads return
    /// the correct pending state.
    pub fn set_pending_bit(&self, index: u16) {
        let mut state = self.state.lock();
        if let Some(entry) = state.vectors.get_mut(index as usize) {
            entry.state.is_pending = true;
        } else {
            tracelimit::warn_ratelimited!(
                index,
                count = state.vectors.len(),
                "set_pending_bit: vector index out of range"
            );
        }
    }

    /// Enable kernel-mediated interrupt delivery for all vectors.
    ///
    /// Creates one irqfd route per vector using the provided [`IrqFd`]
    /// interface. The `register` callback receives references to the events
    /// for all vectors. The caller uses this to pass events to the
    /// interrupt source (e.g., VFIO `map_msix` or vhost-user
    /// `SET_VRING_CALL`). After the callback returns, the routes are
    /// installed in the emulator. When the guest programs MSI-X table
    /// entries, the emulator automatically updates the kernel's MSI routing.
    ///
    /// Call [`disable_irqfd`](Self::disable_irqfd) to tear down the routes.
    pub fn enable_irqfd(
        &self,
        irqfd: &dyn IrqFd,
        register: impl FnOnce(&[&pal_event::Event]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let state = self.state.lock();
        let mut routes: Vec<Box<dyn IrqFdRoute>> = Vec::with_capacity(state.vectors.len());
        for _ in state.vectors.iter() {
            routes.push(irqfd.new_irqfd_route()?);
        }

        // Collect event references while routes are still in our local Vec.
        let events: Vec<&pal_event::Event> = routes.iter().map(|r| r.event()).collect();
        register(&events)?;

        // Move routes into the emulator's MsiInterrupts.
        for (entry, route) in state.vectors.iter().zip(routes) {
            entry.msi.set_route(route);
        }
        Ok(())
    }

    /// Tear down kernel-mediated interrupt delivery.
    ///
    /// Drops all irqfd routes, unregistering them from the hypervisor and
    /// freeing GSI allocations.
    pub fn disable_irqfd(&self) {
        let state = self.state.lock();
        for entry in &state.vectors {
            entry.msi.clear_route();
        }
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
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Debug, Protobuf)]
        #[mesh(package = "pci.caps.msix")]
        pub struct SavedMsixMessageTableEntryState {
            #[mesh(1)]
            pub address: u64,
            #[mesh(2)]
            pub data: u32,
            #[mesh(3)]
            pub control: u32,
            #[mesh(4)]
            pub is_pending: bool,
        }

        #[derive(Debug, Protobuf, SavedStateRoot)]
        #[mesh(package = "pci.caps.msix")]
        pub struct SavedState {
            #[mesh(2)]
            pub enabled: bool,
            #[mesh(3)]
            pub vectors: Vec<SavedMsixMessageTableEntryState>,
        }
    }

    #[derive(Debug, Error)]
    enum MsixRestoreError {
        #[error("mismatched vector lengths: current:{0}, saved:{1}")]
        MismatchedTableLengths(usize, usize),
    }

    impl SaveRestore for MsixCapability {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let state = self.state.lock();
            let saved_state = state::SavedState {
                enabled: state.enabled,
                vectors: {
                    state
                        .vectors
                        .iter()
                        .map(|vec| {
                            let EntryState {
                                address,
                                data,
                                control,
                                is_pending,
                            } = vec.state;

                            state::SavedMsixMessageTableEntryState {
                                address,
                                data,
                                control,
                                is_pending,
                            }
                        })
                        .collect()
                },
            };
            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { enabled, vectors } = state;

            let mut state = self.state.lock();
            state.enabled = enabled;

            if vectors.len() != state.vectors.len() {
                return Err(RestoreError::InvalidSavedState(
                    MsixRestoreError::MismatchedTableLengths(vectors.len(), state.vectors.len())
                        .into(),
                ));
            }

            for (new_vec, vec) in vectors.into_iter().zip(state.vectors.iter_mut()) {
                vec.state = EntryState {
                    address: new_vec.address,
                    data: new_vec.data,
                    control: new_vec.control,
                    is_pending: new_vec.is_pending,
                }
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{msi::MsiConnection, test_helpers::TestPciInterruptController};

    #[test]
    fn msix_check() {
        let msi_conn = MsiConnection::new();
        let (mut msix, mut cap) = MsixEmulator::new(2, 64, msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());
        // check capabilities
        assert_eq!(cap.read_u32(0), 0x3f0011);
        assert_eq!(cap.read_u32(4), 2);
        assert_eq!(cap.read_u32(8), 0x402);
        cap.write_u32(0, 0xffffffff);
        assert_eq!(cap.read_u32(0), 0x803f0011);
        // check BAR
        // Vector[0]
        assert_eq!(msix.read_u32(0), 0);
        assert_eq!(msix.read_u32(4), 0);
        assert_eq!(msix.read_u32(8), 0);
        assert_eq!(msix.read_u32(12), 1);
        msix.write_u32(0, 0x12345678);
        msix.write_u32(4, 0x9abcdef0);
        msix.write_u32(8, 0x123);
        msix.write_u32(12, 0x456);
        assert_eq!(msix.read_u32(0), 0x12345678);
        assert_eq!(msix.read_u32(4), 0x9abcdef0);
        assert_eq!(msix.read_u32(8), 0x123);
        assert_eq!(msix.read_u32(12), 0x456);
        // Vector[63]
        assert_eq!(msix.read_u32(0x3f0), 0);
        assert_eq!(msix.read_u32(0x3f4), 0);
        assert_eq!(msix.read_u32(0x3f8), 0);
        assert_eq!(msix.read_u32(0x3fc), 1);
        msix.write_u32(0x3f0, 0x12345678);
        msix.write_u32(0x3f4, 0x9abcdef0);
        msix.write_u32(0x3f8, 0x123);
        msix.write_u32(0x3fc, 0x456);
        assert_eq!(msix.read_u32(0x3f0), 0x12345678);
        assert_eq!(msix.read_u32(0x3f4), 0x9abcdef0);
        assert_eq!(msix.read_u32(0x3f8), 0x123);
        assert_eq!(msix.read_u32(0x3fc), 0x456);
        // Pending Bit Array
        assert_eq!(msix.read_u32(0x400), 0);
        assert_eq!(msix.read_u32(0x404), 0);
        msix.set_pending_bit(1);
        assert_eq!(msix.read_u32(0x400), 2);
        assert_eq!(msix.read_u32(0x404), 0);
        msix.set_pending_bit(33);
        assert_eq!(msix.read_u32(0x400), 2);
        assert_eq!(msix.read_u32(0x404), 2);
        msix.set_pending_bit(63);
        msix.set_pending_bit(31);
        assert_eq!(msix.read_u32(0x400), 0x80000002);
        assert_eq!(msix.read_u32(0x404), 0x80000002);
        msix.clear_pending_bit(1);
        assert_eq!(msix.read_u32(0x400), 0x80000000);
        assert_eq!(msix.read_u32(0x404), 0x80000002);
    }

    use pal_event::Event;
    use parking_lot::Mutex;

    /// Record of a call made to a [`MockIrqFdRoute`].
    #[derive(Debug, Clone, PartialEq)]
    enum RouteCall {
        SetMsi { address: u64, data: u32 },
        ClearMsi,
        Mask,
        ConsumePending,
    }

    /// Mock irqfd route that records calls.
    struct MockIrqFdRoute {
        event: Event,
        calls: Arc<Mutex<Vec<RouteCall>>>,
        pending: Arc<Mutex<bool>>,
    }

    impl IrqFdRoute for MockIrqFdRoute {
        fn event(&self) -> &Event {
            &self.event
        }

        fn set_msi(&self, address: u64, data: u32) -> anyhow::Result<()> {
            self.calls.lock().push(RouteCall::SetMsi { address, data });
            Ok(())
        }

        fn clear_msi(&self) -> anyhow::Result<()> {
            self.calls.lock().push(RouteCall::ClearMsi);
            Ok(())
        }

        fn mask(&self) -> anyhow::Result<()> {
            self.calls.lock().push(RouteCall::Mask);
            Ok(())
        }

        fn unmask(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn consume_pending(&self) -> bool {
            self.calls.lock().push(RouteCall::ConsumePending);
            let mut p = self.pending.lock();
            let was = *p;
            *p = false;
            was
        }
    }

    /// Mock IrqFd that creates MockIrqFdRoutes and returns shared call logs.
    struct MockIrqFd {
        routes: Mutex<Vec<(Arc<Mutex<Vec<RouteCall>>>, Arc<Mutex<bool>>)>>,
    }

    impl MockIrqFd {
        fn new(count: usize) -> (Self, Vec<Arc<Mutex<Vec<RouteCall>>>>, Vec<Arc<Mutex<bool>>>) {
            let mut call_logs = Vec::new();
            let mut pendings = Vec::new();
            let mut route_params = Vec::new();
            for _ in 0..count {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let pending = Arc::new(Mutex::new(false));
                call_logs.push(calls.clone());
                pendings.push(pending.clone());
                route_params.push((calls, pending));
            }
            (
                Self {
                    routes: Mutex::new(route_params),
                },
                call_logs,
                pendings,
            )
        }
    }

    impl IrqFd for MockIrqFd {
        fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn IrqFdRoute>> {
            let (calls, pending) = self.routes.lock().remove(0);
            Ok(Box::new(MockIrqFdRoute {
                event: Event::new(),
                calls,
                pending,
            }))
        }
    }

    #[test]
    fn route_set_msi_on_unmask() {
        let msi_conn = MsiConnection::new();
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        let (mock_irqfd, calls, _pendings) = MockIrqFd::new(2);
        msix.enable_irqfd(&mock_irqfd, |_| Ok(())).unwrap();

        // Enable MSI-X globally.
        cap.write_u32(0, 0x80000000);

        // Program vector 0 addr/data (still masked — control starts at 1).
        msix.write_u32(0, 0xFEE00000); // addr_lo
        msix.write_u32(4, 0); // addr_hi
        msix.write_u32(8, 0x42); // data

        // No set_msi yet because vector is still masked.
        assert!(
            !calls[0]
                .lock()
                .iter()
                .any(|c| matches!(c, RouteCall::SetMsi { .. }))
        );

        // Unmask vector 0 (write control = 0).
        calls[0].lock().clear();
        msix.write_u32(12, 0);

        // Should have called consume_pending then set_msi.
        let log = calls[0].lock().clone();
        assert!(log.contains(&RouteCall::ConsumePending));
        assert!(log.contains(&RouteCall::SetMsi {
            address: 0xFEE00000,
            data: 0x42
        }));
    }

    #[test]
    fn route_mask_on_vector_mask() {
        let msi_conn = MsiConnection::new();
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        let (mock_irqfd, calls, _pendings) = MockIrqFd::new(2);
        msix.enable_irqfd(&mock_irqfd, |_| Ok(())).unwrap();

        // Enable MSI-X, program and unmask vector 0.
        cap.write_u32(0, 0x80000000);
        msix.write_u32(0, 0xFEE00000);
        msix.write_u32(8, 0x42);
        msix.write_u32(12, 0); // unmask

        calls[0].lock().clear();

        // Mask vector 0 (write control = 1).
        msix.write_u32(12, 1);

        let log = calls[0].lock().clone();
        assert!(log.contains(&RouteCall::Mask));
    }

    #[test]
    fn route_global_disable_masks_all() {
        let msi_conn = MsiConnection::new();
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        let (mock_irqfd, calls, _pendings) = MockIrqFd::new(2);
        msix.enable_irqfd(&mock_irqfd, |_| Ok(())).unwrap();

        // Enable, program, and unmask both vectors.
        cap.write_u32(0, 0x80000000);
        for v in 0..2u64 {
            msix.write_u32(v * 16, 0xFEE00000);
            msix.write_u32(v * 16 + 8, (v + 1) as u32);
            msix.write_u32(v * 16 + 12, 0); // unmask
        }
        calls[0].lock().clear();
        calls[1].lock().clear();

        // Disable MSI-X globally.
        cap.write_u32(0, 0);

        // Both vectors should have been masked.
        assert!(calls[0].lock().contains(&RouteCall::Mask));
        assert!(calls[1].lock().contains(&RouteCall::Mask));
    }

    #[test]
    fn route_consume_pending_on_pba_read() {
        let msi_conn = MsiConnection::new();
        let (msix, mut cap) = MsixEmulator::new(2, 2, msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        let (mock_irqfd, calls, pendings) = MockIrqFd::new(2);
        msix.enable_irqfd(&mock_irqfd, |_| Ok(())).unwrap();

        // Enable MSI-X but leave vectors masked (control = 1 by default).
        cap.write_u32(0, 0x80000000);

        // Simulate a pending interrupt on vector 0.
        *pendings[0].lock() = true;
        calls[0].lock().clear();

        // PBA is at offset = vector_count * 16 = 32.
        let pba = msix.read_u32(32);

        // Should have called consume_pending and returned bit 0 set.
        assert!(calls[0].lock().contains(&RouteCall::ConsumePending));
        assert_eq!(pba & 1, 1);
    }

    #[test]
    fn route_set_msi_on_addr_data_change_while_unmasked() {
        let msi_conn = MsiConnection::new();
        let (mut msix, mut cap) = MsixEmulator::new(2, 1, msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        let (mock_irqfd, calls, _pendings) = MockIrqFd::new(1);
        msix.enable_irqfd(&mock_irqfd, |_| Ok(())).unwrap();

        // Enable, program, unmask.
        cap.write_u32(0, 0x80000000);
        msix.write_u32(0, 0xFEE00000);
        msix.write_u32(8, 0x42);
        msix.write_u32(12, 0);
        calls[0].lock().clear();

        // Change data while still unmasked.
        msix.write_u32(8, 0x99);

        let log = calls[0].lock().clone();
        assert!(log.contains(&RouteCall::SetMsi {
            address: 0xFEE00000,
            data: 0x99
        }));
    }
}
