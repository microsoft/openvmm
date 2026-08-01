// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A very incomplete implementation of ARM GICv3.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub use gicd::Distributor;
pub use gicr::Redistributor;

mod gicd {
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
        #[inspect(iter_by_index)]
        pending: Vec<u32>,
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
        pub fn new(gicd_base: u64, gicr_range: MemoryRange, max_spis: u32) -> Self {
            let n = (max_spis as usize + 1) / 32;
            Self {
                state: Mutex::new(DistributorState {
                    pending: vec![0; n],
                    active: vec![0; n],
                    group: vec![0; n],
                    enable: vec![0; n],
                    cfg: vec![0; n * 2],
                    priority: vec![0; n * 8],
                    route: vec![0; n * 64],
                    enable_grp0: false,
                    enable_grp1: false,
                }),
                max_spi_intid: 32 + max_spis - 1,
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

        pub fn set_pending(&self, intid: u32, pending: bool) -> Option<u32> {
            let v = &mut self.state.lock().pending[intid as usize / 32];
            let mask = 1 << (intid & 31);
            if (*v & mask != 0) != pending {
                tracing::debug!(intid, pending, "set pending");
            }
            if pending {
                *v |= mask;
                Some(0)
            } else {
                *v &= !mask;
                None
            }
        }

        pub fn irq_pending(&self, gicr: &Redistributor) -> bool {
            self.select(gicr, true).is_some()
        }

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
        /// the best SPI on the distributor (PE 0 only), then gated by this PE's
        /// PMR and preemption state. Returns `(intid, group_priority)`.
        ///
        /// Checking only the single best candidate is sufficient: if the
        /// highest-priority pending interrupt cannot pass PMR/preemption, none of
        /// lower priority can either. The redistributor and distributor locks are
        /// taken in turn, never simultaneously (matching the existing pattern).
        fn select(&self, gicr: &Redistributor, group1: bool) -> Option<(u32, u8)> {
            if !self.group_enabled(group1) {
                return None;
            }
            let mut cand = gicr.best_candidate(group1);
            if gicr.index == 0 {
                if let Some(spi) = self.best_spi(group1) {
                    // Lowest priority byte wins; ties keep the lower intid (the
                    // SGI/PPI, which is numerically below any SPI).
                    cand = Some(match cand {
                        Some((i, p)) if p <= spi.1 => (i, p),
                        _ => spi,
                    });
                }
            }
            let (intid, pri) = cand?;
            let gp = gicr.admit(group1, pri)?;
            Some((intid, gp))
        }

        /// The best deliverable Group-`group1` SPI: pending, inactive, enabled,
        /// of the matching group, with the lowest priority byte (ties → lowest
        /// intid). Word 0 is the per-redistributor SGI/PPI range, so the scan
        /// starts at intid 32.
        fn best_spi(&self, group1: bool) -> Option<(u32, u8)> {
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
                    let pri = state.priority[intid as usize / 4].to_ne_bytes()[intid as usize % 4];
                    best = Some(match best {
                        Some((bi, bp)) if bp <= pri => (bi, bp),
                        _ => (intid, pri),
                    });
                }
            }
            best
        }

        pub fn ack(&self, gicr: &mut Redistributor, group1: bool) -> u32 {
            let Some((intid, gp)) = self.select(gicr, group1) else {
                return 1023;
            };
            if intid < 32 {
                gicr.activate(intid);
            } else {
                let mut state = self.state.lock();
                let w = intid as usize / 32;
                state.pending[w] &= !(1 << (intid % 32));
                state.active[w] |= 1 << (intid % 32);
            }
            gicr.push_priority(group1, gp);
            tracing::trace!(intid, "gic ack");
            intid
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
                _ => return gicr.write_cpuif(reg, value),
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
                _ => return gicr.read_cpuif(reg),
            };
            Some(v)
        }

        fn eoi(&self, gicr: &mut Redistributor, group1: bool, intid: u32) {
            // Special INTIDs (>= 1020) have no active state.
            if intid >= 1020 {
                return;
            }
            if intid < 32 {
                // SGI/PPI: priority-drop + deactivate both happen on the redist.
                gicr.eoi(group1, intid);
                return;
            }
            if gicr.index != 0 {
                return;
            }
            // SPI: priority-drop on the acknowledging PE, then deactivate the
            // distributor's active bit.
            gicr.pop_priority(group1);
            tracing::trace!(intid, "gic eoi");
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
                    // Match the Hyper-V GIC interface expected by ARM64 guests.
                    .with_security_extn(true)
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

    #[cfg(test)]
    mod tests {
        use super::Distributor;
        use super::Redistributor;
        use aarch64defs::SystemReg;
        use aarch64defs::gic::GicdCtlr;
        use aarch64defs::gic::GicdRegister;
        use memory_range::MemoryRange;

        const GICD_BASE: u64 = 0x0800_0000;

        fn dist() -> Distributor {
            Distributor::new(GICD_BASE, MemoryRange::new(0x0808_0000..0x0810_0000), 988)
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
            assert_eq!(u32::from_ne_bytes(w), 0x4433_2211);

            // The ICFGR word the buggy index collided with (cfg[8], GICD offset
            // 0xc00 + 0x20) must be untouched by a priority write.
            let cfg_off = GICD_BASE + GicdRegister::ICFGR0.0 as u64 + 0x20;
            let mut c = [0u8; 4];
            d.read(cfg_off, &mut c);
            assert_eq!(u32::from_ne_bytes(c), 0);
        }

        #[test]
        fn distributor_group_enable_gates_delivery_without_losing_pending_state() {
            let d = dist();
            let (mut gicr, _shared) = Redistributor::new(0, 0, true);
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

    /// Architectural minimum value of ICC_BPR0_EL1 for this configuration
    /// (writes below it read back as it; it is also the reset value).
    pub(crate) const MIN_BPR0: u8 = 2;

    /// Architectural minimum value of ICC_BPR1_EL1 (`MIN_BPR0 + 1`).
    pub(crate) const MIN_BPR1: u8 = 3;

    /// Running priority reported when the active-priority bitmap is empty: the
    /// lowest possible priority, so any unmasked interrupt preempts.
    pub(crate) const IDLE_PRIORITY: u8 = 0xff;

    /// SGIs are permanently enabled, matching the Hyper-V GIC interface.
    pub(crate) const SGI_ENABLE_MASK: u32 = 0x0000_ffff;

    /// Reset SGIs and PPIs to Non-secure Group 1, matching the Hyper-V GIC
    /// interface used by ARM64 guests.
    pub(crate) const IGROUPR0_RESET: u32 = 0xffff_ffff;

    /// The group-priority mask for binary point `bpr`. The binary point splits
    /// an 8-bit priority into a group-priority field (the high `7 - bpr` bits,
    /// used for preemption) and a subpriority field (the low `bpr + 1` bits,
    /// ignored for preemption). Computed in u16 to avoid a shift overflow at
    /// `bpr == 7` (where there is no preemption and the mask is 0).
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
                    self.mutable.lock().priority[n as usize] = data;
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
        pub(crate) fn write_cpuif(&mut self, reg: SystemReg, value: u64) -> bool {
            let mut state = self.shared.mutable.lock();
            match reg {
                SystemReg::ICC_PMR_EL1 => state.icc_pmr = value as u8,
                SystemReg::ICC_BPR0_EL1 => state.icc_bpr0 = ((value & 0x7) as u8).max(MIN_BPR0),
                SystemReg::ICC_BPR1_EL1 => state.icc_bpr1 = ((value & 0x7) as u8).max(MIN_BPR1),
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
        pub(crate) fn read_cpuif(&self, reg: SystemReg) -> Option<u64> {
            let state = self.shared.mutable.lock();
            let value: u64 = match reg {
                SystemReg::ICC_PMR_EL1 => state.icc_pmr.into(),
                SystemReg::ICC_BPR0_EL1 => state.icc_bpr0.into(),
                SystemReg::ICC_BPR1_EL1 => state.icc_bpr1.into(),
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
            (gp < running).then_some(gp)
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
            shared.write(SGI + IPRIORITYR0 + 0x14, &[0x20]);

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
