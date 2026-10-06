// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A very incomplete implementation of ARM GICv3.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use std::collections::HashMap;

pub use gicd::Distributor;
pub use gicr::Redistributor;

#[derive(Clone, Copy, Debug)]
pub struct PendingInterrupt {
    pub intid: u32,
    pub priority: u8,
    pub group1: bool,
}

/// The returned execution state of a virtual interrupt represented by a list
/// register. Configuration remains in the software GIC model; this only
/// describes the transient pending/active state owned by the list register.
#[derive(Clone, Copy, Debug)]
pub struct ListRegisterInterrupt {
    pub intid: u32,
    pub pending: bool,
    pub active: bool,
}

use aarch64defs::SystemReg;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::VpIndex;
use vm_topology::processor::aarch64::Aarch64Topology;
use vm_topology::processor::aarch64::GicVersion;

#[derive(Debug, thiserror::Error)]
pub enum GicV3ModelError {
    #[error("software GIC requires GICv3")]
    UnsupportedGicVersion,
    #[error("GIC redistributor range overflowed")]
    RedistributorRangeOverflow,
    #[error("GIC distributor range overflowed")]
    DistributorRangeOverflow,
}

/// Software model of a GICv3 distributor and its per-VP redistributors.
pub struct GicV3Model {
    /// Shared distributor state and interrupt-routing logic.
    distributor: Distributor,
    /// Redistributor register interfaces indexed by VP index.
    redistributors: Vec<Mutex<Redistributor>>,
    /// Distributor MMIO address range.
    distributor_range: MemoryRange,
    /// Contiguous MMIO address range containing all redistributor frames.
    redistributor_range: MemoryRange,
}

impl GicV3Model {
    pub fn new(topology: &ProcessorTopology<Aarch64Topology>) -> Result<Self, GicV3ModelError> {
        let redistributors_base = match topology.gic_version() {
            GicVersion::V3 {
                redistributors_base,
            } => redistributors_base,
            GicVersion::V2 { .. } => return Err(GicV3ModelError::UnsupportedGicVersion),
        };
        let redistributors_size = aarch64defs::GIC_REDISTRIBUTOR_SIZE
            .checked_mul(u64::from(topology.vp_count()))
            .ok_or(GicV3ModelError::RedistributorRangeOverflow)?;
        let redistributors_end = redistributors_base
            .checked_add(redistributors_size)
            .ok_or(GicV3ModelError::RedistributorRangeOverflow)?;
        let redistributor_range = MemoryRange::new(redistributors_base..redistributors_end);
        let distributor_base = topology.gic_distributor_base();
        let distributor_end = distributor_base
            .checked_add(aarch64defs::GIC_DISTRIBUTOR_SIZE)
            .ok_or(GicV3ModelError::DistributorRangeOverflow)?;
        let distributor_range = MemoryRange::new(distributor_base..distributor_end);

        let mut distributor = Distributor::new(
            distributor_base,
            redistributor_range,
            topology.gic_nr_irqs(),
        );

        let mut redistributors: Vec<Mutex<Redistributor>> = vec![];
        let vp_count = topology.vp_count() as usize;
        for (index, vp) in topology.vps_arch().enumerate() {
            let gicr = distributor.add_redistributor(vp.mpidr.into(), index + 1 == vp_count);
            redistributors.push(Mutex::new(gicr));
        }

        Ok(Self {
            distributor,
            redistributors,
            distributor_range,
            redistributor_range,
        })
    }

    pub fn contains(&self, address: u64) -> bool {
        self.distributor_range.contains_addr(address)
            || self.redistributor_range.contains_addr(address)
    }

    pub fn read(&self, address: u64, data: &mut [u8]) -> bool {
        self.distributor.read(address, data)
    }

    pub fn write(&self, address: u64, data: &[u8]) -> bool {
        self.distributor.write(address, data)
    }

    pub fn set_spi_irq(&self, intid: u32, high: bool) -> Vec<VpIndex> {
        self.distributor.set_spi_irq(intid, high)
    }

    pub fn cancel_spi_reservation(&self, vp: VpIndex, intid: u32) {
        self.distributor.cancel_spi_reservation(vp, intid);
    }

    /// Reconcile returned list registers with the durable interrupt model.
    ///
    /// List registers are an execution cache. Pending latches raised while a
    /// VP was running are retained, while returned pending and active state is
    /// folded into the model before the registers are repacked.
    pub fn fold_list_registers(&self, vp: VpIndex, lrs: &[ListRegisterInterrupt]) {
        self.distributor.fold_list_registers(vp, lrs);
    }

    pub fn write_sysreg(
        &self,
        vp: VpIndex,
        reg: SystemReg,
        value: u64,
        wake: impl FnMut(usize),
    ) -> bool {
        let Some(gicr) = self.redistributors.get(vp.index() as usize) else {
            return false;
        };
        let mut gicr = gicr.lock();
        self.distributor.write_sysreg(&mut gicr, reg, value, wake)
    }

    pub fn next_pending_private_interrupts(
        &self,
        vp: VpIndex,
        running_priority: u8,
    ) -> Option<HashMap<u64, PendingInterrupt>> {
        self.distributor
            .next_private_interrupts(vp, running_priority)
    }

    pub fn reserve_pending_spi_interrupts(
        &self,
        vp: VpIndex,
        running_priority: u8,
    ) -> Option<HashMap<u64, PendingInterrupt>> {
        self.distributor
            .reserve_pending_spi_interrupts(vp, running_priority)
    }

    pub fn raise_ppi(&self, vp: VpIndex, intid: u32) -> bool {
        self.distributor.raise_ppi(vp, intid)
    }

    pub fn mark_private_injected(&self, vp: VpIndex, intid: u32) {
        self.distributor.mark_private_injected(vp, intid);
    }

    pub fn pend_ppi(&self, asserted: bool, intid: u32, vp: VpIndex) -> bool {
        match self.redistributors.get(vp.index() as usize) {
            Some(gicr) => {
                let gicr = gicr.lock();
                gicr.shared.pend_ppi(asserted, intid)
            }
            None => false,
        }
    }

    pub fn get_latches(&self, vp: VpIndex) -> Option<(u32, u32)> {

        match self.redistributors.get(vp.index() as usize) {
            Some(gicr) => Some(gicr.lock().shared.get_latches()),
            None => None,
        }
    }
}

mod gicd {
    use super::ListRegisterInterrupt;
    use super::PendingInterrupt;
    use super::Redistributor;
    use super::gicr::SharedState;
    use aarch64defs::MpidrEl1;
    use aarch64defs::SystemReg;
    use aarch64defs::gic::GicdCtlr;
    use aarch64defs::gic::GicdRegister;
    use aarch64defs::gic::GicdTyper;
    use aarch64defs::gic::GicdTyper2;
    use aarch64defs::gic::GicrSgi;
    use inspect::Inspect;
    use memory_range::MemoryRange;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use vm_topology::processor::VpIndex;
    use std::collections::HashMap;

    #[derive(Debug, Inspect)]
    pub struct Distributor {
        state: Arc<Mutex<DistributorState>>,
        max_spi_intid: u32,
        #[inspect(skip)]
        gicr: Vec<Arc<SharedState>>,
        gicd_range: MemoryRange,
        gicr_range: MemoryRange,
    }

    #[derive(Debug, Inspect)]
    pub struct DistributorState {
        /// Level-triggered SPI input lines currently asserted by devices.
        #[inspect(iter_by_index)]
        asserted: Vec<u32>,
        #[inspect(iter_by_index)]
        pending: Vec<u32>,
        /// Bitmap of 32-INTID words that contain pending or asserted SPIs.
        pending_word_summary: u32,
        /// SPIs reserved for injection or represented in the CCA virtual
        /// active/pending list, indexed by INTID. Some entries in that list can
        /// live outside the implemented hardware list registers.
        #[inspect(iter_by_index)]
        in_flight: Vec<Option<u32>>,
        /// Reserved or in-flight SPI INTIDs owned by each VP.
        #[inspect(skip)]
        in_flight_by_vp: Vec<Vec<u32>>,
        #[inspect(iter_by_index)]
        active: Vec<u32>,
        #[inspect(iter_by_index)]
        group_status: Vec<u32>,
        #[inspect(iter_by_index)]
        group_modifier: Vec<u32>,
        #[inspect(iter_by_index)]
        enable: Vec<u32>,
        #[inspect(iter_by_index)]
        cfg: Vec<u32>,
        #[inspect(iter_by_index)]
        priority: Vec<u32>,
        #[inspect(iter_by_index)]
        route: Vec<u64>,
        pub enable_grp0: bool,
        pub enable_grp1_non_secure: bool,
        pub enable_grp1_secure: bool,
    }

    impl Distributor {
        pub fn new(gicd_base: u64, gicr_range: MemoryRange, interrupt_count: u32) -> Self {
            let n = interrupt_count.div_ceil(32) as usize;
            assert!(n <= u32::BITS as usize);
            Self {
                state: Arc::new(Mutex::new(DistributorState {
                    asserted: vec![0; n],
                    pending: vec![0; n],
                    pending_word_summary: 0,
                    in_flight: vec![None; n * 32],
                    in_flight_by_vp: Vec::new(),
                    active: vec![0; n],
                    group_status: vec![0; n],
                    group_modifier: vec![0; n],
                    enable: vec![0; n],
                    cfg: vec![0; n * 2],
                    priority: vec![0; n * 8],
                    route: vec![0; n * 64],
                    enable_grp0: false,
                    enable_grp1_non_secure: false,
                    enable_grp1_secure: false,
                })),
                max_spi_intid: interrupt_count.saturating_sub(1),
                gicr: Default::default(),
                gicd_range: MemoryRange::new(
                    gicd_base..gicd_base + aarch64defs::GIC_DISTRIBUTOR_SIZE,
                ),
                gicr_range,
            }
        }

        pub fn add_redistributor(&mut self, mpidr: u64, last: bool) -> Redistributor {
            let mpidr = mpidr & u64::from(MpidrEl1::AFFINITY_MASK);
            let (gicr, state) = Redistributor::new(self.gicr.len(), mpidr, last, self.state.clone());
            self.gicr.push(state);
            self.state
                .lock()
                .in_flight_by_vp
                .push(Vec::with_capacity(16));
            assert!(
                (self.gicr.len() as u64)
                    <= self.gicr_range.len() / aarch64defs::GIC_REDISTRIBUTOR_SIZE
            );
            gicr
        }

        pub fn raise_ppi(&self, vp: VpIndex, intid: u32) -> bool {
            if let Some(gicr) = self.gicr.get(vp.index() as usize) {
                gicr.raise(intid)
            } else {
                false
            }
        }

        pub fn mark_private_injected(&self, vp: VpIndex, intid: u32) {
            if intid < 32 {
                if let Some(gicr) = self.gicr.get(vp.index() as usize) {
                    gicr.mark_injected(intid);
                }
            }
        }

        pub fn set_spi_irq(&self, intid: u32, high: bool) -> Vec<VpIndex> {
            if intid < 32 || intid > self.max_spi_intid {
                tracelimit::warn_ratelimited!(intid, high, "invalid GIC SPI assertion");
                return Vec::new();
            }

            let mut state = self.state.lock();
            let changed = Self::set_bit(&mut state.asserted, intid, high);
            Self::update_pending_word_summary(&mut state, intid as usize / 32);
            if changed && high && Self::edge_triggered(&state, intid) {
                Self::set_pending_locked(&mut state, intid, true);
            }
            if !changed || !high {
                return Vec::new();
            }

            self.target_vps_for_spi(&state, intid)
        }

        #[cfg(test)]
        pub fn mark_spi_injected(&self, vp: VpIndex, intid: u32) {
            let mut state = self.state.lock();
            Self::set_pending_locked(&mut state, intid, false);
            Self::set_in_flight_locked(&mut state, vp, intid);
        }

        #[cfg(test)]
        pub fn retain_in_flight_spis(
            &self,
            vp: VpIndex,
            mut is_in_flight: impl FnMut(u32) -> bool,
        ) {
            let mut state = self.state.lock();
            let vp_index = vp.index() as usize;
            let Some(owned) = state.in_flight_by_vp.get(vp_index).cloned() else {
                return;
            };
            for intid in owned {
                if !is_in_flight(intid) {
                    state.in_flight[intid as usize] = None;
                    state.in_flight_by_vp[vp_index].retain(|owned_intid| *owned_intid != intid);
                }
            }
        }

        pub fn cancel_spi_reservation(&self, vp: VpIndex, intid: u32) {
            if intid < 32 || intid > self.max_spi_intid {
                tracelimit::warn_ratelimited!(intid, "invalid GIC SPI reservation cancellation");
                return;
            }

            let mut state = self.state.lock();
            if state.in_flight[intid as usize] == Some(vp.index()) {
                state.in_flight[intid as usize] = None;
                if let Some(owned) = state.in_flight_by_vp.get_mut(vp.index() as usize) {
                    owned.retain(|owned_intid| *owned_intid != intid);
                }
            }
        }

        pub fn fold_list_registers(&self, vp: VpIndex, lrs: &[ListRegisterInterrupt]) {
            let mut state = self.state.lock();
            let vp_index = vp.index() as usize;
            let Some(owned) = state.in_flight_by_vp.get(vp_index).cloned() else {
                return;
            };

            for intid in owned {
                let returned = lrs.iter().find(|lr| lr.intid == intid);
                let keep = returned.is_some();
                if let Some(lr) = returned {
                    Self::set_bit(&mut state.active, intid, lr.active);
                    if lr.pending {
                        Self::set_pending_locked(&mut state, intid, true);
                    }
                } else {
                    Self::set_bit(&mut state.active, intid, false);
                    state.in_flight[intid as usize] = None;
                }
                if !keep {
                    state.in_flight_by_vp[vp_index].retain(|owned_intid| *owned_intid != intid);
                }
            }

            drop(state);
            if let Some(gicr) = self.gicr.get(vp_index) {
                gicr.fold_list_registers(lrs);
            }
        }


        pub fn reserve_pending_spi_interrupts(
            &self,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<HashMap<u64, PendingInterrupt>> {
            let mut state = self.state.lock();
            let interrupts = self.next_spi_interrupts_locked(&state, vp, running_priority)?;
            // Reserve while still holding the selection lock so another VP
            // cannot select the same IRM-routed SPIs.
            for (&intid, _) in &interrupts {
                Self::set_pending_locked(&mut state, intid as u32, false);
                Self::set_in_flight_locked(&mut state, vp, intid as u32);
            }
            Some(interrupts)
        }

        pub fn next_private_interrupts(
            &self,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<HashMap<u64, PendingInterrupt>> {
            if !self.state.lock().enable_grp1_non_secure && !self.state.lock().enable_grp1_secure {
                return None;
            }

            self.gicr
                .get(vp.index() as usize)?
                .next_private_interrupts(running_priority)
        }

        pub fn clear_pending(&self, vp_index: usize, intid: u32) {
            if intid < 32 {
                if let Some(gicr) = self.gicr.get(vp_index) {
                    gicr.clear_pending(intid);
                }
            } else {
                let mut state = self.state.lock();
                Self::set_pending_locked(&mut state, intid, false);
            }
        }

        fn next_spi_interrupts_locked(
            &self,
            state: &DistributorState,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<HashMap<u64, PendingInterrupt>> {
            if !state.enable_grp1_non_secure && !state.enable_grp1_secure {
                return None;
            }

            let mut interrupts = HashMap::new();
            let mut ready_words = state.pending_word_summary & !1;
            while ready_words != 0 {
                let word = ready_words.trailing_zeros() as usize;
                ready_words &= ready_words - 1;

                let mut group = 0;
                if state.enable_grp0 {
                    group |= !state.group_modifier[word] & !state.group_status[word];
                }
                if state.enable_grp1_non_secure {
                    // modified bit: 0b1, status bit: 0b1, Reserved, treated as Non-secure Group 1
                    group |= state.group_status[word];
                }
                if state.enable_grp1_secure {
                    group |= state.group_modifier[word] & !state.group_status[word];
                }

                let mut candidates = (state.pending[word] | state.asserted[word])
                    & state.enable[word]
                    & group;

                while candidates != 0 {
                    let bit = candidates.trailing_zeros();
                    candidates &= candidates - 1;

                    let intid = word as u32 * 32 + bit;
                    let mask = 1 << bit;
                    let pending = state.pending[word] & mask != 0;
                    let level_asserted =
                        state.asserted[word] & mask != 0 && !Self::edge_triggered(&state, intid);
                    let group1 = (state.group_status[word] & mask) | (state.group_modifier[word] & mask) != 0;
                    if !pending && !level_asserted {
                        continue;
                    }
                    let owned_by_vp = state.in_flight[intid as usize] == Some(vp.index());
                    if !pending && owned_by_vp {
                        continue;
                    }
                    if intid > self.max_spi_intid
                        || (state.in_flight[intid as usize].is_some() && !owned_by_vp)
                        || (state.active[word] & mask != 0 && !owned_by_vp)
                        || !self.spi_targets_vp(&state, intid, vp)
                    {
                        continue;
                    }

                    let priority = Self::priority(&state.priority, intid);
                    if priority >= running_priority {
                        continue;
                    }

                    let interrupt = PendingInterrupt {
                        intid,
                        priority,
                        group1,
                    };

                    interrupts.insert(intid as u64, interrupt);
                }
            }

            if interrupts.is_empty() {
                return None;
            }

            Some(interrupts)
        }

        fn set_pending_locked(state: &mut DistributorState, intid: u32, pending: bool) -> bool {
            let changed = Self::set_bit(&mut state.pending, intid, pending);
            if changed {
                Self::update_pending_word_summary(state, intid as usize / 32);
                tracing::debug!(intid, pending, "set pending");
            }
            changed
        }

        fn update_pending_word_summary(state: &mut DistributorState, word: usize) {
            let mask = 1 << word;
            if state.pending[word] | state.asserted[word] != 0 {
                state.pending_word_summary |= mask;
            } else {
                state.pending_word_summary &= !mask;
            }
        }

        fn set_in_flight_locked(state: &mut DistributorState, vp: VpIndex, intid: u32) {
            let vp_index = vp.index() as usize;
            if state.in_flight[intid as usize] == Some(vp.index()) {
                return;
            }

            if let Some(previous_vp) = state.in_flight[intid as usize] {
                if let Some(owned) = state.in_flight_by_vp.get_mut(previous_vp as usize) {
                    owned.retain(|owned_intid| *owned_intid != intid);
                }
            }
            state.in_flight[intid as usize] = Some(vp.index());
            state.in_flight_by_vp[vp_index].push(intid);
        }

        fn set_bit(bits: &mut [u32], intid: u32, value: bool) -> bool {
            let Some(word) = bits.get_mut(intid as usize / 32) else {
                return false;
            };

            let mask = 1 << (intid & 31);
            let changed = (*word & mask != 0) != value;
            if value {
                *word |= mask;
            } else {
                *word &= !mask;
            }
            changed
        }

        /// Returns whether `intid` is configured as edge-triggered in GICD_ICFGR.
        /// Each INTID uses two bits: bit 1 selects edge (1) or level (0), while bit 0 is RES0.
        /// 31 30 | 29 28 | ... | 5 4 | 3 2 | 1 0
        /// ------|-------|-----|-----|-----|------
        ///  IRQ15| IRQ14 | ... | IRQ2| IRQ1| IRQ0
        /// trigger bit = 0 → level-triggered
        /// trigger bit = 1 → edge-triggered
        /// each word describes 16 interrupts
        fn edge_triggered(state: &DistributorState, intid: u32) -> bool {
            let word = state.cfg.get(intid as usize / 16).copied().unwrap_or(0);
            let shift = (intid % 16) * 2 + 1;
            word & (1 << shift) != 0
        }

        fn priority(priority: &[u32], intid: u32) -> u8 {
            let word = priority.get(intid as usize / 4).copied().unwrap_or(0);
            let shift = (intid % 4) * 8;
            ((word >> shift) & 0xff) as u8
        }

        fn target_vps_for_spi(&self, state: &DistributorState, intid: u32) -> Vec<VpIndex> {
            self.gicr
                .iter()
                .enumerate()
                .filter_map(|(index, _)| {
                    let vp = VpIndex::new(index as u32);
                    self.spi_targets_vp(state, intid, vp).then_some(vp)
                })
                .collect()
        }

        fn spi_targets_vp(&self, state: &DistributorState, intid: u32, vp: VpIndex) -> bool {
            let Some(gicr) = self.gicr.get(vp.index() as usize) else {
                return false;
            };
            let route = state.route.get(intid as usize).copied().unwrap_or(0);
            if route & (1 << 31) != 0 {
                return true;
            }

            let mpidr = gicr.mpidr;
            u64::from(mpidr.aff0()) == (route & 0xff)
                && u64::from(mpidr.aff1()) == ((route >> 8) & 0xff)
                && u64::from(mpidr.aff2()) == ((route >> 16) & 0xff)
                && u64::from(mpidr.aff3()) == ((route >> 32) & 0xff)
        }

        pub fn set_pending(&self, intid: u32, pending: bool) -> Option<u32> {
            if Self::set_pending_locked(&mut self.state.lock(), intid, pending) && pending {
                Some(0)
            } else {
                None
            }
        }

        pub fn irq_pending(&self, gicr: &Redistributor) -> bool {
            if gicr.irq_pending() {
                return true;
            }
            if gicr.index != 0 {
                return false;
            }
            let state = self.state.lock();
            state
                .pending
                .iter()
                .zip(&state.active)
                .zip(&state.enable)
                .any(|((&p, &a), e)| p & !a & e != 0)
        }

        pub fn ack(&self, gicr: &mut Redistributor, group1: bool) -> u32 {
            if let Some(intid) = gicr.ack(group1) {
                return intid;
            }
            if gicr.index != 0 {
                return 1023;
            }
            let mut state = self.state.lock();
            let state = &mut *state;
            if let Some((i, (p, a))) = state
                .pending
                .iter_mut()
                .zip(&mut state.active)
                .enumerate()
                .find(|(_, (p, a))| **p & !**a != 0)
            {
                let v = 31 - (*p & !*a).leading_zeros();
                *p &= !(1 << v);
                *a |= 1 << v;
                let intid = i as u32 * 32 + v;
                Self::update_pending_word_summary(state, i);
                tracing::debug!(intid, "gicd ack");
                intid
            } else {
                1023
            }
        }

        pub fn write_sysreg(
            &self,
            gicr: &mut Redistributor,
            reg: SystemReg,
            value: u64,
            wake: impl FnMut(usize),
        ) -> bool {
            match reg {
                SystemReg::ICC_EOIR0_EL1 => self.eoi(gicr, false, value as u32),
                SystemReg::ICC_EOIR1_EL1 => self.eoi(gicr, true, value as u32),
                SystemReg::ICC_SGI0R_EL1 => self.sgi(gicr, false, value, wake),
                SystemReg::ICC_SGI1R_EL1 => self.sgi(gicr, true, value, wake),
                _ => return false,
            }
            true
        }

        fn sgi(
            &self,
            this: &mut Redistributor,
            _group1: bool,
            value: u64,
            mut wake: impl FnMut(usize),
        ) {
            let value = GicrSgi::from(value);
            for (index, gicr) in self.gicr.iter().enumerate() {
                if (value.irm() && !Arc::ptr_eq(&this.shared, gicr))
                    || (!value.irm()
                        && gicr.mpidr.aff3() == value.aff3()
                        && gicr.mpidr.aff2() == value.aff2()
                        && gicr.mpidr.aff1() == value.aff1()
                        && (gicr.mpidr.aff0() >> 4) == value.rs() && (value.target_list() & (1 << (gicr.mpidr.aff0() & 0xf)) != 0))
                {
                    if gicr.raise(value.intid()) {
                        wake(index);
                    }
                }
            }
        }

        pub fn read_sysreg(&self, gicr: &mut Redistributor, reg: SystemReg) -> Option<u64> {
            let v = match reg {
                SystemReg::ICC_IAR0_EL1 => self.ack(gicr, false).into(),
                SystemReg::ICC_IAR1_EL1 => self.ack(gicr, true).into(),
                _ => return None,
            };
            Some(v)
        }

        fn eoi(&self, gicr: &mut Redistributor, group1: bool, intid: u32) {
            if intid < 32 {
                gicr.eoi(group1, intid);
                return;
            }
            if gicr.index != 0 {
                return;
            }
            tracing::debug!(intid, "gicd eoi");
            let v = &mut self.state.lock().active[intid as usize / 32];
            *v &= !(1 << (intid & 31));
        }

        fn write8(&self, address: GicdRegister, value: u8) -> bool {
            let j = address.0%4;
            match address {
                r if GicdRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x3ff) / 4;
                    if n >= 8 {
                        if let Some(priority) = self.state.lock().priority.get_mut(n as usize) {
                            *priority &= !(0xff << (j * 8));
                            *priority |= u32::from(value) << (j * 8);
                        }
                    }
                }
                _ => return false,
            };

            true
        }

        fn read8(&self, address: GicdRegister) -> Option<u8> {
            let j = address.0 % 4;
            match address {
                r if GicdRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x3ff) / 4;
                    if n >= 8 {
                        if let Some(priority) = self.state.lock().priority.get(n as usize) {
                           return Some(((*priority >> (j * 8)) & 0xff) as u8);
                        }
                    }
                    None
                },
                _ => None,
            }
        }

        fn write32(&self, address: GicdRegister, value: u32) -> bool {
            assert!(address.0 & 3 == 0);
            match address {
                GicdRegister::CTLR => {
                    let ctlr = GicdCtlr::from(value);
                    let mut state = self.state.lock();
                    let state = &mut *state;
                    state.enable_grp0 = ctlr.enable_grp0();
                    state.enable_grp1_non_secure = ctlr.enable_grp1_non_secure();
                    state.enable_grp1_secure = ctlr.enable_grp1_secure();
                }
                r if GicdRegister::IGROUPR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        if let Some(group_status) = self.state.lock().group_status.get_mut(n as usize) {
                            *group_status = value;
                        }
                    }
                }
                r if GicdRegister::ISENABLER.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        if let Some(enable) = self.state.lock().enable.get_mut(n as usize) {
                            *enable |= value;
                        }
                    }
                }
                r if GicdRegister::ICENABLER.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        if let Some(enable) = self.state.lock().enable.get_mut(n as usize) {
                            *enable &= !value;
                        }
                    }
                }
                r if GicdRegister::ISPENDR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        let mut state = self.state.lock();
                        if let Some(pending) = state.pending.get_mut(n as usize) {
                            *pending |= value;
                            Self::update_pending_word_summary(&mut state, n as usize);
                        }
                    }
                }
                r if GicdRegister::ICPENDR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        let mut state = self.state.lock();
                        if let Some(pending) = state.pending.get_mut(n as usize) {
                            *pending &= !value;
                            Self::update_pending_word_summary(&mut state, n as usize);
                        }
                    }
                }
                r if GicdRegister::ICFGR.contains(&r.0) => {
                    let n = (r.0 & 0xff) / 4;
                    if n >= 2 {
                        if let Some(cfg) = self.state.lock().cfg.get_mut(n as usize) {
                            // The low bit of each bit pair is res0.
                            *cfg = value & 0xaaaaaaaa;
                        }
                    }
                }
                r if GicdRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x3ff) / 4;
                    if n >= 8 {
                        if let Some(priority) = self.state.lock().priority.get_mut(n as usize) {
                            *priority = value;
                        }
                    }
                }
                r if GicdRegister::ISACTIVER.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        if let Some(active) = self.state.lock().active.get_mut(n as usize) {
                            *active |= value;
                        }
                    }
                }
                r if GicdRegister::ICACTIVER.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        if let Some(active) = self.state.lock().active.get_mut(n as usize) {
                            *active &= !value;
                        }
                    }
                }
                r if GicdRegister::IGRPMODR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        if let Some(group_modifier) = self.state.lock().group_modifier.get_mut(n as usize) {
                            *group_modifier |= value;
                        }
                    }
                }
                _ => return false,
            }
            true
        }

        fn read32(&self, address: GicdRegister) -> Option<u32> {
            assert!(address.0 & 3 == 0);
            let v = match address {
                GicdRegister::PIDR2 => {
                    // GICv3
                    3 << 4
                }
                GicdRegister::TYPER => {
                    let it_lines_number = ((self.max_spi_intid + 1).div_ceil(32) - 1) as u8;
                    let id_bits = (32 - self.max_spi_intid.leading_zeros()) as u8;
                    GicdTyper::new()
                        .with_it_lines_number(it_lines_number)
                        .with_id_bits(id_bits)
                        // IROUTER matching includes Aff3, so advertise that
                        // Aff3 routing is implemented.
                        .with_a3v(true)
                        .into()
                }
                GicdRegister::IIDR => 0,
                GicdRegister::TYPER2 => GicdTyper2::new().into(),
                GicdRegister::CTLR => {
                    let state = self.state.lock();
                    GicdCtlr::new()
                        .with_enable_grp0(state.enable_grp0)
                        .with_enable_grp1_non_secure(state.enable_grp1_non_secure)
                        .with_enable_grp1_secure(state.enable_grp1_secure)
                        .with_ds(true)
                        .with_are(true)
                        .into()
                }
                r if GicdRegister::IGROUPR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    self.state
                        .lock()
                        .group_status
                        .get(n as usize)
                        .copied()
                        .unwrap_or(0)
                }
                r if GicdRegister::ICENABLER.contains(&r.0)
                    || GicdRegister::ISENABLER.contains(&r.0) =>
                {
                    let n = (r.0 & 0x7f) / 4;
                    self.state
                        .lock()
                        .enable
                        .get(n as usize)
                        .copied()
                        .unwrap_or(0)
                }
                r if GicdRegister::ICFGR.contains(&r.0) => {
                    let n = (r.0 & 0xff) / 4;
                    self.state.lock().cfg.get(n as usize).copied().unwrap_or(0)
                }
                r if GicdRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x3ff) / 4;
                    self.state
                        .lock()
                        .priority
                        .get(n as usize)
                        .copied()
                        .unwrap_or(0)
                }
                r if GicdRegister::ICACTIVER.contains(&r.0)
                    || GicdRegister::ISACTIVER.contains(&r.0) =>
                {
                    let n = (r.0 & 0x7f) / 4;
                    self.state
                        .lock()
                        .active
                        .get(n as usize)
                        .copied()
                        .unwrap_or(0)
                }
                r if GicdRegister::ICPENDR.contains(&r.0)
                    || GicdRegister::ISPENDR.contains(&r.0) =>
                {
                    let n = (r.0 & 0x7f) / 4;
                    self.state
                        .lock()
                        .pending
                        .get(n as usize)
                        .copied()
                        .unwrap_or(0)
                }
                r if GicdRegister::IGRPMODR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    self.state
                        .lock()
                        .group_modifier
                        .get(n as usize)
                        .copied()
                        .unwrap_or(0)
                }
                _ => return None,
            };
            Some(v)
        }

        fn write64(&self, address: GicdRegister, value: u64) -> bool {
            assert!(address.0 & 7 == 0);
            match address {
                r if GicdRegister::IROUTER.contains(&r.0) => {
                    let n = (r.0 & 0x1fff) / 8;
                    if n >= 32 {
                        if let Some(route) = self.state.lock().route.get_mut(n as usize) {
                            *route = value;
                        }
                    }
                }
                _ => return false,
            }
            true
        }

        fn read64(&self, address: GicdRegister) -> Option<u64> {
            assert!(address.0 & 7 == 0);
            let v = match address {
                r if GicdRegister::IROUTER.contains(&r.0) => {
                    let n = (r.0 & 0x1fff) / 8;
                    self.state
                        .lock()
                        .route
                        .get(n as usize)
                        .copied()
                        .unwrap_or(0)
                }
                _ => return None,
            };
            Some(v)
        }

        pub fn read(&self, address: u64, data: &mut [u8]) -> bool {
            if self.gicd_range.contains_addr(address) {
                self.read_gicd(address - self.gicd_range.start(), data);
            } else if self.gicr_range.contains_addr(address) {
                let vp = (address - self.gicr_range.start()) / aarch64defs::GIC_REDISTRIBUTOR_SIZE;
                if let Some(gicr) = self.gicr.get(vp as usize) {
                    gicr.read(address - self.gicr_range.start(), data);
                } else {
                    tracelimit::warn_ratelimited!(
                        address,
                        ?data,
                        "gicr read unallocated redistributor"
                    );
                    data.fill(0);
                }
            } else {
                return false;
            }
            true
        }

        fn read_gicd(&self, address: u64, data: &mut [u8]) {
            if address & (data.len() as u64 - 1) != 0 {
                data.fill(!0);
                tracing::warn!(address, ?data, "gicd read unaligned access");
                return;
            }
            let address = GicdRegister(address as u16);
            let handled = match data.len() {
                1 => {
                    if let Some(v) = self.read8(address) {
                        data[0] = v;
                        true
                    } else {
                        false
                    }
                }
                4 => {
                    if let Some(v) = self.read32(address) {
                        data.copy_from_slice(&v.to_ne_bytes());
                        true
                    } else {
                        false
                    }
                }
                8 => {
                    if let Some(v) = self.read64(address) {
                        data.copy_from_slice(&v.to_ne_bytes());
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            };
            if !handled {
                data.fill(0);
                tracelimit::warn_ratelimited!(?address, ?data, "unsupported gicd register read");
            }
        }

        pub fn write(&self, address: u64, data: &[u8]) -> bool {
            if self.gicd_range.contains_addr(address) {
                self.write_gicd(address - self.gicd_range.start(), data);
            } else if self.gicr_range.contains_addr(address) {
                let vp = (address - self.gicr_range.start()) / aarch64defs::GIC_REDISTRIBUTOR_SIZE;
                if let Some(gicr) = self.gicr.get(vp as usize) {
                    gicr.write(address - self.gicr_range.start(), data);
                } else {
                    tracelimit::warn_ratelimited!(
                        address,
                        ?data,
                        "gicr write unallocated redistributor"
                    );
                }
            } else {
                return false;
            }
            true
        }

        fn write_gicd(&self, address: u64, data: &[u8]) {
            if address & (data.len() as u64 - 1) != 0 {
                tracing::warn!(address, ?data, "gicd write unaligned access");
                return;
            }
            let address = GicdRegister(address as u16);
            let handled = match data.len() {
                1 => self.write8(address, u8::from_ne_bytes(data.try_into().unwrap())),
                4 => self.write32(address, u32::from_ne_bytes(data.try_into().unwrap())),
                8 => self.write64(address, u64::from_ne_bytes(data.try_into().unwrap())),
                _ => false,
            };
            if !handled {
                tracelimit::warn_ratelimited!(?address, ?data, "unsupported gicd register write");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use aarch64defs::gic::GicdTyper;
        use aarch64defs::gic::GicrSgiRegister;

        const TEST_SPI: u32 = 32;

        fn test_distributor() -> Distributor {
            let mut distributor = Distributor::new(
                0,
                MemoryRange::new(
                    aarch64defs::GIC_DISTRIBUTOR_SIZE
                        ..aarch64defs::GIC_DISTRIBUTOR_SIZE
                            + 2 * aarch64defs::GIC_REDISTRIBUTOR_SIZE,
                ),
                64,
            );
            distributor.add_redistributor(0, true);

            let mut state = distributor.state.lock();
            state.enable_grp1_non_secure = true;
            state.enable[1] |= 1;
            state.group_status[1] |= 1;
            state.group_modifier[1] |= 1;
            drop(state);

            distributor
        }

        #[test]
        fn typer_matches_the_implemented_interrupts_and_routing_model() {
            const INTERRUPT_COUNT: u32 = 256;
            let mut distributor = Distributor::new(
                0,
                MemoryRange::new(
                    aarch64defs::GIC_DISTRIBUTOR_SIZE
                        ..aarch64defs::GIC_DISTRIBUTOR_SIZE
                            + 2 * aarch64defs::GIC_REDISTRIBUTOR_SIZE,
                ),
                INTERRUPT_COUNT,
            );
            distributor.add_redistributor(0, false);
            distributor.add_redistributor(1 << 32, true);

            let typer = GicdTyper::from(distributor.read32(GicdRegister::TYPER).unwrap());
            assert_eq!(typer.it_lines_number(), 7);
            assert_eq!(typer.id_bits(), 8);
            assert!(typer.a3v());

            // Registers describing INTIDs 256 and above are RAZ/WI because
            // this distributor implements only INTIDs 0 through 255.
            let unimplemented_enable = GicdRegister(GicdRegister::ISENABLER0.0 + 8 * 4);
            assert_eq!(distributor.read32(unimplemented_enable), Some(0));
            assert!(distributor.write32(unimplemented_enable, u32::MAX));
            assert_eq!(distributor.read32(unimplemented_enable), Some(0));

            // Aff3 selects the second redistributor rather than the
            // Aff3 == 0 redistributor for a deliverable Group 1 SPI.
            {
                let mut state = distributor.state.lock();
                state.enable_grp1_non_secure = true;
                state.enable[TEST_SPI as usize / 32] |= 1 << (TEST_SPI & 31);
                state.group_status[TEST_SPI as usize / 32] |= 1 << (TEST_SPI & 31);
                state.route[TEST_SPI as usize] = 1 << 32;
            }
            assert_eq!(distributor.set_spi_irq(TEST_SPI, true), [VpIndex::new(1)]);
            assert_eq!(
                reserve_pending_spi(&distributor, VpIndex::new(1))
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );
        }

        fn write_gicr_sgi(distributor: &Distributor, register: u16, value: u32) {
            let address = aarch64defs::GIC_DISTRIBUTOR_SIZE
                + aarch64defs::GIC_REDISTRIBUTOR_FRAME_SIZE
                + u64::from(register);
            assert!(distributor.write(address, &value.to_ne_bytes()));
        }

        fn set_private_priority(distributor: &Distributor, intid: u32, priority: u8) {
            let register = GicrSgiRegister::IPRIORITYR0.0 + (intid / 4 * 4) as u16;
            let value = u32::from(priority) << ((intid % 4) * 8);
            write_gicr_sgi(distributor, register, value);
        }

        fn reserve_pending_spi(
            distributor: &Distributor,
            vp: VpIndex,
        ) -> Option<PendingInterrupt> {
            distributor
                .reserve_pending_spi_interrupts(vp, u8::MAX)?
                .remove(&u64::from(TEST_SPI))
        }

        #[test]
        fn level_spi_is_redelivered_until_deasserted() {
            let distributor = test_distributor();
            let vp = VpIndex::new(0);

            // A newly asserted level-sensitive line wakes its target VP and
            // makes the SPI eligible for injection.
            assert_eq!(distributor.set_spi_irq(TEST_SPI, true), [vp]);
            assert_eq!(
                reserve_pending_spi(&distributor, vp)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );

            // Once the SPI is represented in an LR, do not select it again
            // while that delivery remains in flight.
            distributor.mark_spi_injected(vp, TEST_SPI);
            assert!(
                reserve_pending_spi(&distributor, vp)
                    .is_none()
            );

            // Reporting the INTID as still present in an LR preserves its
            // in-flight state and continues to suppress duplicate injection.
            distributor.retain_in_flight_spis(vp, |intid| intid == TEST_SPI);
            assert!(
                reserve_pending_spi(&distributor, vp)
                    .is_none()
            );

            // Simulate guest EOI followed by LR retirement. The device line
            // is still asserted, so the level-sensitive SPI is eligible again.
            distributor.retain_in_flight_spis(vp, |_| false);
            assert_eq!(
                reserve_pending_spi(&distributor, vp)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );

            // After the second injection, the device deasserts its line. Once
            // that LR retires, the SPI must not be delivered again.
            distributor.mark_spi_injected(vp, TEST_SPI);
            assert!(distributor.set_spi_irq(TEST_SPI, false).is_empty());
            distributor.retain_in_flight_spis(vp, |_| false);
            assert!(
                reserve_pending_spi(&distributor, vp)
                    .is_none()
            );
        }

        #[test]
        fn deasserting_line_does_not_clear_software_pending_spi() {
            let distributor = test_distributor();
            let vp = VpIndex::new(0);

            assert_eq!(distributor.set_pending(TEST_SPI, true), Some(0));
            assert!(distributor.set_spi_irq(TEST_SPI, false).is_empty());
            assert_eq!(
                reserve_pending_spi(&distributor, vp)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );
        }

        #[test]
        fn pending_word_summary_tracks_pending_and_asserted_spis() {
            let distributor = test_distributor();
            let word_mask = 1 << (TEST_SPI / 32);

            assert_eq!(distributor.state.lock().pending_word_summary & word_mask, 0);
            assert_eq!(distributor.set_pending(TEST_SPI, true), Some(0));
            assert_ne!(distributor.state.lock().pending_word_summary & word_mask, 0);
            assert_eq!(distributor.set_pending(TEST_SPI, false), None);
            assert_eq!(distributor.state.lock().pending_word_summary & word_mask, 0);

            assert_eq!(distributor.set_spi_irq(TEST_SPI, true), [VpIndex::new(0)]);
            assert_ne!(distributor.state.lock().pending_word_summary & word_mask, 0);
            assert!(distributor.set_spi_irq(TEST_SPI, false).is_empty());
            assert_eq!(distributor.state.lock().pending_word_summary & word_mask, 0);
        }

        #[test]
        fn edge_spi_is_latched_until_injected() {
            let distributor = test_distributor();
            let vp = VpIndex::new(0);
            distributor.state.lock().cfg[2] |= 1 << 1;

            assert_eq!(distributor.set_spi_irq(TEST_SPI, true), [vp]);
            assert!(distributor.set_spi_irq(TEST_SPI, false).is_empty());
            assert_eq!(
                reserve_pending_spi(&distributor, vp)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );

            distributor.mark_spi_injected(vp, TEST_SPI);
            distributor.retain_in_flight_spis(vp, |_| false);
            assert!(
                reserve_pending_spi(&distributor, vp)
                    .is_none()
            );
        }

        #[test]
        fn irm_spi_reservation_is_exclusive_and_can_be_cancelled() {
            let mut distributor = test_distributor();
            let vp0 = VpIndex::new(0);
            let vp1 = VpIndex::new(1);
            distributor.add_redistributor(1, true);
            distributor.state.lock().route[TEST_SPI as usize] = 1 << 31;

            assert_eq!(distributor.set_spi_irq(TEST_SPI, true), [vp0, vp1]);
            assert_eq!(
                reserve_pending_spi(&distributor, vp0)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );
            assert_eq!(distributor.state.lock().in_flight_by_vp[0], [TEST_SPI]);
            assert!(
                reserve_pending_spi(&distributor, vp1)
                    .is_none()
            );

            distributor.cancel_spi_reservation(vp0, TEST_SPI);
            assert!(distributor.state.lock().in_flight_by_vp[0].is_empty());
            assert_eq!(
                reserve_pending_spi(&distributor, vp1)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );
        }

        #[test]
        fn returned_spi_state_updates_active_and_releases_ownership() {
            let distributor = test_distributor();
            let vp = VpIndex::new(0);

            assert_eq!(distributor.set_pending(TEST_SPI, true), Some(0));
            assert!(
                reserve_pending_spi(&distributor, vp)
                    .is_some()
            );
            distributor.fold_list_registers(
                vp,
                &[ListRegisterInterrupt {
                    intid: TEST_SPI,
                    pending: false,
                    active: true,
                }],
            );
            assert_ne!(distributor.state.lock().active[1] & 1, 0);

            assert_eq!(distributor.set_pending(TEST_SPI, true), Some(0));
            distributor.fold_list_registers(vp, &[]);
            let state = distributor.state.lock();
            assert_eq!(state.active[1] & 1, 0);
            assert!(state.in_flight_by_vp[0].is_empty());
            drop(state);
            assert_eq!(
                reserve_pending_spi(&distributor, vp)
                    .map(|irq| irq.intid),
                Some(TEST_SPI)
            );
        }

        #[test]
        fn returned_private_state_updates_active_and_releases_ownership() {
            const TEST_PPI: u32 = 27;
            let distributor = test_distributor();
            let vp = VpIndex::new(0);
            write_gicr_sgi(&distributor, GicrSgiRegister::IGROUPR0.0, 1 << TEST_PPI);
            write_gicr_sgi(&distributor, GicrSgiRegister::ISENABLER0.0, 1 << TEST_PPI);
            let gicr = &distributor.gicr[0];

            assert!(gicr.raise(TEST_PPI));
            distributor.mark_private_injected(vp, TEST_PPI);
            distributor.fold_list_registers(
                vp,
                &[ListRegisterInterrupt {
                    intid: TEST_PPI,
                    pending: false,
                    active: true,
                }],
            );
            assert!(distributor.next_private_interrupts(vp, u8::MAX).is_none());

            assert!(gicr.raise(TEST_PPI));
            distributor.fold_list_registers(vp, &[]);
            let interrupts = distributor.next_private_interrupts(vp, u8::MAX).unwrap();
            assert_eq!(interrupts.get(&u64::from(TEST_PPI)).map(|irq| irq.intid), Some(TEST_PPI));
        }

        #[test]
        fn private_interrupts_obey_enable_and_priority() {
            const TEST_SGI: u32 = 5;
            const TEST_PPI: u32 = 20;

            let distributor = test_distributor();
            let vp = VpIndex::new(0);
            let both_pending = (1 << TEST_SGI) | (1 << TEST_PPI);
            let gicr = &distributor.gicr[0];
            assert!(gicr.raise(TEST_SGI));
            assert!(gicr.raise(TEST_PPI));

            // Pending private interrupts are not deliverable until their
            // redistributor enable and Group 1 bits are both set.
            assert!(distributor.next_private_interrupts(vp, u8::MAX).is_none());
            write_gicr_sgi(&distributor, GicrSgiRegister::IGROUPR0.0, both_pending);
            assert!(distributor.next_private_interrupts(vp, u8::MAX).is_none());
            write_gicr_sgi(&distributor, GicrSgiRegister::ISENABLER0.0, both_pending);

            set_private_priority(&distributor, TEST_SGI, 0x40);
            set_private_priority(&distributor, TEST_PPI, 0x80);

            assert!(distributor.next_private_interrupts(vp, 0x40).is_none());
            let interrupts = distributor.next_private_interrupts(vp, 0x41).unwrap();
            let interrupt = interrupts.get(&u64::from(TEST_SGI)).unwrap();
            assert_eq!((interrupt.intid, interrupt.priority), (TEST_SGI, 0x40));
            assert_eq!(interrupts.len(), 1);

            distributor.clear_pending(vp.index() as usize, TEST_SGI);
            assert!(distributor.next_private_interrupts(vp, 0x80).is_none());
            let interrupts = distributor.next_private_interrupts(vp, 0x81).unwrap();
            let interrupt = interrupts.get(&u64::from(TEST_PPI)).unwrap();
            assert_eq!((interrupt.intid, interrupt.priority), (TEST_PPI, 0x80));
            assert_eq!(interrupts.len(), 1);

            distributor.state.lock().enable_grp1_non_secure = false;
            assert!(distributor.next_private_interrupts(vp, u8::MAX).is_none());
        }
    }
}

mod gicr {
    use super::ListRegisterInterrupt;
    use super::PendingInterrupt;
    use aarch64defs::MpidrEl1;
    use aarch64defs::gic::GicrCtlr;
    use aarch64defs::gic::GicrRdRegister;
    use aarch64defs::gic::GicrSgiRegister;
    use aarch64defs::gic::GicrTyper;
    use aarch64defs::gic::GicrWaker;
    use inspect::Inspect;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use super::gicd::DistributorState;

    #[derive(Debug, Inspect)]
    pub struct Redistributor {
        #[inspect(flatten)]
        pub(super) shared: Arc<SharedState>,
        pub(super) index: usize,
    }

    #[derive(Debug, Inspect)]
    pub(crate) struct SharedState {
        pub(super) pending: AtomicU32,
        pub(super) in_flight: AtomicU32,
        pub(super) ppi_level_line: AtomicU32,
        pub(super) ppi_clearing_latch: AtomicU32,
        pub(super) ppi_pending_latch: AtomicU32,
        #[inspect(with = "|&x| u64::from(x)")]
        pub(super) mpidr: MpidrEl1,
        last: bool,
        distributor_state: Arc<Mutex<DistributorState>>,
        mutable: Mutex<SharedMutState>,
    }

    #[derive(Debug, Inspect)]
    struct SharedMutState {
        #[inspect(hex)]
        active: u32,
        #[inspect(hex)]
        group_status: u32,
        #[inspect(hex)]
        group_modifier: u32,
        #[inspect(hex)]
        enable: u32,
        #[inspect(hex)]
        ppi_cfg: u32,
        #[inspect(iter_by_index)]
        priority: [u32; 8],
        sleep: bool,
    }

    impl SharedState {
        pub(crate) fn next_private_interrupts(
            &self,
            running_priority: u8,
        ) -> Option<HashMap<u64, PendingInterrupt>> {
            let pending = self.pending.load(Ordering::Relaxed);
            self.select_private_interrupts(pending, running_priority)
        }

        pub(crate) fn select_private_interrupts(
            &self,
            pending: u32,
            running_priority: u8,
        ) -> Option<HashMap<u64, PendingInterrupt>> {
            let state = self.mutable.lock();
            let in_flight = self.in_flight.load(Ordering::Relaxed);
            let mut group = 0;
            if self.distributor_state.lock().enable_grp0 {
                group |= !state.group_modifier & !state.group_status;
            }
            if self.distributor_state.lock().enable_grp1_non_secure {
                // modified bit: 0b1, status bit: 0b1, Reserved, treated as Non-secure Group 1
                group |= state.group_status;
            }
            if self.distributor_state.lock().enable_grp1_secure {
                group |= state.group_modifier & !state.group_status;
            }

            let deliverable = pending & state.enable & group & (!state.active | in_flight);

            // Too make it active+pending the deliverable mask above is wrong/too strict
            // let repend = pending & state.enable & group & state.active & in_flight;

            let mut interrupts: HashMap<u64, PendingInterrupt> = HashMap::new();
            for intid in 0..32 {
                let group1 = (state.group_modifier | state.group_status) & (1 << intid) != 0;
                if deliverable & (1 << intid) == 0 {
                    continue;
                }

                /*
                intid 0..3   -> priority[0]
                intid 4..7   -> priority[1]
                intid 8..11  -> priority[2]
                */
                let word = state.priority[(intid / 4) as usize];
                let priority = ((word >> ((intid % 4) * 8)) & 0xff) as u8;
                if priority >= running_priority {
                    continue;
                }

                interrupts.insert(
                    intid as u64,
                    PendingInterrupt {
                        intid,
                        priority,
                        group1,
                    },
                );

            }

            if interrupts.is_empty() {
                None
            } else {
                Some(interrupts)
            }
        }

        pub fn raise(&self, intid: u32) -> bool {
            let mask = 1 << intid;
            self.pending.fetch_or(mask, Ordering::Relaxed) & mask == 0
        }

        pub fn pend_ppi(&self, asserted: bool, intid: u32) -> bool {
            let mask = 1 << intid;
            let state = self.mutable.lock();
            let edge_triggered = state.ppi_cfg & mask != 0;

            if edge_triggered {
                return true;
            }

            if asserted {
                self.ppi_level_line.fetch_or(mask, Ordering::Relaxed);
            } else {
                self.ppi_level_line.fetch_and(!mask, Ordering::Relaxed);
            }

            asserted
        }

        pub fn get_latches(&self) -> (u32, u32) {
            let clearing_latch = self.ppi_clearing_latch.swap(0, Ordering::Relaxed);
            let pending_latch = self.ppi_pending_latch.swap(0, Ordering::Relaxed);
            (clearing_latch, pending_latch)
        }

        pub fn mark_injected(&self, intid: u32) {
            let mask = 1 << intid;
            self.pending.fetch_and(!mask, Ordering::Relaxed);
            self.in_flight.fetch_or(mask, Ordering::Relaxed);
        }

        pub fn fold_list_registers(&self, lrs: &[ListRegisterInterrupt]) {
            let mut state = self.mutable.lock();
            let mut in_flight = self.in_flight.load(Ordering::Relaxed);
            while in_flight != 0 {
                let intid = in_flight.trailing_zeros();
                let mask = 1 << intid;
                in_flight &= !mask;
                if let Some(lr) = lrs.iter().find(|lr| lr.intid == intid) {
                    if lr.active {
                        state.active |= mask;
                    } else {
                        state.active &= !mask;
                    }
                    if lr.pending {
                        self.pending.fetch_or(mask, Ordering::Relaxed);
                    }
                } else {
                    state.active &= !mask;
                    self.in_flight.fetch_and(!mask, Ordering::Relaxed);
                }
            }
        }

        pub fn read(&self, address: u64, data: &mut [u8]) {
            if address & (data.len() as u64 - 1) != 0 {
                data.fill(!0);
                tracing::warn!(address, ?data, "gicr read unaligned access");
                return;
            }

            if address & 0x10000 == 0 {
                let address = GicrRdRegister(address as u16);
                let handled = match data.len() {
                    4 => {
                        if let Some(v) = self.rd_read32(address) {
                            data.copy_from_slice(&v.to_ne_bytes());
                            true
                        } else {
                            false
                        }
                    }
                    8 => {
                        if let Some(v) = self.rd_read64(address) {
                            data.copy_from_slice(&v.to_ne_bytes());
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if !handled {
                    data.fill(0);
                    tracelimit::warn_ratelimited!(?address, "unsupported gicr rd register read");
                }
            } else {
                let address = GicrSgiRegister(address as u16);
                let handled = match data.len() {
                    1 => {
                        if let Some(v) = self.sgi_read8(address) {
                            data[0] = v;
                            true
                        } else {
                            false
                        }
                    }
                    4 => {
                        if let Some(v) = self.sgi_read32(address) {
                            data.copy_from_slice(&v.to_ne_bytes());
                            true
                        } else {
                            false
                        }
                    }
                    _ => false,
                };
                if !handled {
                    data.fill(0);
                    tracelimit::warn_ratelimited!(
                        ?address,
                        ?data,
                        "unsupported gicr sgi register read"
                    );
                }
            }
        }

        pub fn write(&self, address: u64, data: &[u8]) {
            if address & (data.len() as u64 - 1) != 0 {
                tracing::warn!(address, ?data, "gicr write unaligned access");
                return;
            }

            if address & 0x10000 == 0 {
                let address = GicrRdRegister(address as u16);
                let handled = match data.len() {
                    4 => {
                        let data = u32::from_ne_bytes(data.try_into().unwrap());
                        self.rd_write32(address, data)
                    }
                    8 => {
                        let data = u64::from_ne_bytes(data.try_into().unwrap());
                        self.rd_write64(address, data)
                    }
                    _ => false,
                };
                if !handled {
                    tracelimit::warn_ratelimited!(
                        ?address,
                        ?data,
                        "unsupported gicr rd register write"
                    );
                }
            } else {
                let address = GicrSgiRegister(address as u16);
                let handled = match data.len() {
                    1 => {
                        let data = u8::from_ne_bytes(data.try_into().unwrap());
                        self.sgi_write8(address, data)
                    }
                    4 => {
                        let data = u32::from_ne_bytes(data.try_into().unwrap());
                        self.sgi_write32(address, data)
                    }
                    _ => false,
                };
                if !handled {
                    tracelimit::warn_ratelimited!(
                        ?address,
                        ?data,
                        "unsupported gicr sgi register write"
                    );
                }
            }
        }

        fn rd_read32(&self, address: GicrRdRegister) -> Option<u32> {
            let v = match address {
                GicrRdRegister::PIDR2 => {
                    // GICv3
                    3 << 4
                }
                GicrRdRegister::CTLR => GicrCtlr::new().into(),
                GicrRdRegister::WAKER => {
                    let sleep = self.mutable.lock().sleep;
                    GicrWaker::new()
                        .with_processor_sleep(sleep)
                        .with_children_asleep(sleep)
                        .into()
                }
                _ => return None,
            };
            tracing::debug!(?address, v, "gicr rd read32");
            Some(v)
        }

        fn rd_write32(&self, address: GicrRdRegister, data: u32) -> bool {
            match address {
                GicrRdRegister::CTLR => {}
                GicrRdRegister::WAKER => {
                    let v = GicrWaker::from(data);
                    self.mutable.lock().sleep = v.processor_sleep();
                }
                _ => return false,
            }
            tracing::debug!(?address, data, "gicr rd write32");
            true
        }

        fn rd_read64(&self, address: GicrRdRegister) -> Option<u64> {
            let v = match address {
                GicrRdRegister::TYPER => GicrTyper::new()
                    .with_aff0(self.mpidr.aff0())
                    .with_aff1(self.mpidr.aff1())
                    .with_aff2(self.mpidr.aff2())
                    .with_aff3(self.mpidr.aff3())
                    .with_last(self.last)
                    .into(),
                _ => return None,
            };
            Some(v)
        }

        fn rd_write64(&self, _address: GicrRdRegister, _data: u64) -> bool {
            false
        }

        fn sgi_read8(&self, address: GicrSgiRegister) -> Option<u8> {
            let j = address.0%4;
            match address {
                r if GicrSgiRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x1f) / 4;
                    Some((self.mutable.lock().priority[n as usize] >> (j*8) & 0xff) as u8)
                }
                _ => None,
            }
        }

        fn sgi_write8(&self, address: GicrSgiRegister, data: u8) -> bool {
            let j = address.0%4;
            match address {
                r if GicrSgiRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x1f) / 4;
                    let mut state = self.mutable.lock();
                    let priority = &mut state.priority[n as usize];
                    *priority = (*priority & !(0xff << (j * 8))) | (u32::from(data) << (j * 8));
                    true
                }
                _ => false,
            }
        }

        fn sgi_read32(&self, address: GicrSgiRegister) -> Option<u32> {
            let v = match address {
                GicrSgiRegister::IGROUPR0 => self.mutable.lock().group_status,
                GicrSgiRegister::ICACTIVER0 | GicrSgiRegister::ISACTIVER0 => {
                    self.mutable.lock().active
                }
                GicrSgiRegister::ICENABLER0 | GicrSgiRegister::ISENABLER0 => {
                    self.mutable.lock().enable
                }
                GicrSgiRegister::ICPENDR0 | GicrSgiRegister::ISPENDR0 => {
                    self.pending.load(Ordering::Relaxed)
                }
                GicrSgiRegister::ICFGR0 => {
                    // SGIs are always edge triggered.
                    0xaaaaaaaa
                }
                GicrSgiRegister::ICFGR1 => self.mutable.lock().ppi_cfg,
                r if GicrSgiRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x1f) / 4;
                    self.mutable.lock().priority[n as usize]
                }
                GicrSgiRegister::IGRPMODR0 => self.mutable.lock().group_modifier,
                _ => return None,
            };
            tracing::debug!(?address, v, "gicr sgi read32");
            Some(v)
        }

        fn sgi_write32(&self, address: GicrSgiRegister, data: u32) -> bool {
            match address {
                GicrSgiRegister::IGROUPR0 => self.mutable.lock().group_status = data,
                GicrSgiRegister::ISACTIVER0 => self.mutable.lock().active |= data,
                GicrSgiRegister::ICACTIVER0 => self.mutable.lock().active &= !data,
                GicrSgiRegister::ISENABLER0 => self.mutable.lock().enable |= data,
                GicrSgiRegister::ICENABLER0 => self.mutable.lock().enable &= !data,
                GicrSgiRegister::ICPENDR0 => {
                    self.clear_pending_icpendr0(data);
                    // need to change the LR pending to inactive
                    // Or change from active + pending tp active
                    self.ppi_clearing_latch.fetch_or(data, Ordering::Relaxed);
                }
                GicrSgiRegister::ISPENDR0 => {
                    self.pending.fetch_or(data, Ordering::Relaxed);
                    // from inactive to pending
                    // from active to active+pending
                    // just make pending and then in get next interrupt selection it will be handled
                    // self.ppi_pending_latch.fetch_or(data, Ordering::Relaxed);
                }
                GicrSgiRegister::ICFGR0 => {
                    // Cannot change trigger mode for SGIs.
                }
                GicrSgiRegister::ICFGR1 => self.mutable.lock().ppi_cfg = data,
                r if GicrSgiRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x1f) / 4;
                    self.mutable.lock().priority[n as usize] = data;
                }
                GicrSgiRegister::IGRPMODR0 => self.mutable.lock().group_modifier = data,
                _ => return false,
            }
            tracing::debug!(?address, data, "gicr sgi write32");
            true
        }

        pub fn clear_pending(&self, intid: u32) {
            debug_assert!(intid < 32);

            self.pending.fetch_and(!(1 << intid), Ordering::Relaxed);
        }

        pub fn clear_pending_icpendr0(&self, data: u32) {
            let state = self.mutable.lock();

            for intid in 0..32 {
                if data & (1 << intid) == 0 {
                    continue;
                }
                let edge_triggered = state.ppi_cfg & (1 << intid) != 0;
                if edge_triggered {
                    self.clear_pending(intid);
                } else {
                    let asserted = self.ppi_level_line.load(Ordering::Relaxed) & (1 << intid) != 0;
                    if !asserted {
                        self.clear_pending(intid);
                    }
                }
            }
        }
    }

    impl Redistributor {
        pub(crate) fn new(index: usize, mpidr: u64, last: bool, distributor_state: Arc<Mutex<DistributorState>>) -> (Self, Arc<SharedState>) {
            let shared = Arc::new(SharedState {
                pending: AtomicU32::new(0),
                in_flight: AtomicU32::new(0),
                ppi_level_line: AtomicU32::new(0),
                ppi_clearing_latch: AtomicU32::new(0),
                ppi_pending_latch: AtomicU32::new(0),
                mpidr: mpidr.into(),
                last,
                distributor_state: distributor_state,
                mutable: Mutex::new(SharedMutState {
                    active: 0,
                    group_status: 0,
                    group_modifier: 0,
                    enable: 0,
                    ppi_cfg: 0,
                    priority: [0; 8],
                    sleep: false,
                }),
            });
            (
                Self {
                    index,
                    shared: shared.clone(),
                },
                shared,
            )
        }

        pub fn raise(&mut self, intid: u32) {
            self.shared.pending.fetch_or(1 << intid, Ordering::Relaxed);
        }

        pub(crate) fn irq_pending(&self) -> bool {
            let pending = self.shared.pending.load(Ordering::Relaxed);
            if pending == 0 {
                return false;
            }
            let state = self.shared.mutable.lock();
            (pending & !state.active & state.enable & state.group_status & !state.group_modifier) != 0
        }

        pub fn is_pending_or_active(&self, intid: u32) -> bool {
            let state = self.shared.mutable.lock();
            (self.shared.pending.load(Ordering::Relaxed) | state.active) & (1 << intid) != 0
        }

        pub(crate) fn ack(&mut self, _group1: bool) -> Option<u32> {
            let pending = self.shared.pending.load(Ordering::Relaxed);
            if pending == 0 {
                None
            } else {
                let mut state = self.shared.mutable.lock();
                let intid = 31 - (pending & !state.active).leading_zeros();
                tracing::trace!(intid, "ack");
                self.shared
                    .pending
                    .fetch_and(!(1 << intid), Ordering::Relaxed);
                state.active |= 1 << intid;
                Some(intid)
            }
        }

        pub(crate) fn eoi(&mut self, _group1: bool, intid: u32) {
            assert!(intid < 32);
            tracing::trace!(intid, "eoi");
            self.shared.mutable.lock().active &= !(1 << intid);
        }
    }
}
