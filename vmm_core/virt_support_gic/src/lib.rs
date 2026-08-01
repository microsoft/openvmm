// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A very incomplete implementation of ARM GICv3.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub use gicd::Distributor;
pub use gicr::Redistributor;

#[cfg(test)]
mod model_tests;

mod gicd {
    use super::Redistributor;
    use super::gicr::PRIORITY_WORD_MASK;
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
        #[inspect(iter_by_index)]
        pending: Vec<u32>,
        #[inspect(iter_by_index)]
        line_level: Vec<u32>,
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
        /// Creates a distributor with `spi_count` shared peripheral interrupts,
        /// in addition to the 32 private SGI and PPI INTIDs.
        ///
        /// Arm GIC Architecture Specification, Arm IHI 0069H.b:
        /// - §2.2.1, Table 2-1: SPI INTIDs are 32-1019; 1020-1023 are special.
        /// - §12.9.38, `GICD_TYPER.ITLinesNumber`: "maximum SPI INTID is
        ///   32(N+1) - 1".
        ///
        /// Normative consequence: at most 988 SPIs are accepted and TYPER must
        /// report the configured upper bound. Enforced by
        /// `gicd_typer_reports_configured_intids` and
        /// `configured_intid_limit_bounds_spi_delivery`.
        pub fn new(gicd_base: u64, gicr_range: MemoryRange, spi_count: u32) -> Self {
            assert!(spi_count <= 988);
            let intid_count = 32 + spi_count;
            let n = intid_count.div_ceil(32) as usize;
            Self {
                state: Mutex::new(DistributorState {
                    pending: vec![0; n],
                    line_level: vec![0; n],
                    active: vec![0; n],
                    group: vec![0; n],
                    enable: vec![0; n],
                    cfg: vec![0; n * 2],
                    priority: vec![0; n * 8],
                    route: vec![0; n * 64],
                    enable_grp0: false,
                    enable_grp1: false,
                }),
                max_spi_intid: intid_count - 1,
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

        /// Resolves a GICD_IROUTER value to a redistributor index. Affinity
        /// routing requires an exact MPIDR match; an unmatched affinity does not
        /// forward the interrupt. One-of-N routing uses the first eligible PE.
        ///
        /// Arm IHI 0069H.b §12.9.22, `GICD_IROUTER<n>`:
        /// - IRM=0: "Interrupts routed to the PE specified by a.b.c.d."
        /// - IRM=1: "Interrupts routed to any PE ... participating node."
        ///
        /// An unmatched IRM=0 affinity is CONSTRAINED UNPREDICTABLE; choosing
        /// "not forwarded" is one permitted behavior. Selecting the first
        /// awake, group-enabled PE for IRM=1 is a deterministic implementation
        /// policy within architectural latitude, not a fairness claim. Enforced by
        /// `spi_affinity_route_selects_the_target_pe`,
        /// `unmatched_spi_affinity_preserves_pending_without_forwarding`, and
        /// `one_of_n_spi_route_uses_a_deterministic_pe`.
        fn route_to_pe(&self, route: u64, group1: bool) -> Option<usize> {
            const IRM: u64 = 1 << 31;

            if route & IRM != 0 {
                return self
                    .gicr
                    .iter()
                    .position(|gicr| gicr.eligible_for_one_of_n(group1));
            }

            let aff0 = (route & 0xff) as u8;
            let aff1 = ((route >> 8) & 0xff) as u8;
            let aff2 = ((route >> 16) & 0xff) as u8;
            let aff3 = ((route >> 32) & 0xff) as u8;
            self.gicr.iter().position(|gicr| {
                gicr.mpidr.aff0() == aff0
                    && gicr.mpidr.aff1() == aff1
                    && gicr.mpidr.aff2() == aff2
                    && gicr.mpidr.aff3() == aff3
            })
        }

        pub fn set_pending(&self, intid: u32, pending: bool) -> Option<u32> {
            if !(32..=self.max_spi_intid).contains(&intid) {
                return None;
            }
            let mut state = self.state.lock();
            let word = intid as usize / 32;
            let mask = 1 << (intid & 31);
            if (state.line_level[word] & mask != 0) != pending {
                tracing::debug!(intid, pending, "set pending");
            }
            if pending {
                state.line_level[word] |= mask;
                state.pending[word] |= mask;
                let route = state.route.get(intid as usize).copied().unwrap_or(0);
                let group1 = state.group[word] & mask != 0;
                drop(state);
                self.route_to_pe(route, group1).map(|pe| pe as u32)
            } else {
                state.line_level[word] &= !mask;
                if !Self::edge_triggered(&state, intid) {
                    state.pending[word] &= !mask;
                }
                None
            }
        }

        fn edge_triggered(state: &DistributorState, intid: u32) -> bool {
            let field = (intid % 16) * 2 + 1;
            state.cfg[intid as usize / 16] & (1 << field) != 0
        }

        pub fn irq_pending(&self, gicr: &Redistributor) -> bool {
            self.irq_pending_for_group(gicr, false) || self.irq_pending_for_group(gicr, true)
        }

        pub fn irq_pending_for_group(&self, gicr: &Redistributor, group1: bool) -> bool {
            self.select(gicr, group1).is_some()
        }

        /// Distributor group gate.
        ///
        /// Arm IHI 0069H.b §4.7 and §12.9.4, `GICD_CTLR.EnableGrp*`:
        /// a pending interrupt in a disabled group "is not ... considered" for
        /// highest-priority selection. Pending state is retained while gated.
        /// Enforced by `distributor_group_enable_gates_delivery_without_losing_pending_state`.
        fn group_enabled(&self, group1: bool) -> bool {
            let state = self.state.lock();
            if group1 {
                state.enable_grp1
            } else {
                state.enable_grp0
            }
        }

        /// The globally highest-priority Group-`group1` interrupt deliverable to
        /// this PE right now: the best SGI/PPI on its redistributor combined with
        /// the best SPI routed to it, then gated by this PE's PMR and preemption
        /// state. Returns `(intid, group_priority)`.
        ///
        /// Checking only the single best candidate is sufficient: if the
        /// highest-priority pending interrupt cannot pass PMR/preemption, none of
        /// lower priority can either. The redistributor and distributor locks are
        /// taken in turn, never simultaneously (matching the existing pattern).
        ///
        /// Arm IHI 0069H.b §1.2.4, "Sufficient priority", requires comparison
        /// with `ICC_PMR_EL1`, the BPRs, and `ICC_RPR_EL1`. §12.2.14
        /// `ICC_IAR1_EL1` returns the highest-priority pending interrupt only if
        /// it has sufficient priority. Enforced by `pmr_masks_then_admits`,
        /// `selection_prefers_higher_priority_then_lower_intid`,
        /// `higher_priority_preempts_active`, and the independent model.
        fn select(&self, gicr: &Redistributor, group1: bool) -> Option<(u32, u8)> {
            if !self.group_enabled(group1) {
                return None;
            }
            let mut cand = gicr.best_candidate(group1);
            if let Some(spi) = self.best_spi(gicr.index, group1) {
                // Lowest priority byte wins; ties keep the lower intid (the
                // SGI/PPI, which is numerically below any SPI).
                cand = Some(match cand {
                    Some((i, p)) if p <= spi.1 => (i, p),
                    _ => spi,
                });
            }
            let (intid, pri) = cand?;
            let gp = gicr.admit(group1, pri)?;
            Some((intid, gp))
        }

        /// The best deliverable Group-`group1` SPI routed to `pe`: pending,
        /// inactive, enabled, and with the lowest priority byte (ties choose the
        /// lowest INTID).
        ///
        /// Arm IHI 0069H.b §4.8.2 defines the highest-priority pending
        /// interrupt by numerically lowest priority; §4.7 excludes disabled
        /// interrupts/groups. Lower-INTID tie-breaking is this implementation's
        /// deterministic policy. Enforced by
        /// `selection_prefers_higher_priority_then_lower_intid`.
        fn best_spi(&self, pe: usize, group1: bool) -> Option<(u32, u8)> {
            let state = self.state.lock();
            let mut best: Option<(u32, u8)> = None;
            for w in 1..state.pending.len() {
                let group = if group1 {
                    state.group[w]
                } else {
                    !state.group[w]
                };
                let mut deliverable = state.pending[w] & !state.active[w] & state.enable[w] & group;
                while deliverable != 0 {
                    let bit = deliverable.trailing_zeros();
                    deliverable &= deliverable - 1;
                    let intid = w as u32 * 32 + bit;
                    if intid > self.max_spi_intid {
                        continue;
                    }
                    let route = state.route.get(intid as usize).copied().unwrap_or(0);
                    if self.route_to_pe(route, group1) != Some(pe) {
                        continue;
                    }
                    let pri = state.priority[intid as usize / 4].to_ne_bytes()[intid as usize % 4];
                    best = Some(match best {
                        Some((bi, bp)) if bp <= pri => (bi, bp),
                        _ => (intid, pri),
                    });
                }
            }
            best
        }

        /// Acknowledges the selected interrupt and moves it to Active state.
        ///
        /// Arm IHI 0069H.b §12.2.14, `ICC_IAR1_EL1`: "This read acts as an
        /// acknowledge"; §4.1.2 Transition C moves an acknowledged edge
        /// interrupt from Pending to Active. §2.2.1 requires special INTID 1023
        /// when no pending interrupt has sufficient priority. Enforced by
        /// `distributor_group_enable_gates_nonzero_pe_acknowledge`,
        /// `spi_affinity_route_selects_the_target_pe`, and the independent
        /// model.
        ///
        /// §4.1.2 Transition D requires an asserted level-sensitive interrupt
        /// to become Active and Pending. `line_level` retains that assertion
        /// across acknowledge; deassertion clears the retained Pending state.
        /// Enforced by `level_spi_remains_pending_until_line_deasserts` and
        /// `edge_spi_latches_one_pending_assertion`.
        pub fn ack(&self, gicr: &mut Redistributor, group1: bool) -> u32 {
            let Some((intid, gp)) = self.select(gicr, group1) else {
                return 1023;
            };
            if intid < 32 {
                gicr.activate(intid);
            } else if !self.activate_spi(intid, gicr.index, group1) {
                return 1023;
            }
            gicr.push_priority(group1, gp);
            tracing::trace!(intid, "gic ack");
            intid
        }

        fn activate_spi(&self, intid: u32, pe: usize, group1: bool) -> bool {
            let mut state = self.state.lock();
            let word = intid as usize / 32;
            let mask = 1 << (intid % 32);
            let in_group = state.group[word] & mask != 0;
            let distributor_enabled = if group1 {
                state.enable_grp1
            } else {
                state.enable_grp0
            };
            if !distributor_enabled
                || state.pending[word] & !state.active[word] & state.enable[word] & mask == 0
                || in_group != group1
            {
                return false;
            }
            let route = state.route.get(intid as usize).copied().unwrap_or(0);
            if self.route_to_pe(route, group1) != Some(pe) {
                return false;
            }
            if Self::edge_triggered(&state, intid) || state.line_level[word] & mask == 0 {
                state.pending[word] &= !mask;
            }
            state.active[word] |= mask;
            true
        }

        pub fn write_sysreg(
            &self,
            gicr: &mut Redistributor,
            reg: SystemReg,
            value: u64,
            mut wake: impl FnMut(usize),
        ) -> bool {
            match reg {
                SystemReg::ICC_EOIR0_EL1 => {
                    let intid = value as u32;
                    let deactivates_spi = (32..1020).contains(&intid) && !gicr.eoimode();
                    self.eoi(gicr, false, intid);
                    if deactivates_spi {
                        for index in 0..self.gicr.len() {
                            wake(index);
                        }
                    }
                }
                SystemReg::ICC_EOIR1_EL1 => {
                    let intid = value as u32;
                    let deactivates_spi = (32..1020).contains(&intid) && !gicr.eoimode();
                    self.eoi(gicr, true, intid);
                    if deactivates_spi {
                        for index in 0..self.gicr.len() {
                            wake(index);
                        }
                    }
                }
                SystemReg::ICC_DIR_EL1 => {
                    let intid = value as u32;
                    let deactivates_spi = (32..1020).contains(&intid) && gicr.eoimode();
                    self.dir(gicr, intid);
                    if deactivates_spi {
                        for index in 0..self.gicr.len() {
                            wake(index);
                        }
                    }
                }
                SystemReg::ICC_SGI0R_EL1 => self.sgi(gicr, false, value, wake),
                SystemReg::ICC_SGI1R_EL1 => self.sgi(gicr, true, value, wake),
                _ => {
                    let handled = gicr.write_cpuif(reg, value);
                    if handled
                        && matches!(reg, SystemReg::ICC_IGRPEN0_EL1 | SystemReg::ICC_IGRPEN1_EL1)
                    {
                        for index in 0..self.gicr.len() {
                            wake(index);
                        }
                    }
                    return handled;
                }
            }
            true
        }

        fn sgi(
            &self,
            this: &mut Redistributor,
            group1: bool,
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
                    if gicr.is_group1(value.intid()) == group1 && gicr.raise(value.intid()) {
                        wake(index);
                    }
                }
            }
        }

        pub fn read_sysreg(&self, gicr: &mut Redistributor, reg: SystemReg) -> Option<u64> {
            let v = match reg {
                SystemReg::ICC_IAR0_EL1 => self.ack(gicr, false).into(),
                SystemReg::ICC_IAR1_EL1 => self.ack(gicr, true).into(),
                _ => return gicr.read_cpuif(reg),
            };
            Some(v)
        }

        /// Performs the EOIR priority drop and, in combined mode, deactivation.
        ///
        /// Arm IHI 0069H.b §12.2.10, `ICC_EOIR1_EL1`: EOImode=0 "drops the
        /// priority ... and also deactivates"; EOImode=1 "only drops the
        /// priority". Special INTIDs are ignored. Architecturally invalid,
        /// out-of-order EOIR writes are UNPREDICTABLE; this model assumes the
        /// required most-recent-IAR ordering. Enforced by
        /// `eoimode_split_completion_for_{sgi,spi}`,
        /// `special_intids_do_not_drop_or_deactivate`, and AP nesting tests.
        fn eoi(&self, gicr: &mut Redistributor, group1: bool, intid: u32) {
            if intid >= 1020 {
                return;
            }
            gicr.pop_priority(group1);
            tracing::trace!(intid, "gic eoi");
            if !gicr.eoimode() {
                self.deactivate_intid(gicr, intid);
            }
        }

        /// Deactivates an interrupt after a split priority drop.
        ///
        /// Arm IHI 0069H.b §12.2.8, `ICC_DIR_EL1`: "When interrupt priority
        /// drop is separated ... deactivates the specified interrupt." GICv3
        /// mandates ignoring DIR when EOImode=0. Enforced by
        /// `dir_is_ignored_when_eoimode_is_clear` and split-completion tests.
        fn dir(&self, gicr: &mut Redistributor, intid: u32) {
            if intid >= 1020 || !gicr.eoimode() {
                return;
            }
            tracing::trace!(intid, "gic dir");
            self.deactivate_intid(gicr, intid);
        }

        fn deactivate_intid(&self, gicr: &mut Redistributor, intid: u32) {
            if intid < 32 {
                gicr.deactivate(intid);
            } else if let Some(v) = self.state.lock().active.get_mut(intid as usize / 32) {
                *v &= !(1 << (intid & 31));
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
                            *priority = value & PRIORITY_WORD_MASK;
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
                    // Arm IHI 0069H.b §12.9.38:
                    // ITLinesNumber=N reports maximum SPI 32(N+1)-1.
                    .with_it_lines_number((self.max_spi_intid / 32) as u8)
                    .with_id_bits(15)
                    // Arm IHI 0069H.b §12.9.38 requires SecurityExtn to be
                    // RAZ when the single-security-state view reports DS=1.
                    .with_security_extn(false)
                    .into(),
                GicdRegister::IIDR => 0,
                GicdRegister::TYPER2 => GicdTyper2::new().into(),
                GicdRegister::CTLR => {
                    let state = self.state.lock();
                    // Hyper-V platform policy: expose one Security state and
                    // affinity routing permanently enabled. Arm IHI 0069H.b
                    // §12.9.4 defines DS/ARE; only group enables are writable
                    // in this model.
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

        /// Implements byte/halfword priority access only.
        ///
        /// Arm IHI 0069H.b §12.1.3 requires byte access to
        /// `GICD_IPRIORITYR<n>` and defines byte access to unlisted registers
        /// as unsupported/UNPREDICTABLE. Restricting read-modify-write to plain
        /// priority storage avoids corrupting set/clear register semantics.
        /// Enforced by `gicd_ipriorityr_supports_subword_access`.
        fn read_subword(&self, address: GicdRegister, data: &mut [u8]) -> bool {
            if !GicdRegister::IPRIORITYR.contains(&address.0) {
                return false;
            }
            let word = GicdRegister(address.0 & !0x3);
            let Some(value) = self.read32(word) else {
                return false;
            };
            let bytes = value.to_ne_bytes();
            let offset = (address.0 & 0x3) as usize;
            data.copy_from_slice(&bytes[offset..offset + data.len()]);
            true
        }

        fn write_subword(&self, address: GicdRegister, data: &[u8]) -> bool {
            if !GicdRegister::IPRIORITYR.contains(&address.0) {
                return false;
            }
            let word = GicdRegister(address.0 & !0x3);
            let Some(value) = self.read32(word) else {
                return false;
            };
            let mut bytes = value.to_ne_bytes();
            let offset = (address.0 & 0x3) as usize;
            bytes[offset..offset + data.len()].copy_from_slice(data);
            self.write32(word, u32::from_ne_bytes(bytes))
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
                1 | 2 => self.read_subword(address, data),
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
                1 | 2 => self.write_subword(address, data),
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
        use super::Distributor;
        use super::Redistributor;
        use aarch64defs::SystemReg;
        use aarch64defs::gic::GicdCtlr;
        use aarch64defs::gic::GicdRegister;
        use aarch64defs::gic::GicdTyper;
        use memory_range::MemoryRange;

        const GICD_BASE: u64 = 0x0800_0000;

        fn dist() -> Distributor {
            Distributor::new(GICD_BASE, MemoryRange::new(0x0808_0000..0x0810_0000), 960)
        }

        fn dist_with_redists(count: usize) -> (Distributor, Vec<Redistributor>) {
            let mut d = dist();
            let mut redists = (0..count)
                .map(|index| d.add_redistributor(index as u64, index + 1 == count))
                .collect::<Vec<_>>();
            for redist in &mut redists {
                redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
                redist.write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 1);
            }
            let ctlr: u32 = GicdCtlr::new().with_enable_grp1(true).into();
            d.write(GICD_BASE + GicdRegister::CTLR.0 as u64, &ctlr.to_ne_bytes());
            (d, redists)
        }

        #[test]
        fn gicd_typer_reports_configured_intids() {
            let d = dist();
            let mut value = [0; 4];

            d.read(GICD_BASE + GicdRegister::TYPER.0 as u64, &mut value);

            let typer = GicdTyper::from(u32::from_ne_bytes(value));
            assert_eq!(typer.it_lines_number(), 30);
            assert_eq!(typer.id_bits(), 15);
            assert!(!typer.security_extn());
        }

        #[test]
        fn configured_intid_limit_bounds_spi_delivery() {
            let d = Distributor::new(GICD_BASE, MemoryRange::new(0x0808_0000..0x0810_0000), 64);

            assert_eq!(d.set_pending(95, true), None);
            assert_eq!(d.state.lock().pending[2], 1 << 31);
            assert_eq!(d.set_pending(96, true), None);
            assert_eq!(d.set_pending(31, true), None);
        }

        fn provision_spi(d: &Distributor, intid: u32, priority: u8) {
            let mut state = d.state.lock();
            let word = intid as usize / 32;
            let bit = 1 << (intid % 32);
            state.enable[word] |= bit;
            state.group[word] |= bit;
            let priority_word = &mut state.priority[intid as usize / 4];
            let mut priorities = priority_word.to_ne_bytes();
            priorities[intid as usize % 4] = priority;
            *priority_word = u32::from_ne_bytes(priorities);
        }

        fn configure_edge(d: &Distributor, intid: u32) {
            let mut state = d.state.lock();
            state.cfg[intid as usize / 16] |= 2 << ((intid % 16) * 2);
        }

        #[test]
        fn level_spi_remains_pending_until_line_deasserts() {
            let (d, mut redists) = dist_with_redists(1);
            let redist = &mut redists[0];
            provision_spi(&d, 40, 0x20);
            route_spi(&d, 40, 0);

            d.set_pending(40, true);
            assert_eq!(d.ack(redist, true), 40);
            {
                let state = d.state.lock();
                assert_ne!(state.pending[1] & (1 << 8), 0);
                assert_ne!(state.active[1] & (1 << 8), 0);
            }

            d.eoi(redist, true, 40);
            assert_eq!(d.ack(redist, true), 40);
            d.set_pending(40, false);
            d.eoi(redist, true, 40);
            assert!(!d.irq_pending(redist));
        }

        #[test]
        fn edge_spi_latches_one_pending_assertion() {
            let (d, mut redists) = dist_with_redists(1);
            let redist = &mut redists[0];
            provision_spi(&d, 40, 0x20);
            configure_edge(&d, 40);
            route_spi(&d, 40, 0);

            d.set_pending(40, true);
            d.set_pending(40, false);
            assert_eq!(d.ack(redist, true), 40);
            d.eoi(redist, true, 40);
            assert!(!d.irq_pending(redist));
        }

        fn route_spi(d: &Distributor, intid: u32, route: u64) {
            let address = GICD_BASE + GicdRegister::IROUTER0.0 as u64 + u64::from(intid) * 8;
            d.write(address, &route.to_ne_bytes());
        }

        // Regression: GICD IPRIORITYR writes previously landed in the ICFGR
        // `cfg` array (a copy-paste defect) instead of `priority`, so SPI
        // priority writes were dropped on read-back AND corrupted interrupt
        // configuration. They must round-trip through `priority` and leave the
        // colliding `cfg` word untouched.
        #[test]
        fn gicd_ipriorityr_writes_priority_not_cfg() {
            let d = dist();
            // IPRIORITYR word for SPIs 32..=35 (GICD offset 0x400 + 0x20).
            let prio_off = GICD_BASE + GicdRegister::IPRIORITYR0.0 as u64 + 0x20;
            d.write(prio_off, &0x4433_2211u32.to_ne_bytes());

            let mut w = [0u8; 4];
            d.read(prio_off, &mut w);
            assert_eq!(u32::from_ne_bytes(w), 0x4030_2010);

            // The ICFGR word the buggy index collided with (cfg[8], GICD offset
            // 0xc00 + 0x20) must be untouched by a priority write.
            let cfg_off = GICD_BASE + GicdRegister::ICFGR0.0 as u64 + 0x20;
            let mut c = [0u8; 4];
            d.read(cfg_off, &mut c);
            assert_eq!(u32::from_ne_bytes(c), 0);
        }

        #[test]
        fn gicd_ipriorityr_supports_subword_access() {
            let d = dist();
            let priority = GICD_BASE + GicdRegister::IPRIORITYR0.0 as u64 + 32;

            d.write(priority, &[0x27]);
            d.write(priority + 2, &[0x3f, 0x48]);

            let mut word = [0u8; 4];
            d.read(priority, &mut word);
            assert_eq!(word, [0x20, 0, 0x38, 0x48]);

            let mut byte = [0u8; 1];
            d.read(priority, &mut byte);
            assert_eq!(byte, [0x20]);

            let mut halfword = [0u8; 2];
            d.read(priority + 2, &mut halfword);
            assert_eq!(halfword, [0x38, 0x48]);
        }

        #[test]
        fn distributor_group_enable_gates_delivery_without_losing_pending_state() {
            let mut d = dist();
            let mut gicr = d.add_redistributor(0, true);
            gicr.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            gicr.write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 1);
            {
                let mut state = d.state.lock();
                state.pending[1] = 1;
                state.enable[1] = 1;
                state.group[1] = 1;
            }

            assert_eq!(d.select(&gicr, true), None);

            let ctlr: u32 = GicdCtlr::new().with_enable_grp1(true).into();
            d.write(GICD_BASE + GicdRegister::CTLR.0 as u64, &ctlr.to_ne_bytes());
            assert_eq!(d.select(&gicr, true), Some((32, 0)));

            d.write(GICD_BASE + GicdRegister::CTLR.0 as u64, &0u32.to_ne_bytes());
            assert_eq!(d.select(&gicr, true), None);
            assert_eq!(d.state.lock().pending[1], 1);
        }

        #[test]
        fn distributor_group_enable_gates_nonzero_pe_acknowledge() {
            let d = dist();
            let (mut gicr, _shared) = Redistributor::new(1, 1, true);
            gicr.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            gicr.write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 1);
            gicr.raise(1);

            assert_eq!(d.ack(&mut gicr, true), 1023);

            let ctlr: u32 = GicdCtlr::new().with_enable_grp1(true).into();
            d.write(GICD_BASE + GicdRegister::CTLR.0 as u64, &ctlr.to_ne_bytes());
            assert_eq!(d.ack(&mut gicr, true), 1);
        }

        #[test]
        fn spi_affinity_route_selects_the_target_pe() {
            let (d, mut redists) = dist_with_redists(2);
            provision_spi(&d, 40, 0x40);
            route_spi(&d, 40, 1);

            assert_eq!(d.set_pending(40, true), Some(1));
            assert!(!d.irq_pending(&redists[0]));
            assert!(d.irq_pending(&redists[1]));
            assert_eq!(d.ack(&mut redists[0], true), 1023);
            assert_eq!(d.ack(&mut redists[1], true), 40);

            d.write_sysreg(&mut redists[1], SystemReg::ICC_EOIR1_EL1, 40, |_| {});
            assert_eq!(d.set_pending(40, true), Some(1));
            assert_eq!(d.ack(&mut redists[1], true), 40);
        }

        #[test]
        fn spi_eoi_uses_intid_not_the_current_route() {
            let (d, mut redists) = dist_with_redists(2);
            provision_spi(&d, 40, 0x40);
            route_spi(&d, 40, 1);
            d.set_pending(40, true);
            assert_eq!(d.ack(&mut redists[1], true), 40);

            route_spi(&d, 40, 0);
            d.write_sysreg(&mut redists[1], SystemReg::ICC_EOIR1_EL1, 40, |_| {});
            assert_eq!(
                redists[1].read_cpuif(SystemReg::ICC_RPR_EL1),
                Some(u64::from(super::super::gicr::IDLE_PRIORITY))
            );

            assert_eq!(d.set_pending(40, true), Some(0));
            assert_eq!(d.ack(&mut redists[0], true), 40);
        }

        #[test]
        fn unmatched_spi_affinity_preserves_pending_without_forwarding() {
            let (d, mut redists) = dist_with_redists(2);
            provision_spi(&d, 40, 0x40);
            route_spi(&d, 40, 0xff);

            assert_eq!(d.set_pending(40, true), None);
            assert!(!d.irq_pending(&redists[0]));
            assert!(!d.irq_pending(&redists[1]));

            route_spi(&d, 40, 1);
            assert_eq!(d.ack(&mut redists[1], true), 40);
        }

        #[test]
        fn one_of_n_spi_route_uses_a_deterministic_pe() {
            let (d, mut redists) = dist_with_redists(2);
            provision_spi(&d, 40, 0x40);
            route_spi(&d, 40, 1 << 31);

            assert_eq!(d.set_pending(40, true), Some(0));
            assert_eq!(d.ack(&mut redists[0], true), 40);
            assert_eq!(d.ack(&mut redists[1], true), 1023);
        }

        #[test]
        fn one_of_n_spi_skips_cpu_disabled_pe() {
            let (d, mut redists) = dist_with_redists(2);
            redists[0].write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 0);
            provision_spi(&d, 40, 0x20);
            route_spi(&d, 40, 1 << 31);

            assert_eq!(d.set_pending(40, true), Some(1));
            assert!(!d.irq_pending_for_group(&redists[0], true));
            assert!(d.irq_pending_for_group(&redists[1], true));
        }

        #[test]
        fn one_of_n_spi_skips_sleeping_pe() {
            let (d, redists) = dist_with_redists(2);
            let waker: u32 = aarch64defs::gic::GicrWaker::new()
                .with_processor_sleep(true)
                .into();
            redists[0].shared.write(
                aarch64defs::gic::GicrRdRegister::WAKER.0 as u64,
                &waker.to_ne_bytes(),
            );
            provision_spi(&d, 40, 0x20);
            route_spi(&d, 40, 1 << 31);

            assert_eq!(d.set_pending(40, true), Some(1));
            assert!(d.irq_pending_for_group(&redists[1], true));
        }

        #[test]
        fn group0_pending_is_reported_separately() {
            let (d, mut redists) = dist_with_redists(1);
            let redist = &mut redists[0];
            redist.write_cpuif(SystemReg::ICC_IGRPEN0_EL1, 1);
            let ctlr: u32 = GicdCtlr::new().with_enable_grp0(true).into();
            d.write(GICD_BASE + GicdRegister::CTLR.0 as u64, &ctlr.to_ne_bytes());
            provision_spi(&d, 40, 0x20);
            {
                let mut state = d.state.lock();
                state.group[1] &= !(1 << 8);
            }
            route_spi(&d, 40, 0);

            d.set_pending(40, true);
            assert!(d.irq_pending_for_group(redist, false));
            assert!(!d.irq_pending_for_group(redist, true));
        }

        #[test]
        fn spi_deactivation_notifies_new_one_of_n_target() {
            let (d, mut redists) = dist_with_redists(2);
            redists[0].write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 0);
            provision_spi(&d, 40, 0x20);
            route_spi(&d, 40, 1 << 31);
            d.set_pending(40, true);
            assert_eq!(d.ack(&mut redists[1], true), 40);

            redists[0].write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 1);
            let mut woken = Vec::new();
            assert!(
                d.write_sysreg(&mut redists[1], SystemReg::ICC_EOIR1_EL1, 40, |index| woken
                    .push(index))
            );

            assert!(woken.contains(&0));
            assert!(d.irq_pending_for_group(&redists[0], true));
        }

        #[test]
        fn stale_spi_selection_cannot_activate_twice() {
            let (d, _) = dist_with_redists(2);
            provision_spi(&d, 40, 0x20);
            route_spi(&d, 40, 1 << 31);
            d.set_pending(40, true);

            assert!(d.activate_spi(40, 0, true));
            assert!(!d.activate_spi(40, 0, true));
            assert!(!d.activate_spi(40, 1, true));
        }

        #[test]
        fn eoimode_split_completion_for_sgi() {
            let (d, mut redists) = dist_with_redists(1);
            let redist = &mut redists[0];
            redist.write_cpuif(SystemReg::ICC_CTLR_EL1, 1 << 1);
            redist.raise(1);
            assert_eq!(d.ack(redist, true), 1);

            assert!(d.write_sysreg(redist, SystemReg::ICC_EOIR1_EL1, 1, |_| {}));
            assert_eq!(
                redist.read_cpuif(SystemReg::ICC_RPR_EL1),
                Some(u64::from(super::super::gicr::IDLE_PRIORITY))
            );

            redist.raise(1);
            assert_eq!(d.ack(redist, true), 1023);
            assert!(d.write_sysreg(redist, SystemReg::ICC_DIR_EL1, 1, |_| {}));
            assert_eq!(d.ack(redist, true), 1);
        }

        #[test]
        fn eoimode_split_completion_for_spi() {
            let (d, mut redists) = dist_with_redists(1);
            let redist = &mut redists[0];
            redist.write_cpuif(SystemReg::ICC_CTLR_EL1, 1 << 1);
            provision_spi(&d, 40, 0x40);
            route_spi(&d, 40, 0);
            d.set_pending(40, true);
            assert_eq!(d.ack(redist, true), 40);

            assert!(d.write_sysreg(redist, SystemReg::ICC_EOIR1_EL1, 40, |_| {}));
            assert_eq!(d.set_pending(40, true), Some(0));
            assert_eq!(d.ack(redist, true), 1023);

            assert!(d.write_sysreg(redist, SystemReg::ICC_DIR_EL1, 40, |_| {}));
            assert_eq!(d.ack(redist, true), 40);
        }

        #[test]
        fn dir_is_ignored_when_eoimode_is_clear() {
            let (d, mut redists) = dist_with_redists(1);
            let redist = &mut redists[0];
            redist.raise(1);
            assert_eq!(d.ack(redist, true), 1);

            assert!(d.write_sysreg(redist, SystemReg::ICC_DIR_EL1, 1, |_| {}));
            redist.raise(1);
            assert_eq!(d.ack(redist, true), 1023);

            assert!(d.write_sysreg(redist, SystemReg::ICC_EOIR1_EL1, 1, |_| {}));
            assert_eq!(d.ack(redist, true), 1);
        }

        #[test]
        fn special_intids_do_not_drop_or_deactivate() {
            let (d, mut redists) = dist_with_redists(1);
            let redist = &mut redists[0];
            redist.write_cpuif(SystemReg::ICC_CTLR_EL1, 1 << 1);
            redist.raise(1);
            assert_eq!(d.ack(redist, true), 1);

            assert!(d.write_sysreg(redist, SystemReg::ICC_EOIR1_EL1, 1023, |_| {}));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_RPR_EL1), Some(0));
            assert!(d.write_sysreg(redist, SystemReg::ICC_DIR_EL1, 1023, |_| {}));

            redist.raise(1);
            assert_eq!(d.ack(redist, true), 1023);
        }
    }
}

mod gicr {
    use aarch64defs::MpidrEl1;
    use aarch64defs::SystemReg;
    use aarch64defs::gic::GicrCtlr;
    use aarch64defs::gic::GicrRdRegister;
    use aarch64defs::gic::GicrSgiRegister;
    use aarch64defs::gic::GicrTyper;
    use aarch64defs::gic::GicrWaker;
    use aarch64defs::gic::IccCtlrEl1;
    use inspect::Inspect;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    // The CPU interface models five priority bits without NMI or EL3 support.
    //
    // Arm GIC Architecture Specification, Arm IHI 0069H.b §12.2.6,
    // ICC_CTLR_EL1.PRIbits: "number of priority bits implemented, minus one."
    // A two-Security-state implementation must provide at least five bits.
    // `icc_cpu_interface_round_trips` verifies PRIbits=4, meaning five bits.

    /// Number of implemented priority bits. With 5 bits the upper 5 bits of each
    /// 8-bit priority value are significant, so ICC_CTLR_EL1.PRIbits reads back
    /// `PRIBITS - 1 = 4`. Five preemption levels map onto a single 32-bit
    /// active-priority word (ICC_AP1R0_EL1), matching the redistributor's
    /// existing single-word state shape.
    pub(crate) const PRIBITS: u8 = 5;

    /// Active-priority bitmap index shift: `group_priority >> PREEMPT_SHIFT` is
    /// the bit position in ICC_AP{0,1}R0. With 5 priority bits a group priority
    /// is a multiple of 8 (its low 3 bits are res0), so the 32 group priorities
    /// map onto the 32 bits of the word.
    pub(crate) const PREEMPT_SHIFT: u8 = 8 - PRIBITS;

    /// Mask of implemented bits in each priority field.
    pub(crate) const PRIORITY_MASK: u8 = u8::MAX << PREEMPT_SHIFT;

    /// Four packed priority fields, for GICD/GICR priority register writes.
    pub(crate) const PRIORITY_WORD_MASK: u32 = PRIORITY_MASK as u32 * 0x0101_0101;

    /// Architectural minimum value of ICC_BPR0_EL1 for this configuration
    /// (writes below it read back as it; it is also the reset value).
    ///
    /// Arm IHI 0069H.b §4.8.3, Table 4-13 gives minimum BPR0=2 for
    /// five implemented priority bits. §12.2.4 requires writes below the
    /// minimum to read back as the minimum.
    pub(crate) const MIN_BPR0: u8 = 2;

    /// Architectural minimum value of ICC_BPR1_EL1 (`MIN_BPR0 + 1`).
    ///
    /// Arm IHI 0069H.b §12.2.5, ICC_BPR1_EL1.BinaryPoint requires the
    /// Non-secure minimum to be "ICC_BPR0_EL1 + 1".
    pub(crate) const MIN_BPR1: u8 = 3;

    /// Running priority reported when the active-priority bitmap is empty: the
    /// lowest possible priority, so any unmasked interrupt preempts.
    ///
    /// Arm IHI 0069H.b §1.2.4 defines idle priority as 0xFF.
    pub(crate) const IDLE_PRIORITY: u8 = 0xff;

    /// SGIs are permanently enabled, matching the Hyper-V GIC interface.
    ///
    /// Arm IHI 0069H.b §4.7.1 makes permanent SGI enablement an
    /// IMPLEMENTATION DEFINED choice.
    pub(crate) const SGI_ENABLE_MASK: u32 = 0x0000_ffff;

    /// Reset SGIs and PPIs to Non-secure Group 1, matching the Hyper-V GIC
    /// interface used by ARM64 guests.
    ///
    /// Arm IHI 0069H.b §12.11.12 defines `GICR_IGROUPR0` reset as
    /// architecturally UNKNOWN. All ones is an explicit Hyper-V platform
    /// policy within that latitude, not a universal GIC reset value.
    pub(crate) const IGROUPR0_RESET: u32 = 0xffff_ffff;

    /// The group-priority mask for binary point `bpr`. The binary point splits
    /// an 8-bit priority into a group-priority field (the high `7 - bpr` bits,
    /// used for preemption) and a subpriority field (the low `bpr + 1` bits,
    /// ignored for preemption). Computed in u16 to avoid a shift overflow at
    /// `bpr == 7` (where there is no preemption and the mask is 0).
    ///
    /// Arm IHI 0069H.b §4.8.3: interrupts with the same group priority have
    /// equal preemption rank "regardless of the subpriority". Enforced by
    /// `group_mask_and_priority_split` and the independent model.
    pub(crate) fn group_mask(bpr: u8) -> u8 {
        (0xffu16 << (bpr + 1)) as u8
    }

    /// The group-priority field of `priority` under binary point `bpr`.
    pub(crate) fn group_priority(priority: u8, bpr: u8) -> u8 {
        priority & group_mask(bpr)
    }

    /// The effective binary point for a candidate. For Non-secure Group 1 with
    /// CBPR clear the architecture uses `BPR1 - 1` (the `VGroupBits`
    /// pseudocode); otherwise (Group 0, or CBPR set which aliases Group 1 onto
    /// BPR0) it uses BPR0.
    ///
    /// Arm IHI 0069H.b §4.8.3 `VGroupBits()` uses
    /// `ICC_BPR1_EL1NS.BinaryPoint - 1` for Non-secure Group 1 when CBPR=0.
    /// Enforced by `effective_bpr_group1_uses_bpr1_minus_one`.
    pub(crate) fn effective_bpr(group1: bool, cbpr: bool, bpr0: u8, bpr1: u8) -> u8 {
        if group1 && !cbpr {
            bpr1.saturating_sub(1)
        } else {
            bpr0
        }
    }

    /// The running priority encoded by an active-priority bitmap: its lowest set
    /// bit scaled back into the 8-bit priority space, or `IDLE_PRIORITY` when
    /// the bitmap is empty.
    ///
    /// Arm IHI 0069H.b §12.2.20, `ICC_RPR_EL1.Priority`, returns the current
    /// active group priority or Idle priority after all priority drops.
    /// Enforced by `running_priority_from_active_bitmap` and AP nesting tests.
    pub(crate) fn running_priority(apr: u32) -> u8 {
        if apr == 0 {
            IDLE_PRIORITY
        } else {
            (apr.trailing_zeros() as u8) << PREEMPT_SHIFT
        }
    }

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
        // GICv3 CPU-interface registers. Banked per-PE, so they live on the
        // per-CPU redistributor state. Delivery (`best_candidate`/`admit`/
        // `ack`/`eoi`) consults these to honor priority masking (PMR),
        // preemption (the active-priority bitmap), and binary-point grouping.
        #[inspect(hex)]
        icc_pmr: u8,
        #[inspect(hex)]
        icc_bpr0: u8,
        #[inspect(hex)]
        icc_bpr1: u8,
        icc_grpen0: bool,
        icc_grpen1: bool,
        icc_cbpr: bool,
        icc_eoimode: bool,
        // Active-priority bitmaps (ICC_AP0R0_EL1 / ICC_AP1R0_EL1). Each set bit
        // marks an active group priority; the lowest set bit is the running
        // priority. Maintained by `push_priority` (acknowledge) and
        // `pop_priority` (EOI priority-drop).
        #[inspect(hex)]
        icc_ap0r0: u32,
        #[inspect(hex)]
        icc_ap1r0: u32,
    }

    impl SharedState {
        pub fn raise(&self, intid: u32) -> bool {
            let mask = 1 << intid;
            self.pending.fetch_or(mask, Ordering::Relaxed) & mask == 0
        }

        pub(super) fn eligible_for_one_of_n(&self, group1: bool) -> bool {
            let state = self.mutable.lock();
            !state.sleep
                && if group1 {
                    state.icc_grpen1
                } else {
                    state.icc_grpen0
                }
        }

        pub(super) fn is_group1(&self, intid: u32) -> bool {
            self.mutable.lock().group & (1 << intid) != 0
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
                    1 | 2 => self.sgi_read_subword(address, data),
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
                    1 | 2 => self.sgi_write_subword(address, data),
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
                GicrSgiRegister::ISENABLER0 => {
                    // SGIs (low 16 bits) are permanently enabled; only PPI
                    // enable bits are writable.
                    self.mutable.lock().enable |= data & !SGI_ENABLE_MASK;
                }
                GicrSgiRegister::ICENABLER0 => {
                    // SGIs (low 16 bits) are permanently enabled and cannot be
                    // disabled; only PPI enable bits are clearable.
                    self.mutable.lock().enable &= !(data & !SGI_ENABLE_MASK);
                }
                GicrSgiRegister::ICFGR0 => {
                    // Cannot change trigger mode for SGIs.
                }
                GicrSgiRegister::ICFGR1 => self.mutable.lock().ppi_cfg = data,
                r if GicrSgiRegister::IPRIORITYR.contains(&r.0) => {
                    let n = (r.0 & 0x1f) / 4;
                    self.mutable.lock().priority[n as usize] = data & PRIORITY_WORD_MASK;
                }
                _ => return false,
            }
            tracing::debug!(?address, data, "gicr sgi write32");
            true
        }

        /// Handles byte/halfword writes to the SGI-frame `IPRIORITYR` registers.
        /// The architecture permits software to access priority registers at byte
        /// granularity, and the guest does. This is restricted to `IPRIORITYR`
        /// because the read-modify-write needed for sub-word access is only
        /// correct for plain storage registers; the set/clear registers
        /// (ISENABLER/ICENABLER/ISPENDR/...) must not be RMW'd this way.
        ///
        /// Arm IHI 0069H.b §12.1.3 expressly requires byte access to
        /// `GICR_IPRIORITYR<n>` and makes byte access to unlisted registers
        /// unsupported/UNPREDICTABLE. Enforced by
        /// `ipriorityr_byte_write_read`,
        /// `ipriorityr_byte_writes_dont_clobber`, and
        /// `subword_does_not_corrupt_set_clear_registers`.
        fn sgi_write_subword(&self, address: GicrSgiRegister, data: &[u8]) -> bool {
            if !GicrSgiRegister::IPRIORITYR.contains(&address.0) {
                return false;
            }
            let word = GicrSgiRegister(address.0 & !0x3);
            let mut bytes = self.sgi_read32(word).unwrap_or(0).to_ne_bytes();
            let offset = (address.0 & 0x3) as usize;
            bytes[offset..offset + data.len()].copy_from_slice(data);
            self.sgi_write32(word, u32::from_ne_bytes(bytes))
        }

        /// Handles byte/halfword reads of the SGI-frame `IPRIORITYR` registers.
        /// See [`Self::sgi_write_subword`] for why this is limited to `IPRIORITYR`.
        fn sgi_read_subword(&self, address: GicrSgiRegister, data: &mut [u8]) -> bool {
            if !GicrSgiRegister::IPRIORITYR.contains(&address.0) {
                return false;
            }
            let word = GicrSgiRegister(address.0 & !0x3);
            let Some(value) = self.sgi_read32(word) else {
                return false;
            };
            let bytes = value.to_ne_bytes();
            let offset = (address.0 & 0x3) as usize;
            data.copy_from_slice(&bytes[offset..offset + data.len()]);
            true
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
                    group: IGROUPR0_RESET,
                    enable: SGI_ENABLE_MASK,
                    ppi_cfg: 0,
                    priority: [0; 8],
                    sleep: false,
                    icc_pmr: 0,
                    icc_bpr0: MIN_BPR0,
                    icc_bpr1: MIN_BPR1,
                    icc_grpen0: false,
                    icc_grpen1: false,
                    icc_cbpr: false,
                    icc_eoimode: false,
                    icc_ap0r0: 0,
                    icc_ap1r0: 0,
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

        /// Records a write to one of the GICv3 CPU-interface system registers
        /// (ICC_PMR/BPR/IGRPEN/CTLR). These are banked per-PE, so they live on
        /// the redistributor's per-CPU state. Returns `true` if `reg` is a
        /// CPU-interface register handled here.
        ///
        /// The priority engine consults this state for delivery: PMR masks,
        /// BPR/CBPR group the priority, and IGRPEN/CTLR.EOImode shape ack/eoi.
        /// BPR writes are clamped to the architectural minimum for this
        /// configuration (writes below it read back as it).
        ///
        /// Arm IHI 0069H.b §§12.2.4-12.2.6, 12.2.15-12.2.16, and 12.2.19
        /// define these banked CPU-interface fields. `ICC_PMR_EL1` low
        /// unimplemented bits are RAZ/WI; `ICC_CTLR_EL1.CBPR/EOImode` select
        /// BPR sharing and split completion. Enforced by
        /// `icc_cpu_interface_round_trips`.
        pub(crate) fn write_cpuif(&mut self, reg: SystemReg, value: u64) -> bool {
            let mut state = self.shared.mutable.lock();
            match reg {
                SystemReg::ICC_PMR_EL1 => state.icc_pmr = value as u8 & PRIORITY_MASK,
                SystemReg::ICC_BPR0_EL1 => state.icc_bpr0 = ((value & 0x7) as u8).max(MIN_BPR0),
                SystemReg::ICC_BPR1_EL1 => {
                    if !state.icc_cbpr {
                        state.icc_bpr1 = ((value & 0x7) as u8).max(MIN_BPR1);
                    }
                }
                SystemReg::ICC_IGRPEN0_EL1 => state.icc_grpen0 = value & 1 != 0,
                SystemReg::ICC_IGRPEN1_EL1 => state.icc_grpen1 = value & 1 != 0,
                SystemReg::ICC_AP0R0_EL1 => state.icc_ap0r0 = value as u32,
                SystemReg::ICC_AP1R0_EL1 => state.icc_ap1r0 = value as u32,
                SystemReg::ICC_CTLR_EL1 => {
                    let ctlr = IccCtlrEl1::from(value);
                    state.icc_cbpr = ctlr.cbpr();
                    state.icc_eoimode = ctlr.eoi_mode();
                }
                _ => return false,
            }
            true
        }

        /// Reads one of the GICv3 CPU-interface system registers. Returns `None`
        /// if `reg` is not a CPU-interface register handled here.
        ///
        /// The writable registers (PMR/BPR/IGRPEN) echo back what the guest
        /// wrote (clamped for BPR), exactly as real hardware does.
        ///
        /// ICC_CTLR_EL1 reports five implemented priority bits and 16-bit INTIDs.
        /// Arm IHI 0069H.b §12.2.6 encodes PRIbits as implemented bits minus
        /// one and IDbits=0 as 16-bit INTIDs. Enforced by
        /// `icc_cpu_interface_round_trips`.
        pub(crate) fn read_cpuif(&self, reg: SystemReg) -> Option<u64> {
            let state = self.shared.mutable.lock();
            let value: u64 = match reg {
                SystemReg::ICC_PMR_EL1 => state.icc_pmr.into(),
                SystemReg::ICC_BPR0_EL1 => state.icc_bpr0.into(),
                SystemReg::ICC_BPR1_EL1 => {
                    if state.icc_cbpr {
                        state.icc_bpr0.saturating_add(1).min(7).into()
                    } else {
                        state.icc_bpr1.into()
                    }
                }
                SystemReg::ICC_IGRPEN0_EL1 => state.icc_grpen0.into(),
                SystemReg::ICC_IGRPEN1_EL1 => state.icc_grpen1.into(),
                SystemReg::ICC_AP0R0_EL1 => state.icc_ap0r0.into(),
                SystemReg::ICC_AP1R0_EL1 => state.icc_ap1r0.into(),
                SystemReg::ICC_RPR_EL1 => running_priority(state.icc_ap0r0)
                    .min(running_priority(state.icc_ap1r0))
                    .into(),
                SystemReg::ICC_CTLR_EL1 => IccCtlrEl1::new()
                    .with_cbpr(state.icc_cbpr)
                    .with_eoi_mode(state.icc_eoimode)
                    .with_pri_bits(PRIBITS - 1)
                    .with_id_bits(0)
                    .into(),
                _ => return None,
            };
            Some(value)
        }

        pub fn raise(&mut self, intid: u32) {
            self.shared.pending.fetch_or(1 << intid, Ordering::Relaxed);
        }

        /// The best deliverable Group-`group1` SGI/PPI candidate on this
        /// redistributor: the pending, inactive, enabled interrupt of the
        /// matching group with the numerically lowest priority byte (ties broken
        /// by lowest intid). Returns `(intid, priority_byte)`.
        ///
        /// This does NOT apply PMR masking or the preemption test — that is
        /// `admit`'s job — so the distributor can first pick the global winner
        /// across this PE's SGI/PPIs and the shared SPIs, then admit it once.
        ///
        /// Arm IHI 0069H.b §§4.7-4.8 require Pending, enabled, matching-group,
        /// inactive candidates and numerically lowest priority selection.
        /// Lower-INTID tie-breaking is deterministic implementation policy.
        pub(crate) fn best_candidate(&self, group1: bool) -> Option<(u32, u8)> {
            let pending = self.shared.pending.load(Ordering::Relaxed);
            if pending == 0 {
                return None;
            }
            let state = self.shared.mutable.lock();
            let group = if group1 { state.group } else { !state.group };
            let mut deliverable = pending & !state.active & state.enable & group;
            let mut best: Option<(u32, u8)> = None;
            while deliverable != 0 {
                let intid = deliverable.trailing_zeros();
                deliverable &= deliverable - 1;
                let pri = state.priority[intid as usize / 4].to_ne_bytes()[intid as usize % 4];
                // Ascending intid order means a strictly-lower priority wins and
                // ties keep the earlier (lower) intid.
                best = Some(match best {
                    Some((bi, bp)) if bp <= pri => (bi, bp),
                    _ => (intid, pri),
                });
            }
            best
        }

        /// Applies PMR masking and the preemption test to a candidate (an
        /// SGI/PPI on this redistributor, or an SPI selected by the distributor)
        /// using this PE's CPU-interface state. Returns the candidate's group
        /// priority if it is deliverable now, else `None`.
        ///
        /// Arm IHI 0069H.b:
        /// - §12.2.19: only priority higher than PMR is signaled, so equality
        ///   is masked.
        /// - §4.8.5: candidate group priority must be lower than running
        ///   priority; equal priority does not preempt.
        /// - §§12.2.15-12.2.16: CPU-interface group enables gate signaling.
        ///
        /// For directly routed interrupts with IGRPEN clear, §4.7 leaves
        /// consideration IMPLEMENTATION DEFINED; this model consistently
        /// blocks delivery. Enforced by `pmr_masks_then_admits`,
        /// `higher_priority_preempts_active`, `equal_priority_does_not_preempt`,
        /// and `cpu_group_enable_gates_delivery_without_losing_pending_state`.
        pub(crate) fn admit(&self, group1: bool, priority: u8) -> Option<u8> {
            let state = self.shared.mutable.lock();
            if (group1 && !state.icc_grpen1) || (!group1 && !state.icc_grpen0) {
                return None;
            }
            // Priority masking: an interrupt is masked when priority >= PMR.
            if priority >= state.icc_pmr {
                return None;
            }
            let bpr = effective_bpr(group1, state.icc_cbpr, state.icc_bpr0, state.icc_bpr1);
            let gp = group_priority(priority, bpr);
            // Preemption: the candidate's group priority must be higher
            // (numerically lower) than the running priority.
            let running = running_priority(state.icc_ap0r0).min(running_priority(state.icc_ap1r0));
            let preemption_level = if running == IDLE_PRIORITY {
                IDLE_PRIORITY
            } else {
                group_priority(running, bpr)
            };
            (gp < preemption_level).then_some(gp)
        }

        /// Acknowledges `intid` (an SGI/PPI on this redistributor): clears its
        /// pending bit and sets its active bit.
        pub(crate) fn activate(&mut self, intid: u32) {
            self.shared
                .pending
                .fetch_and(!(1 << intid), Ordering::Relaxed);
            self.shared.mutable.lock().active |= 1 << intid;
        }

        /// Pushes a group priority onto this PE's active-priority bitmap for the
        /// given group (on acknowledge), raising the running priority.
        ///
        /// Arm IHI 0069H.b §4.8.4 defines AP registers as the active group
        /// priorities that have not undergone priority drop.
        pub(crate) fn push_priority(&mut self, group1: bool, group_priority: u8) {
            let mut state = self.shared.mutable.lock();
            let bit = 1u32 << (group_priority >> PREEMPT_SHIFT);
            if group1 {
                state.icc_ap1r0 |= bit;
            } else {
                state.icc_ap0r0 |= bit;
            }
        }

        /// Pops the running priority (clears the lowest set active-priority bit)
        /// for the given group — the EOI priority-drop. The IAR/EOIR nesting
        /// invariant guarantees the lowest set bit belongs to the interrupt
        /// being completed.
        ///
        /// Arm IHI 0069H.b §12.2.10 requires EOIR to correspond to the most
        /// recent valid IAR; other writes are UNPREDICTABLE. This implementation
        /// relies on that architectural software invariant. Enforced for valid
        /// nesting by `active_priority_nests_and_unwinds`.
        pub(crate) fn pop_priority(&mut self, group1: bool) {
            let mut state = self.shared.mutable.lock();
            let apr = if group1 {
                &mut state.icc_ap1r0
            } else {
                &mut state.icc_ap0r0
            };
            if *apr != 0 {
                *apr &= *apr - 1;
            }
        }

        /// Deactivates `intid` (an SGI/PPI on this redistributor): clears its
        /// active bit.
        pub(crate) fn deactivate(&mut self, intid: u32) {
            self.shared.mutable.lock().active &= !(1 << intid);
        }

        pub(crate) fn eoimode(&self) -> bool {
            self.shared.mutable.lock().icc_eoimode
        }

        #[cfg(test)]
        pub(crate) fn irq_pending(&self) -> bool {
            match self.best_candidate(true) {
                Some((_, pri)) => self.admit(true, pri).is_some(),
                None => false,
            }
        }

        pub fn is_pending_or_active(&self, intid: u32) -> bool {
            let state = self.shared.mutable.lock();
            (self.shared.pending.load(Ordering::Relaxed) | state.active) & (1 << intid) != 0
        }

        #[cfg(test)]
        pub(crate) fn ack(&mut self, group1: bool) -> Option<u32> {
            let (intid, pri) = self.best_candidate(group1)?;
            let gp = self.admit(group1, pri)?;
            self.activate(intid);
            self.push_priority(group1, gp);
            tracing::trace!(intid, "ack");
            Some(intid)
        }

        #[cfg(test)]
        pub(crate) fn eoi(&mut self, group1: bool, intid: u32) {
            assert!(intid < 32);
            tracing::trace!(intid, "eoi");
            self.pop_priority(group1);
            self.deactivate(intid);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::Redistributor;
        use aarch64defs::gic::GicrSgiRegister;

        // Offset from a redistributor base to its SGI frame.
        const SGI: u64 = 0x1_0000;
        const IPRIORITYR0: u64 = GicrSgiRegister::IPRIORITYR0.0 as u64;
        const ISENABLER0: u64 = GicrSgiRegister::ISENABLER0.0 as u64;
        const ICENABLER0: u64 = GicrSgiRegister::ICENABLER0.0 as u64;

        // Architecture permits byte access to IPRIORITYR; the guest uses it.
        #[test]
        fn ipriorityr_byte_write_read() {
            let (_redist, shared) = Redistributor::new(0, 0, true);

            // intid 20 (the vtimer PPI) sits at IPRIORITYR0 + 0x14.
            shared.write(SGI + IPRIORITYR0 + 0x14, &[0x27]);

            // Reads back at byte, halfword, and word granularity.
            let mut b = [0u8; 1];
            shared.read(SGI + IPRIORITYR0 + 0x14, &mut b);
            assert_eq!(b[0], 0x20);

            let mut w = [0u8; 4];
            shared.read(SGI + IPRIORITYR0 + 0x14, &mut w);
            assert_eq!(u32::from_ne_bytes(w) & 0xff, 0x20);
        }

        // Independent byte writes to one word must not clobber each other.
        #[test]
        fn ipriorityr_byte_writes_dont_clobber() {
            let (_redist, shared) = Redistributor::new(0, 0, true);

            shared.write(SGI + IPRIORITYR0, &[0x10]); // intid 0
            shared.write(SGI + IPRIORITYR0 + 1, &[0x20]); // intid 1
            shared.write(SGI + IPRIORITYR0 + 2, &[0x00]); // intid 2
            shared.write(SGI + IPRIORITYR0 + 3, &[0x40]); // intid 3

            let mut w = [0u8; 4];
            shared.read(SGI + IPRIORITYR0, &mut w);
            assert_eq!(w, [0x10, 0x20, 0x00, 0x40]);
        }

        // Sub-word access to set/clear registers must be rejected, not
        // read-modify-written (which would corrupt the enable mask).
        #[test]
        fn subword_does_not_corrupt_set_clear_registers() {
            let (_redist, shared) = Redistributor::new(0, 0, true);

            // Enable all 32 SGI/PPI interrupts via a word write to ISENABLER0.
            shared.write(SGI + ISENABLER0, &0xffff_ffffu32.to_ne_bytes());

            // A stray byte write to ICENABLER0 must be ignored, not RMW'd
            // (an RMW through ICENABLER would clear the whole enable mask).
            shared.write(SGI + ICENABLER0, &[0xff]);

            let mut w = [0u8; 4];
            shared.read(SGI + ISENABLER0, &mut w);
            assert_eq!(u32::from_ne_bytes(w), 0xffff_ffff);
        }

        // The GICv3 CPU-interface registers round-trip, and ICC_CTLR_EL1 reports
        // the implemented priority capability.
        #[test]
        fn icc_cpu_interface_round_trips() {
            use aarch64defs::SystemReg;
            use aarch64defs::gic::IccCtlrEl1;

            let (mut redist, _shared) = Redistributor::new(0, 0, true);

            // Writable registers read back what was written.
            assert!(redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xf0));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_PMR_EL1), Some(0xf0));
            assert!(redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff));
            assert_eq!(
                redist.read_cpuif(SystemReg::ICC_PMR_EL1),
                Some(super::PRIORITY_MASK.into())
            );
            assert!(redist.write_cpuif(SystemReg::ICC_BPR1_EL1, 0x3));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_BPR1_EL1), Some(0x3));
            assert!(redist.write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 0x1));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_IGRPEN1_EL1), Some(0x1));
            assert!(redist.write_cpuif(SystemReg::ICC_AP0R0_EL1, 1 << 5));
            assert!(redist.write_cpuif(SystemReg::ICC_AP1R0_EL1, 1 << 3));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_AP0R0_EL1), Some(1 << 5));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_AP1R0_EL1), Some(1 << 3));
            assert_eq!(
                redist.read_cpuif(SystemReg::ICC_RPR_EL1),
                Some(u64::from(3u8 << super::PREEMPT_SHIFT))
            );

            // BPR writes below the architectural minimum read back as the
            // minimum (BPR0 >= 2, BPR1 >= 3).
            assert!(redist.write_cpuif(SystemReg::ICC_BPR0_EL1, 0x0));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_BPR0_EL1), Some(2));
            assert!(redist.write_cpuif(SystemReg::ICC_BPR1_EL1, 0x0));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_BPR1_EL1), Some(3));

            // ICC_CTLR_EL1 reports PRIbits = PRIBITS - 1 and IDbits = 0, plus the
            // writable CBPR/EOImode bits.
            redist.write_cpuif(SystemReg::ICC_CTLR_EL1, 0x3); // CBPR | EOImode
            let ctlr = IccCtlrEl1::from(redist.read_cpuif(SystemReg::ICC_CTLR_EL1).unwrap());
            assert_eq!(ctlr.pri_bits(), super::PRIBITS - 1);
            assert_eq!(ctlr.id_bits(), 0);
            assert!(ctlr.cbpr());
            assert!(ctlr.eoi_mode());
            let feature_bits: u64 = IccCtlrEl1::new().with_rss(true).with_ext_range(true).into();
            assert_eq!(feature_bits, 3 << 18);

            // A non-CPU-interface register is not claimed here.
            assert!(!redist.write_cpuif(SystemReg::ICC_IAR1_EL1, 0));
            assert_eq!(redist.read_cpuif(SystemReg::ICC_IAR1_EL1), None);
        }

        // Offset from the SGI frame to the IGROUPR0 register.
        const IGROUPR0: u64 = GicrSgiRegister::IGROUPR0.0 as u64;

        // Enables `intid` (an SGI/PPI) at Group 1 via the SGI-frame MMIO, the
        // way a guest does, so it is deliverable through `irq_pending`/`ack`.
        // IGROUPR0 is a whole-word register (a guest read-modify-writes it), so
        // accumulate the group bit rather than overwriting other intids' bits.
        fn enable_group1(shared: &super::SharedState, intid: u32) {
            shared.write(SGI + ISENABLER0, &(1u32 << intid).to_ne_bytes());
            let mut g = [0u8; 4];
            shared.read(SGI + IGROUPR0, &mut g);
            let group = u32::from_ne_bytes(g) | (1u32 << intid);
            shared.write(SGI + IGROUPR0, &group.to_ne_bytes());
        }

        // Enables `intid` (an SGI/PPI) at Group 0. IGROUPR0 now resets to all
        // Group 1 (matching Hyper-V's vGIC; see `IGROUPR0_RESET`), so explicitly
        // clear this intid's group bit to place it in Group 0.
        fn enable_group0(shared: &super::SharedState, intid: u32) {
            shared.write(SGI + ISENABLER0, &(1u32 << intid).to_ne_bytes());
            let mut g = [0u8; 4];
            shared.read(SGI + IGROUPR0, &mut g);
            let group = u32::from_ne_bytes(g) & !(1u32 << intid);
            shared.write(SGI + IGROUPR0, &group.to_ne_bytes());
        }

        // Sets the 8-bit priority of an SGI/PPI `intid` via byte MMIO, the way a
        // guest does. Lower value == higher priority.
        fn set_priority(shared: &super::SharedState, intid: u32, priority: u8) {
            shared.write(SGI + IPRIORITYR0 + intid as u64, &[priority]);
        }

        fn enable_cpu_group(redist: &mut Redistributor, group1: bool) {
            use aarch64defs::SystemReg;

            let reg = if group1 {
                SystemReg::ICC_IGRPEN1_EL1
            } else {
                SystemReg::ICC_IGRPEN0_EL1
            };
            redist.write_cpuif(reg, 1);
        }

        #[test]
        fn cpu_group_enable_gates_delivery_without_losing_pending_state() {
            use aarch64defs::SystemReg;

            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_group1(&shared, 20);
            redist.raise(20);

            assert!(!redist.irq_pending());
            assert_eq!(redist.ack(true), None);

            enable_cpu_group(&mut redist, true);
            assert!(redist.irq_pending());

            redist.write_cpuif(SystemReg::ICC_IGRPEN1_EL1, 0);
            assert!(!redist.irq_pending());
            assert_eq!(redist.ack(true), None);

            enable_cpu_group(&mut redist, true);
            assert_eq!(redist.ack(true), Some(20));
        }

        // `ack` must honour the same deliverability mask as `irq_pending`
        // (pending & !active & enable & group): a pending-but-disabled
        // interrupt must NOT be acknowledged.
        #[test]
        fn ack_skips_disabled_interrupt() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            // Unmask PMR so the *only* reason for non-delivery is "disabled".
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            // Group 1 + pending for intid 20, but deliberately not enabled.
            shared.write(SGI + IGROUPR0, &(1u32 << 20).to_ne_bytes());
            redist.raise(20);
            assert_eq!(redist.ack(true), None);
        }

        // `ack` must not underflow (`31 - 32`) when something is pending but
        // nothing is deliverable — e.g. the only pending bit is already active.
        #[test]
        fn ack_returns_none_when_pending_is_all_active() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            // PMR must unmask before anything is deliverable (reset PMR=0 masks
            // all, as on real hardware).
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            enable_group1(&shared, 20);
            redist.raise(20);
            assert_eq!(redist.ack(true), Some(20)); // delivers + activates
            redist.raise(20); // re-raise while active: pending != 0, deliverable == 0
            assert_eq!(redist.ack(true), None); // must not panic
        }

        // The normal path: an enabled Group-1 PPI is pending, acks, becomes
        // active (no longer deliverable), and is deliverable again after EOI.
        #[test]
        fn ack_delivers_enabled_group1_ppi() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            enable_group1(&shared, 20);
            redist.raise(20);
            assert!(redist.irq_pending());
            assert_eq!(redist.ack(true), Some(20));
            assert!(!redist.irq_pending());
            redist.eoi(true, 20);
            redist.raise(20);
            assert!(redist.irq_pending());
        }

        // ---- Priority-engine helper unit tests ------------------------------

        #[test]
        fn group_mask_and_priority_split() {
            use super::group_mask;
            use super::group_priority;
            // BPR splits an 8-bit priority into group (high) and sub (low) parts.
            // bpr=2 → low 3 bits are subpriority, so mask = 0xF8.
            assert_eq!(group_mask(2), 0xf8);
            assert_eq!(group_priority(0xa7, 2), 0xa0);
            // bpr=3 → low 4 bits subpriority, mask = 0xF0.
            assert_eq!(group_mask(3), 0xf0);
            assert_eq!(group_priority(0xa7, 3), 0xa0);
            // bpr=7 → no preemption, mask = 0 (must not overflow the shift).
            assert_eq!(group_mask(7), 0x00);
            assert_eq!(group_priority(0xff, 7), 0x00);
        }

        #[test]
        fn effective_bpr_group1_uses_bpr1_minus_one() {
            use super::effective_bpr;
            // NS Group 1, CBPR clear: BPR1 - 1.
            assert_eq!(effective_bpr(true, false, 2, 3), 2);
            // Group 1 with CBPR set aliases onto BPR0.
            assert_eq!(effective_bpr(true, true, 2, 3), 2);
            // Group 0 always uses BPR0.
            assert_eq!(effective_bpr(false, false, 2, 3), 2);
            // Saturates rather than underflowing at BPR1 = 0.
            assert_eq!(effective_bpr(true, false, 0, 0), 0);
        }

        #[test]
        fn cbpr_aliases_bpr1_reads_and_ignores_writes() {
            use aarch64defs::SystemReg;
            use aarch64defs::gic::IccCtlrEl1;

            let (mut redist, _) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_BPR0_EL1, 4);
            redist.write_cpuif(
                SystemReg::ICC_CTLR_EL1,
                IccCtlrEl1::new().with_cbpr(true).into(),
            );
            redist.write_cpuif(SystemReg::ICC_BPR1_EL1, 7);
            assert_eq!(redist.read_cpuif(SystemReg::ICC_BPR1_EL1), Some(5));

            redist.write_cpuif(SystemReg::ICC_CTLR_EL1, 0);
            assert_eq!(
                redist.read_cpuif(SystemReg::ICC_BPR1_EL1),
                Some(super::MIN_BPR1.into())
            );
        }

        #[test]
        fn running_priority_from_active_bitmap() {
            use super::IDLE_PRIORITY;
            use super::PREEMPT_SHIFT;
            use super::running_priority;
            // Empty bitmap → idle (lowest) priority, so anything can preempt.
            assert_eq!(running_priority(0), IDLE_PRIORITY);
            // Lowest set bit is the running priority, scaled into 8-bit space.
            // bit 20 set → running priority 20 << PREEMPT_SHIFT.
            assert_eq!(running_priority(1 << 20), 20 << PREEMPT_SHIFT);
            // With bits 20 and 24 set, the *lowest* (20) wins.
            assert_eq!(running_priority((1 << 20) | (1 << 24)), 20 << PREEMPT_SHIFT);
        }

        // ---- Priority-engine behavior tests --------------------------------

        // PMR masks an interrupt whose priority is numerically >= PMR, and
        // admits it once PMR is raised above it. Linux uses PMR=0xf0 and device
        // priorities around 0xa0.
        #[test]
        fn pmr_masks_then_admits() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            enable_cpu_group(&mut redist, true);
            enable_group1(&shared, 20);
            set_priority(&shared, 20, 0xa0);
            redist.raise(20);

            // PMR at reset (0) masks everything.
            assert!(!redist.irq_pending());
            // PMR = 0xa0: priority 0xa0 is NOT < 0xa0 → still masked.
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xa0);
            assert!(!redist.irq_pending());
            // PMR = 0xf0: 0xa0 < 0xf0 → deliverable.
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xf0);
            assert!(redist.irq_pending());
            assert_eq!(redist.ack(true), Some(20));
        }

        // Lower priority *byte* wins selection; ties break toward the lower
        // intid.
        #[test]
        fn selection_prefers_higher_priority_then_lower_intid() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            // intid 20 @ 0xa0, intid 22 @ 0x80 (higher priority), intid 24 @ 0x80.
            enable_group1(&shared, 20);
            enable_group1(&shared, 22);
            enable_group1(&shared, 24);
            set_priority(&shared, 20, 0xa0);
            set_priority(&shared, 22, 0x80);
            set_priority(&shared, 24, 0x80);
            redist.raise(20);
            redist.raise(22);
            redist.raise(24);
            // 0x80 beats 0xa0; between the two 0x80s the lower intid (22) wins.
            assert_eq!(redist.ack(true), Some(22));
            // Next highest is the other 0x80 (intid 24), but it cannot preempt
            // the equal running priority until EOI drops it.
            assert_eq!(redist.ack(true), None);
            redist.eoi(true, 22);
            assert_eq!(redist.ack(true), Some(24));
        }

        // A higher-priority interrupt preempts a lower-priority active one
        // (running priority gates by group priority), and after EOI the lower
        // one resumes.
        #[test]
        fn higher_priority_preempts_active() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            enable_group1(&shared, 20);
            enable_group1(&shared, 21);
            set_priority(&shared, 20, 0xa0);
            set_priority(&shared, 21, 0x40); // much higher priority
            // Deliver the low-priority one first.
            redist.raise(20);
            assert_eq!(redist.ack(true), Some(20));
            // Now a higher-priority interrupt arrives: it must preempt (running
            // priority is 0xa0's group, 0x40 < 0xa0).
            redist.raise(21);
            assert!(redist.irq_pending());
            assert_eq!(redist.ack(true), Some(21));
            // While 0x40 runs, the 0xa0 (re-raised) cannot preempt.
            redist.raise(20);
            assert_eq!(redist.ack(true), None);
            // EOI the high one; interrupt 20 is still active and *resumes* (it is
            // not re-acked). Its running priority (0xa0) is restored, so the
            // re-raised 0xa0 still cannot be delivered while 20 is active.
            redist.eoi(true, 21);
            assert_eq!(redist.ack(true), None);
            // Completing 20 returns to idle; now the re-raised 20 delivers again.
            redist.eoi(true, 20);
            assert_eq!(redist.ack(true), Some(20));
        }

        #[test]
        fn candidate_bpr_regroups_running_priority_for_preemption() {
            use aarch64defs::SystemReg;

            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            redist.write_cpuif(SystemReg::ICC_BPR0_EL1, 2);
            redist.write_cpuif(SystemReg::ICC_BPR1_EL1, 5);
            enable_cpu_group(&mut redist, false);
            enable_cpu_group(&mut redist, true);
            enable_group0(&shared, 20);
            enable_group1(&shared, 21);
            set_priority(&shared, 20, 0x28);
            set_priority(&shared, 21, 0x20);

            redist.raise(20);
            assert_eq!(redist.ack(false), Some(20));
            redist.raise(21);
            // Group1 effective BPR is 4, so both 0x28 and 0x20 regroup to
            // 0x20. Equal group priority must not preempt.
            assert_eq!(redist.ack(true), None);
        }

        // An equal-priority interrupt does NOT preempt an active one (strict
        // inequality in the preemption test).
        #[test]
        fn equal_priority_does_not_preempt() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            enable_group1(&shared, 20);
            enable_group1(&shared, 21);
            set_priority(&shared, 20, 0xa0);
            set_priority(&shared, 21, 0xa0);
            redist.raise(20);
            assert_eq!(redist.ack(true), Some(20));
            redist.raise(21);
            // Same group priority → no preemption while 20 is active.
            assert!(!redist.irq_pending());
            assert_eq!(redist.ack(true), None);
        }

        // The active-priority bitmap nests: two preemptions push two bits, and
        // EOIs pop them lowest-first, restoring each running priority in turn.
        #[test]
        fn active_priority_nests_and_unwinds() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            for (intid, pri) in [(20u32, 0xc0u8), (21, 0x80), (22, 0x40)] {
                enable_group1(&shared, intid);
                set_priority(&shared, intid, pri);
            }
            redist.raise(20);
            assert_eq!(redist.ack(true), Some(20)); // rp = 0xc0
            redist.raise(21);
            assert_eq!(redist.ack(true), Some(21)); // 0x80 preempts → rp = 0x80
            redist.raise(22);
            assert_eq!(redist.ack(true), Some(22)); // 0x40 preempts → rp = 0x40
            // Unwind: after each EOI the previous running priority is restored,
            // and a fresh equal-to-restored interrupt still cannot preempt.
            redist.eoi(true, 22); // rp back to 0x80
            redist.raise(22);
            set_priority(&shared, 22, 0x80);
            assert_eq!(redist.ack(true), None); // 0x80 !< 0x80
            redist.eoi(true, 21); // rp back to 0xc0
            assert_eq!(redist.ack(true), Some(22)); // 0x80 < 0xc0 now delivers
        }

        // Preemption is gated by the highest active priority across both groups.
        #[test]
        fn active_priority_is_combined_across_groups() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, false);
            enable_cpu_group(&mut redist, true);

            enable_group0(&shared, 20);
            set_priority(&shared, 20, 0x40);
            redist.raise(20);
            assert_eq!(redist.ack(false), Some(20));

            enable_group1(&shared, 21);
            set_priority(&shared, 21, 0x80);
            redist.raise(21);
            assert_eq!(redist.ack(true), None);

            set_priority(&shared, 21, 0x20);
            assert_eq!(redist.ack(true), Some(21));
        }

        // EOI clears the active state so a re-raised interrupt delivers again,
        // and the active-priority bit is dropped (running priority returns to
        // idle).
        #[test]
        fn eoi_clears_active_and_running_priority() {
            use aarch64defs::SystemReg;
            let (mut redist, shared) = Redistributor::new(0, 0, true);
            redist.write_cpuif(SystemReg::ICC_PMR_EL1, 0xff);
            enable_cpu_group(&mut redist, true);
            enable_group1(&shared, 20);
            set_priority(&shared, 20, 0xa0);
            redist.raise(20);
            assert_eq!(redist.ack(true), Some(20));
            assert!(redist.is_pending_or_active(20));
            redist.eoi(true, 20);
            assert!(!redist.is_pending_or_active(20));
            // Running priority is idle again: a lower-priority interrupt now
            // delivers.
            enable_group1(&shared, 21);
            set_priority(&shared, 21, 0xf0);
            redist.raise(21);
            assert_eq!(redist.ack(true), Some(21));
        }
    }
}
