// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MSI-X Capability.

use super::PciCapability;
use crate::msi::MsiRoute;
use crate::msi::MsiTarget;
use crate::spec::caps::CapabilityId;
use crate::spec::caps::msix::MsixCapabilityHeader;
use crate::spec::caps::msix::MsixTableEntryIdx;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use inspect::Inspect;
use inspect::InspectMut;
use pal_event::Event;
use parking_lot::Mutex;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use vmcore::interrupt::EventProxyWait;
use vmcore::interrupt::Interrupt;
use vmcore::interrupt::InterruptTarget;

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

    fn read(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        match MsixCapabilityHeader(offset) {
            MsixCapabilityHeader::CONTROL_CAPS => {
                value.set_low_high(
                    CapabilityId::MSIX.0.into(),
                    (self.count - 1) | if self.state.lock().enabled { 0x8000 } else { 0 },
                );
            }
            MsixCapabilityHeader::OFFSET_TABLE => value.set(self.config_table_location.read_u32()),
            MsixCapabilityHeader::OFFSET_PBA => value.set(self.pending_bits_location.read_u32()),
            _ => panic!("Unreachable read offset {}", offset),
        }
    }

    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite) {
        match MsixCapabilityHeader(offset) {
            MsixCapabilityHeader::CONTROL_CAPS => {
                const MSIX_ENABLE_BIT_MASK: u32 = 0x8000_0000;
                if val.valid_mask() & MSIX_ENABLE_BIT_MASK != 0 {
                    let mut state = self.state.lock();
                    let was_enabled = state.enabled;
                    let new_enabled = (val.extract() & MSIX_ENABLE_BIT_MASK) != 0;

                    state.enabled = new_enabled;
                    if was_enabled && !new_enabled {
                        for entry in &mut state.vectors {
                            if entry.is_enabled(true) {
                                entry.update_interrupt(false);
                            }
                        }
                    } else if new_enabled && !was_enabled {
                        for entry in &mut state.vectors {
                            if entry.is_enabled(true) {
                                entry.state.is_pending |= entry.drain_pending();
                                entry.update_interrupt(true);
                            }
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
            vector.update_interrupt(false);
            vector.drain_pending();
            vector.state = EntryState::new();
        }
    }
}

#[derive(Clone, Inspect, Debug)]
pub(crate) struct MsiInterrupt(#[inspect(flatten)] Arc<Mutex<MsiInterruptInner>>);

#[derive(Inspect)]
struct MsiInterruptInner {
    #[inspect(skip)]
    target: MsiTarget,
    /// Optional kernel-mediated route for direct interrupt delivery.
    /// When present, `enable()`/`disable()` automatically program the
    /// kernel's MSI routing alongside the userspace `MsiTarget` path.
    #[inspect(skip)]
    route: Option<MsiRoute>,
    pending: bool,
    enabled: bool,
    #[inspect(hex)]
    address: u64,
    #[inspect(hex)]
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

impl MsiInterruptInner {
    fn deliver(&mut self) {
        if self.enabled {
            self.signal_msi();
        } else {
            self.pending = true;
        }
    }

    fn signal_msi(&self) {
        self.target.signal_msi(self.address, self.data);
    }

    fn enable_route(&self, route: &MsiRoute) {
        route.enable(self.address, self.data);
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
        if let Some(route) = &state.route {
            state.enable_route(route);
        }

        if state.pending {
            state.signal_msi();
            state.pending = false;
        }
    }

    pub fn disable(&self) {
        let mut state = self.0.lock();
        state.enabled = false;
        if let Some(route) = &state.route {
            route.disable();
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

    pub fn interrupt(&self) -> Interrupt {
        Interrupt::from_target(MsiInterruptTarget(self.0.clone()))
    }

    fn deliver(&self) {
        self.0.lock().deliver();
    }
}

/// [`InterruptTarget`] for MSI-X vectors that lazily allocates an irqfd
/// route when [`event()`](InterruptTarget::event) is first called.
struct MsiInterruptTarget(Arc<Mutex<MsiInterruptInner>>);

impl InterruptTarget for MsiInterruptTarget {
    fn deliver(&self) {
        self.0.lock().deliver();
    }

    fn event(&self) -> Option<Arc<Event>> {
        let mut state = self.0.lock();
        if let Some(route) = &state.route {
            return Some(Arc::new(route.event().clone()));
        }
        let route = match state.target.new_route() {
            Some(Ok(route)) => route,
            Some(Err(e)) => {
                tracelimit::warn_ratelimited!(error = ?e, "failed to allocate MSI route");
                return None;
            }
            None => return None,
        };
        if state.enabled {
            state.enable_route(&route);
        } else {
            route.disable();
        }
        let event = Arc::new(route.event().clone());
        state.route = Some(route);
        Some(event)
    }
}

/// An MSI-X interrupt source with a stable event and a changeable vector mapping.
#[derive(Clone)]
pub struct MsixInterrupt {
    state: Arc<Mutex<MsixState>>,
    source: Arc<Mutex<MsixInterruptSource>>,
    target: MsiTarget,
}

struct MsixInterruptSource {
    vector: Option<u16>,
    route: Option<MsiRoute>,
    proxies: Vec<Weak<EventProxyWait>>,
    #[expect(clippy::option_option)] // Some(None) requests unmapping.
    next_vector: Option<Option<u16>>,
}

impl MsixInterruptSource {
    fn deliver(&self, state: &MsixState) {
        if let Some(index) = self.vector {
            state.vectors[usize::from(index)].msi.deliver();
        }
    }
}

impl MsixInterrupt {
    /// Maps this source to a vector, or unmaps it with `None`.
    ///
    /// An invalid vector unmaps the source and returns `false`.
    /// Finish any outstanding proxy cancellation with [`Self::poll_mapping`].
    pub fn set_vector(&self, vector: Option<u16>) -> bool {
        let state = self.state.lock();
        let mut source = self.source.lock();
        let valid = vector.is_none_or(|index| usize::from(index) < state.vectors.len());
        let vector = vector.filter(|_| valid);
        if source.vector == vector && source.next_vector.is_none() {
            return valid;
        }

        let pending = source.next_vector.replace(vector).is_some();
        drop(source);
        drop(state);
        // Do not replace the task waker of an existing cancellation.
        if !pending {
            let _ = self.poll_mapping(&mut Context::from_waker(Waker::noop()));
        }
        valid
    }

    /// Finishes cancelling proxy waits before changing the route.
    pub fn poll_mapping(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.lock();
        let mut source = self.source.lock();
        let Some(vector) = source.next_vector else {
            return Poll::Ready(());
        };
        let proxies: Vec<_> = source.proxies.iter().filter_map(Weak::upgrade).collect();
        for proxy in &proxies {
            if std::task::ready!(proxy.poll_cancel_wait(cx)) {
                source.deliver(&state);
            }
        }
        if let Some(route) = &source.route {
            route.disable();
            if route.consume_pending() {
                source.deliver(&state);
            }
        }
        if let Some(index) = source.vector {
            let weak = Arc::downgrade(&self.source);
            state.vectors[usize::from(index)]
                .sources
                .retain(|source| !source.ptr_eq(&weak) && source.strong_count() != 0);
        }
        source.vector = vector;
        source.next_vector = None;
        if let Some(index) = vector {
            let enabled = state.enabled;
            let entry = &mut state.vectors[usize::from(index)];
            entry.sources.push(Arc::downgrade(&self.source));
            if let Some(route) = &source.route {
                if entry.is_enabled(enabled) {
                    route.enable(entry.state.address, entry.state.data);
                }
            }
        }
        drop(source);
        drop(state);
        for proxy in proxies {
            proxy.wake();
        }
        Poll::Ready(())
    }

    /// Returns whether mapping is waiting for proxy cancellation.
    pub fn mapping_pending(&self) -> bool {
        self.source.lock().next_vector.is_some()
    }

    /// Returns an interrupt that follows this source through mapping changes.
    pub fn interrupt(&self) -> Interrupt {
        Interrupt::from_target(self.clone())
    }
}

impl InterruptTarget for MsixInterrupt {
    fn deliver(&self) {
        let state = self.state.lock();
        let source = self.source.lock();
        source.deliver(&state);
    }

    fn event(&self) -> Option<Arc<Event>> {
        let state = self.state.lock();
        let mut source = self.source.lock();
        if let Some(route) = &source.route {
            return Some(Arc::new(route.event().clone()));
        }
        let route = match self.target.new_route() {
            Some(Ok(route)) => route,
            Some(Err(error)) => {
                tracelimit::warn_ratelimited!(?error, "failed to allocate MSI-X source route");
                return None;
            }
            None => return None,
        };
        route.disable();
        if let Some(index) = source.vector {
            let entry = &state.vectors[usize::from(index)];
            if entry.is_enabled(state.enabled) {
                route.enable(entry.state.address, entry.state.data);
            }
        }
        let event = Arc::new(route.event().clone());
        source.route = Some(route);
        Some(event)
    }

    fn register_event_proxy(&self, wait: Weak<EventProxyWait>) {
        let mut source = self.source.lock();
        source.proxies.retain(|p| p.strong_count() != 0);
        source.proxies.push(wait);
    }

    fn poll_event_proxy(&self, cx: &mut Context<'_>, wait: &EventProxyWait) -> Poll<()> {
        let state = self.state.lock();
        let source = self.source.lock();
        if source.next_vector.is_some() {
            return Poll::Pending;
        }
        std::task::ready!(wait.poll_wait(cx));
        source.deliver(&state);
        Poll::Ready(())
    }
}

struct MsixMessageTableEntry {
    msi: MsiInterrupt,
    state: EntryState,
    sources: Vec<Weak<Mutex<MsixInterruptSource>>>,
}

impl InspectMut for MsixMessageTableEntry {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .hex("address", self.state.address)
            .hex("data", self.state.data)
            .hex("control", self.state.control)
            .field("enabled", self.state.control & 1 == 0)
            .field("is_pending", self.check_is_pending(true))
            // Delivery state, which can diverge from the table above (e.g. an
            // interrupt latched while masked).
            .field("msi", &self.msi);
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
            sources: Vec::new(),
        }
    }

    fn update_interrupt(&mut self, global_enabled: bool) {
        let enabled = self.is_enabled(global_enabled);
        if enabled {
            self.msi
                .enable(self.state.address, self.state.data, self.state.is_pending);
            self.state.is_pending = false;
        } else {
            self.msi.disable();
        }
        self.sources.retain(|source| {
            let Some(source) = source.upgrade() else {
                return false;
            };
            if let Some(route) = &source.lock().route {
                if enabled {
                    route.enable(self.state.address, self.state.data);
                } else {
                    route.disable();
                }
            }
            true
        });
    }

    fn drain_pending(&mut self) -> bool {
        let mut pending = self.msi.drain_pending();
        self.sources.retain(|source| {
            let Some(source) = source.upgrade() else {
                return false;
            };
            if let Some(route) = &source.lock().route {
                pending |= route.consume_pending();
            }
            true
        });
        pending
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
            self.state.is_pending = self.drain_pending();
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
    target: MsiTarget,
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
                target: msi_target.clone(),
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
                    entry.state.is_pending |= entry.drain_pending();
                }
                if was_enabled || is_enabled {
                    entry.update_interrupt(global);
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

    /// Creates an unmapped interrupt source with independently routed event delivery.
    pub fn new_interrupt(&self) -> MsixInterrupt {
        MsixInterrupt {
            state: self.state.clone(),
            source: Arc::new(Mutex::new(MsixInterruptSource {
                vector: None,
                route: None,
                proxies: Vec::new(),
                next_vector: None,
            })),
            target: self.target.clone(),
        }
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
            let mut state = self.state.lock();
            let enabled = state.enabled;
            let saved_state = state::SavedState {
                enabled: state.enabled,
                vectors: {
                    state
                        .vectors
                        .iter_mut()
                        .map(|vec| {
                            vec.check_is_pending(enabled);
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
                vec.update_interrupt(false);
                vec.drain_pending();
                vec.state = EntryState {
                    address: new_vec.address,
                    data: new_vec.data,
                    control: new_vec.control,
                    is_pending: new_vec.is_pending,
                };
                vec.update_interrupt(enabled);
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msi::MsiConnection;
    use crate::test_helpers::TestPciInterruptController;
    use crate::test_helpers::read_cap_u32;
    use crate::test_helpers::write_cap_u32;
    use test_with_tracing::test;

    #[test]
    fn msix_check() {
        let msi_conn = MsiConnection::new();
        let (mut msix, mut cap) = MsixEmulator::new(2, 64, &msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());
        // check capabilities
        assert_eq!(read_cap_u32(&cap, 0), 0x3f0011);
        assert_eq!(read_cap_u32(&cap, 4), 2);
        assert_eq!(read_cap_u32(&cap, 8), 0x402);
        write_cap_u32(&mut cap, 0, 0xffffffff);
        assert_eq!(read_cap_u32(&cap, 0), 0x803f0011);
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

    /// Record of a call made to a mock route.
    #[derive(Debug, Clone, PartialEq)]
    enum RouteCall {
        SetMsi { address: u64, data: u32 },
        ClearMsi,
    }

    /// Mock IrqFdRoute implementation that records calls.
    struct MockIrqFdRoute {
        event: Event,
        calls: Arc<Mutex<Vec<RouteCall>>>,
    }

    impl vmcore::irqfd::IrqFdRoute for MockIrqFdRoute {
        fn event(&self) -> &Event {
            &self.event
        }

        fn enable(&self, address: u64, data: u32, _devid: Option<u32>) {
            self.calls.lock().push(RouteCall::SetMsi { address, data });
        }

        fn disable(&self) {
            self.calls.lock().push(RouteCall::ClearMsi);
        }
    }

    /// Build a mock IrqFd and shared call logs.
    fn mock_irqfd(
        count: usize,
    ) -> (
        Arc<dyn vmcore::irqfd::IrqFd>,
        Vec<Arc<Mutex<Vec<RouteCall>>>>,
    ) {
        let mut call_logs = Vec::new();
        let route_params = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..count {
            let calls = Arc::new(Mutex::new(Vec::new()));
            call_logs.push(calls.clone());
            route_params.lock().push(calls);
        }

        struct MockIrqFd {
            routes: Mutex<Vec<Arc<Mutex<Vec<RouteCall>>>>>,
        }
        impl vmcore::irqfd::IrqFd for MockIrqFd {
            fn new_irqfd_route(&self) -> anyhow::Result<Box<dyn vmcore::irqfd::IrqFdRoute>> {
                let calls = self.routes.lock().remove(0);
                Ok(Box::new(MockIrqFdRoute {
                    event: Event::new(),
                    calls,
                }))
            }
        }

        (
            Arc::new(MockIrqFd {
                routes: Mutex::new(call_logs.clone()),
            }),
            call_logs,
        )
    }

    #[test]
    fn route_set_msi_on_unmask() {
        let (irqfd, calls) = mock_irqfd(2);
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, &msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        // Trigger lazy irqfd route creation for all vectors.
        for i in 0..2 {
            msix.interrupt(i).unwrap().event();
        }

        // Enable MSI-X globally.
        write_cap_u32(&mut cap, 0, 0x80000000);

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

        // Should have called set_msi to re-establish the route.
        let log = calls[0].lock().clone();
        assert!(log.contains(&RouteCall::SetMsi {
            address: 0xFEE00000,
            data: 0x42
        }));
    }

    #[test]
    fn route_mask_on_vector_mask() {
        let (irqfd, calls) = mock_irqfd(2);
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, &msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        // Trigger lazy irqfd route creation for all vectors.
        for i in 0..2 {
            msix.interrupt(i).unwrap().event();
        }

        // Enable MSI-X, program and unmask vector 0.
        write_cap_u32(&mut cap, 0, 0x80000000);
        msix.write_u32(0, 0xFEE00000);
        msix.write_u32(8, 0x42);
        msix.write_u32(12, 0); // unmask

        calls[0].lock().clear();

        // Mask vector 0 (write control = 1).
        msix.write_u32(12, 1);

        let log = calls[0].lock().clone();
        assert!(log.contains(&RouteCall::ClearMsi));
    }

    #[test]
    fn route_global_disable_masks_all() {
        let (irqfd, calls) = mock_irqfd(2);
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, &msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        // Trigger lazy irqfd route creation for all vectors.
        for i in 0..2 {
            msix.interrupt(i).unwrap().event();
        }

        // Enable, program, and unmask both vectors.
        write_cap_u32(&mut cap, 0, 0x80000000);
        for v in 0..2u64 {
            msix.write_u32(v * 16, 0xFEE00000);
            msix.write_u32(v * 16 + 8, (v + 1) as u32);
            msix.write_u32(v * 16 + 12, 0); // unmask
        }
        calls[0].lock().clear();
        calls[1].lock().clear();

        // Disable MSI-X globally.
        write_cap_u32(&mut cap, 0, 0);

        // Both vectors should have been disabled.
        assert!(calls[0].lock().contains(&RouteCall::ClearMsi));
        assert!(calls[1].lock().contains(&RouteCall::ClearMsi));
    }

    #[test]
    fn route_consume_pending_on_pba_read() {
        let (irqfd, _calls) = mock_irqfd(2);
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);
        let (msix, mut cap) = MsixEmulator::new(2, 2, &msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        // Trigger lazy irqfd route creation for all vectors.
        let events: Vec<_> = (0..2)
            .map(|i| msix.interrupt(i).unwrap().event().unwrap().clone())
            .collect();

        // Enable MSI-X but leave vectors masked (control = 1 by default).
        write_cap_u32(&mut cap, 0, 0x80000000);

        // Simulate a pending interrupt on vector 0 by signaling the event.
        events[0].signal();

        // PBA is at offset = vector_count * 16 = 32.
        let pba = msix.read_u32(32);

        // consume_pending drains the event; PBA bit 0 should be set.
        assert_eq!(pba & 1, 1);
    }

    #[test]
    fn route_set_msi_on_addr_data_change_while_unmasked() {
        let (irqfd, calls) = mock_irqfd(1);
        let msi_conn = MsiConnection::new();
        msi_conn.connect_irqfd(irqfd);
        let (mut msix, mut cap) = MsixEmulator::new(2, 1, &msi_conn.target());
        let msi_controller = TestPciInterruptController::new();
        msi_conn.connect(msi_controller.signal_msi());

        // Trigger lazy irqfd route creation.
        msix.interrupt(0).unwrap().event();

        // Enable, program, unmask.
        write_cap_u32(&mut cap, 0, 0x80000000);
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

    #[test]
    fn mapped_source_route_follows_vector() {
        let (irqfd, calls) = mock_irqfd(1);
        let conn = MsiConnection::new();
        conn.connect_irqfd(irqfd);
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, &conn.target());
        let source = msix.new_interrupt();
        let interrupt = source.interrupt();
        let event = interrupt.event().unwrap().clone();
        assert_eq!(*calls[0].lock(), [RouteCall::ClearMsi]);

        for index in 0..2 {
            msix.write_u32(index * 16, 0xfee00000);
            msix.write_u32(index * 16 + 8, 0x40 + index as u32);
            msix.write_u32(index * 16 + 12, 0);
        }
        write_cap_u32(&mut cap, 0, 0x80000000);

        event.signal();
        assert!(source.set_vector(Some(0)));
        assert!(!event.try_wait());
        assert_eq!(
            calls[0].lock().last(),
            Some(&RouteCall::SetMsi {
                address: 0xfee00000,
                data: 0x40,
            })
        );
        assert!(source.set_vector(Some(1)));
        assert_eq!(
            calls[0].lock().last(),
            Some(&RouteCall::SetMsi {
                address: 0xfee00000,
                data: 0x41,
            })
        );

        calls[0].lock().clear();
        msix.write_u32(8, 0x99);
        assert!(calls[0].lock().is_empty());
        msix.write_u32(24, 0x42);
        assert_eq!(
            calls[0].lock().last(),
            Some(&RouteCall::SetMsi {
                address: 0xfee00000,
                data: 0x42,
            })
        );
        msix.write_u32(28, 1);
        assert_eq!(calls[0].lock().last(), Some(&RouteCall::ClearMsi));
        msix.write_u32(28, 0);
        assert!(matches!(
            calls[0].lock().last(),
            Some(RouteCall::SetMsi { data: 0x42, .. })
        ));
        write_cap_u32(&mut cap, 0, 0);
        assert_eq!(calls[0].lock().last(), Some(&RouteCall::ClearMsi));
        write_cap_u32(&mut cap, 0, 0x80000000);
        assert!(matches!(
            calls[0].lock().last(),
            Some(RouteCall::SetMsi { data: 0x42, .. })
        ));

        event.signal();
        assert!(source.interrupt().event().unwrap().try_wait());
        assert!(source.set_vector(None));
        assert_eq!(calls[0].lock().last(), Some(&RouteCall::ClearMsi));
    }

    #[test]
    fn mapped_source_pending_stays_with_old_vector() {
        let (irqfd, _) = mock_irqfd(1);
        let conn = MsiConnection::new();
        conn.connect_irqfd(irqfd);
        let (msix, mut cap) = MsixEmulator::new(2, 2, &conn.target());
        write_cap_u32(&mut cap, 0, 0x80000000);
        let source = msix.new_interrupt();
        let event = source.interrupt().event().unwrap().clone();

        assert!(source.set_vector(Some(0)));
        event.signal();
        assert!(source.set_vector(Some(1)));
        assert_eq!(msix.read_u32(32), 1);
        event.signal();
        assert!(source.set_vector(None));
        assert_eq!(msix.read_u32(32), 3);

        msix.clear_pending_bit(0);
        msix.clear_pending_bit(1);
        event.signal();
        source.interrupt().deliver();
        assert_eq!(msix.read_u32(32), 0);
        assert!(source.set_vector(Some(0)));
        assert!(!event.try_wait());
        assert_eq!(msix.read_u32(32), 0);

        assert!(!source.set_vector(Some(2)));
        event.signal();
        source.interrupt().deliver();
        assert_eq!(msix.read_u32(32), 0);
        assert!(source.set_vector(Some(1)));
        assert!(!event.try_wait());
    }

    #[test]
    fn mapped_sources_share_vector() {
        for read_pba in [false, true] {
            for global_unmask in [false, true] {
                check_mapped_sources_share_vector(read_pba, global_unmask);
            }
        }
    }

    fn check_mapped_sources_share_vector(read_pba: bool, global_unmask: bool) {
        let (irqfd, calls) = mock_irqfd(2);
        let conn = MsiConnection::new();
        conn.connect_irqfd(irqfd);
        let intc = TestPciInterruptController::new();
        conn.connect(intc.signal_msi());
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, &conn.target());
        write_cap_u32(&mut cap, 0, 0x80000000);
        let sources = [msix.new_interrupt(), msix.new_interrupt()];
        for source in &sources {
            assert!(source.set_vector(Some(0)));
            source.interrupt().event().unwrap().signal();
        }
        if read_pba {
            assert_eq!(msix.read_u32(32), 1);
            for source in &sources {
                source.interrupt().event().unwrap().signal();
            }
        }
        msix.write_u32(8, 0x40);
        if global_unmask {
            write_cap_u32(&mut cap, 0, 0);
            msix.write_u32(12, 0);
            write_cap_u32(&mut cap, 0, 0x80000000);
        } else {
            msix.write_u32(12, 0);
        }
        assert_eq!(intc.get_next_interrupt(), Some((0, 0x40)));
        assert!(intc.get_next_interrupt().is_none());
        for (source, log) in sources.iter().zip(&calls) {
            assert!(!source.interrupt().event().unwrap().try_wait());
            assert!(matches!(
                log.lock().last(),
                Some(RouteCall::SetMsi { data: 0x40, .. })
            ));
        }

        sources[1].set_vector(Some(1));
        calls[1].lock().clear();
        msix.write_u32(8, 0x41);
        assert!(matches!(
            calls[0].lock().last(),
            Some(RouteCall::SetMsi { data: 0x41, .. })
        ));
        assert!(calls[1].lock().is_empty());
        sources[1].interrupt().event().unwrap().signal();
        assert_eq!(msix.read_u32(32), 2);
    }

    #[test]
    fn mapped_source_save_restore_pending() {
        let (irqfd, calls) = mock_irqfd(1);
        let conn = MsiConnection::new();
        conn.connect_irqfd(irqfd);
        let intc = TestPciInterruptController::new();
        conn.connect(intc.signal_msi());
        let (mut msix, mut cap) = MsixEmulator::new(2, 2, &conn.target());
        write_cap_u32(&mut cap, 0, 0x80000000);
        msix.write_u32(0, 0xfee00000);
        msix.write_u32(8, 0x40);
        let source = msix.new_interrupt();
        source.set_vector(Some(0));
        let event = source.interrupt().event().unwrap().clone();
        event.signal();

        let saved = vmcore::save_restore::ProtobufSaveRestore::save(&mut cap).unwrap();
        cap.reset();
        assert_eq!(msix.read_u32(32), 0);
        assert!(!event.try_wait());
        assert_eq!(calls[0].lock().last(), Some(&RouteCall::ClearMsi));
        vmcore::save_restore::ProtobufSaveRestore::restore(&mut cap, saved).unwrap();
        assert_eq!(msix.read_u32(32), 1);
        msix.write_u32(12, 0);
        assert_eq!(intc.get_next_interrupt(), Some((0xfee00000, 0x40)));
        assert!(intc.get_next_interrupt().is_none());
        assert_eq!(
            calls[0].lock().last(),
            Some(&RouteCall::SetMsi {
                address: 0xfee00000,
                data: 0x40,
            })
        );
    }
}
