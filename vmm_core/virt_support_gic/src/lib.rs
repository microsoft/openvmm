// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A very incomplete implementation of ARM GICv3.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub use gicd::Distributor;
pub use gicr::Redistributor;

#[derive(Clone, Copy, Debug)]
pub struct PendingInterrupt {
    pub intid: u32,
    pub priority: u8,
    pub group1: bool,
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

pub struct GicV3Model {
    distributor: Distributor,
    redistributors: Vec<Mutex<Redistributor>>,
    distributor_range: MemoryRange,
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

    pub fn mark_spi_injected(&self, vp: VpIndex, intid: u32) {
        self.distributor.mark_spi_injected(vp, intid);
    }

    pub fn cancel_spi_reservation(&self, vp: VpIndex, intid: u32) {
        self.distributor.cancel_spi_reservation(vp, intid);
    }

    pub fn retain_in_flight_spis(&self, vp: VpIndex, is_in_flight: impl FnMut(u32) -> bool) {
        self.distributor.retain_in_flight_spis(vp, is_in_flight);
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

    pub fn next_pending_interrupt(
        &self,
        vp: VpIndex,
        running_priority: u8,
    ) -> Option<PendingInterrupt> {
        self.distributor
            .next_pending_interrupt(vp, running_priority)
    }

    pub fn next_pending_private_interrupt(
        &self,
        vp: VpIndex,
        running_priority: u8,
    ) -> Option<PendingInterrupt> {
        self.distributor.next_private_interrupt(vp, running_priority)
    }

    pub fn next_pending_spi_interrupt(
        &self,
        vp: VpIndex,
        running_priority: u8,
    ) -> Option<PendingInterrupt> {
        self.distributor
            .next_pending_spi_interrupt(vp, running_priority)
    }

    pub fn reserve_pending_spi_interrupt(
        &self,
        vp: VpIndex,
        running_priority: u8,
    ) -> Option<PendingInterrupt> {
        self.distributor
            .reserve_pending_spi_interrupt(vp, running_priority)
    }

    pub fn raise_ppi(&self, vp: VpIndex, intid: u32) -> bool {
        self.distributor.raise_ppi(vp, intid)
    }

    pub fn complete_interrupt(&self, vp: VpIndex, intid: u32) {
        self.distributor.clear_pending(vp.index() as usize, intid);
    }
}

mod gicd {
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

    #[derive(Debug, Inspect)]
    pub struct Distributor {
        state: Mutex<DistributorState>,
        max_spi_intid: u32,
        #[inspect(skip)]
        gicr: Vec<Arc<SharedState>>,
        gicd_range: MemoryRange,
        gicr_range: MemoryRange,
    }

    #[derive(Debug, Inspect)]
    struct DistributorState {
        /// Level-triggered SPI input lines currently asserted by devices.
        #[inspect(iter_by_index)]
        asserted: Vec<u32>,
        #[inspect(iter_by_index)]
        pending: Vec<u32>,
        /// Bitmap of 32-INTID words that contain pending or asserted SPIs.
        pending_word_summary: u32,
        /// SPIs reserved for injection or represented in a CCA GIC list
        /// register, indexed by INTID.
        #[inspect(iter_by_index)]
        in_flight: Vec<Option<u32>>,
        /// Reserved or in-flight SPI INTIDs owned by each VP.
        #[inspect(skip)]
        in_flight_by_vp: Vec<Vec<u32>>,
        #[inspect(iter_by_index)]
        active: Vec<u32>,
        #[inspect(iter_by_index)]
        group: Vec<u32>,
        #[inspect(iter_by_index)]
        enable: Vec<u32>,
        #[inspect(iter_by_index)]
        cfg: Vec<u32>,
        #[inspect(iter_by_index)]
        priority: Vec<u32>,
        #[inspect(iter_by_index)]
        route: Vec<u64>,
        enable_grp0: bool,
        enable_grp1: bool,
    }

    impl Distributor {
        pub fn new(gicd_base: u64, gicr_range: MemoryRange, interrupt_count: u32) -> Self {
            let n = interrupt_count.div_ceil(32) as usize;
            assert!(n <= u32::BITS as usize);
            Self {
                state: Mutex::new(DistributorState {
                    asserted: vec![0; n],
                    pending: vec![0; n],
                    pending_word_summary: 0,
                    in_flight: vec![None; n * 32],
                    in_flight_by_vp: Vec::new(),
                    active: vec![0; n],
                    group: vec![0; n],
                    enable: vec![0; n],
                    cfg: vec![0; n * 2],
                    priority: vec![0; n * 8],
                    route: vec![0; n * 64],
                    enable_grp0: false,
                    enable_grp1: false,
                }),
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
            let (gicr, state) = Redistributor::new(self.gicr.len(), mpidr, last);
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

        pub fn mark_spi_injected(&self, vp: VpIndex, intid: u32) {
            if intid < 32 || intid > self.max_spi_intid {
                tracelimit::warn_ratelimited!(intid, "invalid injected GIC SPI");
                return;
            }

            let mut state = self.state.lock();
            Self::set_pending_locked(&mut state, intid, false);
            Self::set_in_flight_locked(&mut state, vp, intid);
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

        pub fn retain_in_flight_spis(
            &self,
            vp: VpIndex,
            mut is_in_flight: impl FnMut(u32) -> bool,
        ) {
            let mut state = self.state.lock();
            let vp_index = vp.index() as usize;
            let DistributorState {
                in_flight,
                in_flight_by_vp,
                ..
            } = &mut *state;
            let Some(owned) = in_flight_by_vp.get_mut(vp_index) else {
                return;
            };
            owned.retain(|intid| {
                let retain = is_in_flight(*intid);
                if !retain && in_flight[*intid as usize] == Some(vp.index()) {
                    in_flight[*intid as usize] = None;
                }
                retain
            });
        }

        pub fn next_pending_interrupt(
            &self,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            let private = self.next_private_interrupt(vp, running_priority);
            let spi = self.next_spi_interrupt(vp, running_priority);

            match (private, spi) {
                (Some(a), Some(b)) if interrupt_precedes(a, b) => Some(a),
                (Some(_), Some(b)) => Some(b),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            }
        }

        pub fn next_pending_private_interrupt(
            &self,
            vp: VpIndex,
            pending: u32,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            if !self.state.lock().enable_grp1 {
                return None;
            }

            self.gicr
                .get(vp.index() as usize)?
                .select_private_interrupt(pending, running_priority)
        }

        pub fn next_pending_spi_interrupt(
            &self,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            self.next_spi_interrupt(vp, running_priority)
        }

        pub fn reserve_pending_spi_interrupt(
            &self,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            let mut state = self.state.lock();
            let interrupt = self.next_spi_interrupt_locked(&state, vp, running_priority)?;
            // Reserve while still holding the selection lock so another VP
            // cannot select the same IRM-routed SPI.
            Self::set_in_flight_locked(&mut state, vp, interrupt.intid);
            Some(interrupt)
        }

        pub fn next_private_interrupt(
            &self,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            if !self.state.lock().enable_grp1 {
                return None;
            }

            self.gicr
                .get(vp.index() as usize)?
                .next_private_interrupt(running_priority)
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

        fn next_spi_interrupt(
            &self,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            let state = self.state.lock();
            self.next_spi_interrupt_locked(&state, vp, running_priority)
        }

        fn next_spi_interrupt_locked(
            &self,
            state: &DistributorState,
            vp: VpIndex,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            if !state.enable_grp1 {
                return None;
            }

            let mut best = None;
            let mut ready_words = state.pending_word_summary & !1;
            while ready_words != 0 {
                let word = ready_words.trailing_zeros() as usize;
                ready_words &= ready_words - 1;
                let mut candidates = (state.pending[word] | state.asserted[word])
                    & state.enable[word]
                    & !state.active[word]
                    & state.group[word];
                while candidates != 0 {
                    let bit = candidates.trailing_zeros();
                    candidates &= candidates - 1;

                    let intid = word as u32 * 32 + bit;
                    let mask = 1 << bit;
                    let pending = state.pending[word] & mask != 0;
                    let level_asserted =
                        state.asserted[word] & mask != 0 && !Self::edge_triggered(&state, intid);
                    if !pending && !level_asserted {
                        continue;
                    }
                    if intid > self.max_spi_intid
                        || state.in_flight[intid as usize].is_some()
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
                        group1: true,
                    };
                    if best.is_none_or(|current| interrupt_precedes(interrupt, current)) {
                        best = Some(interrupt);
                    }
                }
            }

            best
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
                        && (1 << gicr.mpidr.aff0()) & value.target_list() != 0)
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

        fn write32(&self, address: GicdRegister, value: u32) -> bool {
            assert!(address.0 & 3 == 0);
            match address {
                GicdRegister::CTLR => {
                    let ctlr = GicdCtlr::from(value);
                    let mut state = self.state.lock();
                    let state = &mut *state;
                    state.enable_grp0 = ctlr.enable_grp0();
                    state.enable_grp1 = ctlr.enable_grp1();
                }
                r if GicdRegister::IGROUPR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    if n != 0 {
                        if let Some(group) = self.state.lock().group.get_mut(n as usize) {
                            *group = value;
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
                GicdRegister::TYPER => GicdTyper::new()
                    .with_it_lines_number(31)
                    .with_id_bits(5)
                    .into(),
                GicdRegister::IIDR => 0,
                GicdRegister::TYPER2 => GicdTyper2::new().into(),
                GicdRegister::CTLR => {
                    let state = self.state.lock();
                    GicdCtlr::new()
                        .with_enable_grp0(state.enable_grp0)
                        .with_enable_grp1(state.enable_grp1)
                        .with_ds(true)
                        .with_are(true)
                        .into()
                }
                r if GicdRegister::IGROUPR.contains(&r.0) => {
                    let n = (r.0 & 0x7f) / 4;
                    self.state
                        .lock()
                        .group
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
                4 => self.write32(address, u32::from_ne_bytes(data.try_into().unwrap())),
                8 => self.write64(address, u64::from_ne_bytes(data.try_into().unwrap())),
                _ => false,
            };
            if !handled {
                tracelimit::warn_ratelimited!(?address, ?data, "unsupported gicd register write");
            }
        }
    }

    fn interrupt_precedes(a: PendingInterrupt, b: PendingInterrupt) -> bool {
        a.priority < b.priority || (a.priority == b.priority && a.intid < b.intid)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
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
            state.enable_grp1 = true;
            state.enable[1] |= 1;
            state.group[1] |= 1;
            drop(state);

            distributor
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

        #[test]
        fn level_spi_is_redelivered_until_deasserted() {
            let distributor = test_distributor();
            let vp = VpIndex::new(0);

            // A newly asserted level-sensitive line wakes its target VP and
            // makes the SPI eligible for injection.
            assert_eq!(distributor.set_spi_irq(TEST_SPI, true), [vp]);
            assert_eq!(
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );

            // Once the SPI is represented in an LR, do not select it again
            // while that delivery remains in flight.
            distributor.mark_spi_injected(vp, TEST_SPI);
            assert!(
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
                    .is_none()
            );

            // Reporting the INTID as still present in an LR preserves its
            // in-flight state and continues to suppress duplicate injection.
            distributor.retain_in_flight_spis(vp, |intid| intid == TEST_SPI);
            assert!(
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
                    .is_none()
            );

            // Simulate guest EOI followed by LR retirement. The device line
            // is still asserted, so the level-sensitive SPI is eligible again.
            distributor.retain_in_flight_spis(vp, |_| false);
            assert_eq!(
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );

            // After the second injection, the device deasserts its line. Once
            // that LR retires, the SPI must not be delivered again.
            distributor.mark_spi_injected(vp, TEST_SPI);
            assert!(distributor.set_spi_irq(TEST_SPI, false).is_empty());
            distributor.retain_in_flight_spis(vp, |_| false);
            assert!(
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
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
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
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
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );

            distributor.mark_spi_injected(vp, TEST_SPI);
            distributor.retain_in_flight_spis(vp, |_| false);
            assert!(
                distributor
                    .next_pending_spi_interrupt(vp, u8::MAX)
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
                distributor
                    .reserve_pending_spi_interrupt(vp0, u8::MAX)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );
            assert_eq!(distributor.state.lock().in_flight_by_vp[0], [TEST_SPI]);
            assert!(
                distributor
                    .reserve_pending_spi_interrupt(vp1, u8::MAX)
                    .is_none()
            );

            distributor.cancel_spi_reservation(vp0, TEST_SPI);
            assert!(distributor.state.lock().in_flight_by_vp[0].is_empty());
            assert_eq!(
                distributor
                    .reserve_pending_spi_interrupt(vp1, u8::MAX)
                    .map(|interrupt| interrupt.intid),
                Some(TEST_SPI)
            );
        }

        #[test]
        fn private_interrupts_obey_enable_and_priority() {
            const TEST_SGI: u32 = 5;
            const TEST_PPI: u32 = 20;

            let distributor = test_distributor();
            let vp = VpIndex::new(0);
            let both_pending = (1 << TEST_SGI) | (1 << TEST_PPI);

            // A requested private interrupt is not deliverable until its
            // redistributor enable and Group 1 bits are both set.
            assert!(
                distributor
                    .next_pending_private_interrupt(vp, both_pending, u8::MAX)
                    .is_none()
            );
            write_gicr_sgi(&distributor, GicrSgiRegister::IGROUPR0.0, both_pending);
            assert!(
                distributor
                    .next_pending_private_interrupt(vp, both_pending, u8::MAX)
                    .is_none()
            );
            write_gicr_sgi(&distributor, GicrSgiRegister::ISENABLER0.0, both_pending);

            set_private_priority(&distributor, TEST_SGI, 0x40);
            set_private_priority(&distributor, TEST_PPI, 0x80);

            // A priority equal to the PMR threshold is masked. Raising the
            // threshold admits the SGI and preserves its programmed priority.
            assert!(
                distributor
                    .next_pending_private_interrupt(vp, both_pending, 0x40)
                    .is_none()
            );
            let interrupt = distributor
                .next_pending_private_interrupt(vp, both_pending, 0x41)
                .unwrap();
            assert_eq!((interrupt.intid, interrupt.priority), (TEST_SGI, 0x40));

            // With only the PPI requested, its independently programmed
            // priority is subject to the same PMR threshold.
            let ppi_pending = 1 << TEST_PPI;
            assert!(
                distributor
                    .next_pending_private_interrupt(vp, ppi_pending, 0x80)
                    .is_none()
            );
            let interrupt = distributor
                .next_pending_private_interrupt(vp, ppi_pending, 0x81)
                .unwrap();
            assert_eq!((interrupt.intid, interrupt.priority), (TEST_PPI, 0x80));

            distributor.state.lock().enable_grp1 = false;
            assert!(
                distributor
                    .next_pending_private_interrupt(vp, both_pending, u8::MAX)
                    .is_none()
            );
        }
    }
}

mod gicr {
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
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    #[derive(Debug, Inspect)]
    pub struct Redistributor {
        #[inspect(flatten)]
        pub(super) shared: Arc<SharedState>,
        pub(super) index: usize,
    }

    #[derive(Debug, Inspect)]
    pub(crate) struct SharedState {
        pub(super) pending: AtomicU32,
        #[inspect(with = "|&x| u64::from(x)")]
        pub(super) mpidr: MpidrEl1,
        last: bool,
        mutable: Mutex<SharedMutState>,
    }

    #[derive(Debug, Inspect)]
    struct SharedMutState {
        #[inspect(hex)]
        active: u32,
        #[inspect(hex)]
        group: u32,
        #[inspect(hex)]
        enable: u32,
        #[inspect(hex)]
        ppi_cfg: u32,
        #[inspect(iter_by_index)]
        priority: [u32; 8],
        sleep: bool,
    }

    impl SharedState {
        pub(crate) fn next_private_interrupt(
            &self,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            let pending = self.pending.load(Ordering::Relaxed);
            self.select_private_interrupt(pending, running_priority)
        }

        pub(crate) fn select_private_interrupt(
            &self,
            pending: u32,
            running_priority: u8,
        ) -> Option<PendingInterrupt> {
            let state = self.mutable.lock();
            let deliverable = pending & state.enable & !state.active & state.group;

            let mut best: Option<PendingInterrupt> = None;
            for intid in 0..32 {
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

                let interrupt = PendingInterrupt {
                    intid,
                    priority,
                    group1: true,
                };
                if best.is_none_or(|current| {
                    priority < current.priority
                        || (priority == current.priority && intid < current.intid)
                }) {
                    best = Some(interrupt);
                }
            }

            best
        }

        pub fn raise(&self, intid: u32) -> bool {
            let mask = 1 << intid;
            self.pending.fetch_or(mask, Ordering::Relaxed) & mask == 0
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

        fn sgi_read32(&self, address: GicrSgiRegister) -> Option<u32> {
            let v = match address {
                GicrSgiRegister::IGROUPR0 => self.mutable.lock().group,
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
                _ => return None,
            };
            tracing::debug!(?address, v, "gicr sgi read32");
            Some(v)
        }

        fn sgi_write32(&self, address: GicrSgiRegister, data: u32) -> bool {
            match address {
                GicrSgiRegister::IGROUPR0 => self.mutable.lock().group = data,
                GicrSgiRegister::ISACTIVER0 => self.mutable.lock().active |= data,
                GicrSgiRegister::ICACTIVER0 => self.mutable.lock().active &= !data,
                GicrSgiRegister::ISENABLER0 => self.mutable.lock().enable |= data,
                GicrSgiRegister::ICENABLER0 => self.mutable.lock().enable &= !data,
                GicrSgiRegister::ICFGR0 => {
                    // Cannot change trigger mode for SGIs.
                }
                GicrSgiRegister::ICFGR1 => self.mutable.lock().ppi_cfg = data,
                r if GicrSgiRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x1f) / 4;
                    self.mutable.lock().priority[n as usize] = data;
                }
                _ => return false,
            }
            tracing::debug!(?address, data, "gicr sgi write32");
            true
        }

        pub fn clear_pending(&self, intid: u32) {
            debug_assert!(intid < 32);

            self.pending
                .fetch_and(!(1 << intid), Ordering::Relaxed);
        }
    }

    impl Redistributor {
        pub(crate) fn new(index: usize, mpidr: u64, last: bool) -> (Self, Arc<SharedState>) {
            let shared = Arc::new(SharedState {
                pending: AtomicU32::new(0),
                mpidr: mpidr.into(),
                last,
                mutable: Mutex::new(SharedMutState {
                    active: 0,
                    group: 0,
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
            (pending & !state.active & state.enable & state.group) != 0
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
