// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Distributor;
use crate::Redistributor;
use aarch64defs::MpidrEl1;
use aarch64defs::SystemReg;
use aarch64defs::gic::GicdCtlr;
use aarch64defs::gic::GicdRegister;
use aarch64defs::gic::GicrSgi;
use aarch64defs::gic::GicrSgiRegister;
use aarch64defs::gic::IccCtlrEl1;
use memory_range::MemoryRange;

const GICD_BASE: u64 = 0;
const SGI_FRAME: u64 = 0x1_0000;
const TEST_INTIDS: u32 = 8;
const PRIORITY_MASK: u8 = 0xf8;

fn no_wake(_: usize) {}

fn new_system(cpu_count: usize) -> (Distributor, Vec<Redistributor>) {
    let gicr_base = aarch64defs::GIC_DISTRIBUTOR_SIZE;
    let gicr_range = MemoryRange::new(
        gicr_base..gicr_base + cpu_count as u64 * aarch64defs::GIC_REDISTRIBUTOR_SIZE,
    );
    let mut dist = Distributor::new(GICD_BASE, gicr_range, 32);
    let redists = (0..cpu_count)
        .map(|index| {
            let mpidr: u64 = MpidrEl1::new().with_aff0(index as u8).into();
            dist.add_redistributor(mpidr, index + 1 == cpu_count)
        })
        .collect();
    (dist, redists)
}

fn set_distributor_groups(dist: &Distributor, group0: bool, group1: bool) {
    let ctlr: u32 = GicdCtlr::new()
        .with_enable_grp0(group0)
        .with_enable_grp1(group1)
        .into();
    assert!(dist.write(GICD_BASE + GicdRegister::CTLR.0 as u64, &ctlr.to_ne_bytes()));
}

fn online(dist: &Distributor, redists: &mut [Redistributor]) {
    set_distributor_groups(dist, true, true);
    for redist in redists {
        assert!(dist.write_sysreg(redist, SystemReg::ICC_PMR_EL1, 0xff, no_wake));
        assert!(dist.write_sysreg(redist, SystemReg::ICC_IGRPEN0_EL1, 1, no_wake));
        assert!(dist.write_sysreg(redist, SystemReg::ICC_IGRPEN1_EL1, 1, no_wake));
    }
}

#[test]
fn targeted_sgi_reaches_only_the_selected_pe() {
    let (dist, mut redists) = new_system(4);
    online(&dist, &mut redists);
    let sgi: u64 = GicrSgi::new().with_intid(3).with_target_list(1 << 1).into();
    let mut woken = Vec::new();

    assert!(
        dist.write_sysreg(&mut redists[0], SystemReg::ICC_SGI1R_EL1, sgi, |index| {
            woken.push(index)
        })
    );
    assert_eq!(woken, [1]);
    assert!(dist.irq_pending(&redists[1]));
    assert!(!dist.irq_pending(&redists[0]));
    assert!(!dist.irq_pending(&redists[2]));
    assert!(!dist.irq_pending(&redists[3]));
    assert_eq!(
        dist.read_sysreg(&mut redists[1], SystemReg::ICC_IAR1_EL1),
        Some(3)
    );
    assert!(dist.write_sysreg(&mut redists[1], SystemReg::ICC_EOIR1_EL1, 3, no_wake));
    assert!(!dist.irq_pending(&redists[1]));
}

#[test]
fn sgi_group_mismatch_is_not_generated() {
    let (dist, mut redists) = new_system(2);
    online(&dist, &mut redists);
    // Redistributor SGIs reset to Group 1 in this Hyper-V model.
    let sgi: u64 = GicrSgi::new().with_intid(3).with_target_list(1 << 1).into();
    let mut woken = Vec::new();

    assert!(
        dist.write_sysreg(&mut redists[0], SystemReg::ICC_SGI0R_EL1, sgi, |index| {
            woken.push(index)
        })
    );
    assert!(woken.is_empty());
    assert!(!dist.irq_pending(&redists[1]));
}

#[test]
fn cpu_group_change_notifies_all_possible_one_of_n_targets() {
    let (dist, mut redists) = new_system(3);
    let mut woken = Vec::new();

    assert!(
        dist.write_sysreg(&mut redists[0], SystemReg::ICC_IGRPEN1_EL1, 1, |index| {
            woken.push(index)
        })
    );
    assert_eq!(woken, [0, 1, 2]);
}

#[test]
fn broadcast_sgi_reaches_every_pe_except_the_sender() {
    let (dist, mut redists) = new_system(4);
    online(&dist, &mut redists);
    let sgi: u64 = GicrSgi::new().with_intid(2).with_irm(true).into();
    let mut woken = Vec::new();

    assert!(
        dist.write_sysreg(&mut redists[2], SystemReg::ICC_SGI1R_EL1, sgi, |index| {
            woken.push(index)
        })
    );
    assert_eq!(woken, [0, 1, 3]);
    assert!(!dist.irq_pending(&redists[2]));
    for cpu in [0, 1, 3] {
        assert!(dist.irq_pending(&redists[cpu]));
        assert_eq!(
            dist.read_sysreg(&mut redists[cpu], SystemReg::ICC_IAR1_EL1),
            Some(2)
        );
        assert!(dist.write_sysreg(&mut redists[cpu], SystemReg::ICC_EOIR1_EL1, 2, no_wake));
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    pending: u32,
    active: u32,
    group: u32,
    priority: [u8; TEST_INTIDS as usize],
    pmr: u8,
    bpr0: u8,
    bpr1: u8,
    grpen0: bool,
    grpen1: bool,
    cbpr: bool,
    eoimode: bool,
    ap0r0: u32,
    ap1r0: u32,
    rpr: u8,
    dist_group0: bool,
    dist_group1: bool,
    irq_pending: bool,
}

fn read_gicr_word(redist: &Redistributor, register: GicrSgiRegister) -> u32 {
    let mut bytes = [0; 4];
    redist
        .shared
        .read(SGI_FRAME + register.0 as u64, &mut bytes);
    u32::from_ne_bytes(bytes)
}

fn real_snapshot(dist: &Distributor, redist: &Redistributor) -> Snapshot {
    let mut priority = [0; TEST_INTIDS as usize];
    for word in 0..2 {
        let mut bytes = [0; 4];
        redist.shared.read(
            SGI_FRAME + GicrSgiRegister::IPRIORITYR0.0 as u64 + word * 4,
            &mut bytes,
        );
        priority[word as usize * 4..word as usize * 4 + 4].copy_from_slice(&bytes);
    }

    let cpu_ctlr = IccCtlrEl1::from(redist.read_cpuif(SystemReg::ICC_CTLR_EL1).unwrap());
    let mut dist_ctlr = [0; 4];
    assert!(dist.read(GICD_BASE + GicdRegister::CTLR.0 as u64, &mut dist_ctlr));
    let dist_ctlr = GicdCtlr::from(u32::from_ne_bytes(dist_ctlr));

    Snapshot {
        pending: read_gicr_word(redist, GicrSgiRegister::ISPENDR0),
        active: read_gicr_word(redist, GicrSgiRegister::ISACTIVER0),
        group: read_gicr_word(redist, GicrSgiRegister::IGROUPR0),
        priority,
        pmr: redist.read_cpuif(SystemReg::ICC_PMR_EL1).unwrap() as u8,
        bpr0: redist.read_cpuif(SystemReg::ICC_BPR0_EL1).unwrap() as u8,
        bpr1: redist.read_cpuif(SystemReg::ICC_BPR1_EL1).unwrap() as u8,
        grpen0: redist.read_cpuif(SystemReg::ICC_IGRPEN0_EL1).unwrap() != 0,
        grpen1: redist.read_cpuif(SystemReg::ICC_IGRPEN1_EL1).unwrap() != 0,
        cbpr: cpu_ctlr.cbpr(),
        eoimode: cpu_ctlr.eoi_mode(),
        ap0r0: redist.read_cpuif(SystemReg::ICC_AP0R0_EL1).unwrap() as u32,
        ap1r0: redist.read_cpuif(SystemReg::ICC_AP1R0_EL1).unwrap() as u32,
        rpr: redist.read_cpuif(SystemReg::ICC_RPR_EL1).unwrap() as u8,
        dist_group0: dist_ctlr.enable_grp0(),
        dist_group1: dist_ctlr.enable_grp1(),
        irq_pending: dist.irq_pending(redist),
    }
}

#[derive(Clone)]
struct Reference {
    pending: u32,
    active: u32,
    group: u32,
    priority: [u8; TEST_INTIDS as usize],
    pmr: u8,
    bpr0: u8,
    bpr1: u8,
    grpen0: bool,
    grpen1: bool,
    cbpr: bool,
    eoimode: bool,
    ap0r0: u32,
    ap1r0: u32,
    dist_group0: bool,
    dist_group1: bool,
}

impl Reference {
    fn new() -> Self {
        Self {
            pending: 0,
            active: 0,
            group: u32::MAX,
            priority: [0; TEST_INTIDS as usize],
            pmr: 0,
            bpr0: 2,
            bpr1: 3,
            grpen0: false,
            grpen1: false,
            cbpr: false,
            eoimode: false,
            ap0r0: 0,
            ap1r0: 0,
            dist_group0: false,
            dist_group1: false,
        }
    }

    fn running_priority(apr: u32) -> u8 {
        if apr == 0 {
            0xff
        } else {
            (apr.trailing_zeros() as u8) << 3
        }
    }

    fn group_priority(priority: u8, bpr: u8) -> u8 {
        priority & (0xffu16 << (bpr + 1)) as u8
    }

    fn effective_bpr(&self, group1: bool) -> u8 {
        if group1 && !self.cbpr {
            self.bpr1.saturating_sub(1)
        } else {
            self.bpr0
        }
    }

    fn best(&self, group1: bool) -> Option<(u32, u8)> {
        let groups = if group1 { self.group } else { !self.group };
        // This model uses only SGIs, which are permanently enabled.
        let mut deliverable = self.pending & !self.active & 0xffff & groups;
        let mut best = None;
        while deliverable != 0 {
            let intid = deliverable.trailing_zeros();
            deliverable &= deliverable - 1;
            if intid >= TEST_INTIDS {
                continue;
            }
            let priority = self.priority[intid as usize];
            if best.is_none_or(|(_, best_priority)| priority < best_priority) {
                best = Some((intid, priority));
            }
        }
        best
    }

    fn select(&self, group1: bool) -> Option<(u32, u8)> {
        let dist_enabled = if group1 {
            self.dist_group1
        } else {
            self.dist_group0
        };
        let cpu_enabled = if group1 { self.grpen1 } else { self.grpen0 };
        if !dist_enabled || !cpu_enabled {
            return None;
        }
        let (intid, priority) = self.best(group1)?;
        if priority >= self.pmr {
            return None;
        }
        let group_priority = Self::group_priority(priority, self.effective_bpr(group1));
        let running = Self::running_priority(self.ap0r0).min(Self::running_priority(self.ap1r0));
        let preemption_level = if running == 0xff {
            0xff
        } else {
            Self::group_priority(running, self.effective_bpr(group1))
        };
        (group_priority < preemption_level).then_some((intid, group_priority))
    }

    fn ack(&mut self, group1: bool) -> u32 {
        let Some((intid, group_priority)) = self.select(group1) else {
            return 1023;
        };
        self.pending &= !(1 << intid);
        self.active |= 1 << intid;
        let bit = 1 << (group_priority >> 3);
        if group1 {
            self.ap1r0 |= bit;
        } else {
            self.ap0r0 |= bit;
        }
        intid
    }

    fn pop_priority(&mut self, group1: bool) {
        let apr = if group1 {
            &mut self.ap1r0
        } else {
            &mut self.ap0r0
        };
        if *apr != 0 {
            *apr &= *apr - 1;
        }
    }

    fn eoi(&mut self, group1: bool, intid: u32) {
        if intid >= 1020 {
            return;
        }
        self.pop_priority(group1);
        if !self.eoimode {
            self.active &= !(1 << intid);
        }
    }

    fn dir(&mut self, intid: u32) {
        if intid < 1020 && self.eoimode {
            self.active &= !(1 << intid);
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            pending: self.pending,
            active: self.active,
            group: self.group,
            priority: self.priority,
            pmr: self.pmr,
            bpr0: self.bpr0,
            bpr1: if self.cbpr {
                self.bpr0.saturating_add(1).min(7)
            } else {
                self.bpr1
            },
            grpen0: self.grpen0,
            grpen1: self.grpen1,
            cbpr: self.cbpr,
            eoimode: self.eoimode,
            ap0r0: self.ap0r0,
            ap1r0: self.ap1r0,
            rpr: Self::running_priority(self.ap0r0).min(Self::running_priority(self.ap1r0)),
            dist_group0: self.dist_group0,
            dist_group1: self.dist_group1,
            irq_pending: self.select(false).is_some() || self.select(true).is_some(),
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn below(&mut self, upper: u32) -> u32 {
        (self.next() % u64::from(upper)) as u32
    }

    fn boolean(&mut self) -> bool {
        self.next() & 1 != 0
    }
}

#[test]
fn sequential_operations_match_an_independent_model() {
    const SEEDS: u64 = 64;
    const STEPS: usize = 256;

    for seed_index in 0..SEEDS {
        let seed = 0xc0ffee_u64.wrapping_mul(seed_index + 1) ^ (seed_index << 32);
        let mut rng = Rng(seed);
        let (dist, mut redists) = new_system(1);
        let redist = &mut redists[0];
        let mut reference = Reference::new();

        for step in 0..STEPS {
            let operation = match rng.below(98) {
                0..=19 => {
                    let intid = rng.below(TEST_INTIDS);
                    redist.raise(intid);
                    reference.pending |= 1 << intid;
                    format!("raise {intid}")
                }
                20..=31 => {
                    let intid = rng.below(TEST_INTIDS);
                    let priority = rng.next() as u8;
                    redist.shared.write(
                        SGI_FRAME + GicrSgiRegister::IPRIORITYR0.0 as u64 + u64::from(intid),
                        &[priority],
                    );
                    reference.priority[intid as usize] = priority & PRIORITY_MASK;
                    format!("priority {intid}={priority:#x}")
                }
                32..=39 => {
                    let group = rng.next() as u32;
                    redist.shared.write(
                        SGI_FRAME + GicrSgiRegister::IGROUPR0.0 as u64,
                        &group.to_ne_bytes(),
                    );
                    reference.group = group;
                    format!("group={group:#x}")
                }
                40..=47 => {
                    let pmr = rng.next() as u8;
                    assert!(dist.write_sysreg(
                        redist,
                        SystemReg::ICC_PMR_EL1,
                        u64::from(pmr),
                        no_wake
                    ));
                    reference.pmr = pmr & PRIORITY_MASK;
                    format!("pmr={pmr:#x}")
                }
                48..=55 => {
                    let group1 = rng.boolean();
                    let value = rng.below(8) as u8;
                    let reg = if group1 {
                        if !reference.cbpr {
                            reference.bpr1 = value.max(3);
                        }
                        SystemReg::ICC_BPR1_EL1
                    } else {
                        reference.bpr0 = value.max(2);
                        SystemReg::ICC_BPR0_EL1
                    };
                    assert!(dist.write_sysreg(redist, reg, u64::from(value), no_wake));
                    format!("bpr{}={value}", u8::from(group1))
                }
                56..=64 => {
                    let group1 = rng.boolean();
                    let enabled = rng.boolean();
                    let reg = if group1 {
                        reference.grpen1 = enabled;
                        SystemReg::ICC_IGRPEN1_EL1
                    } else {
                        reference.grpen0 = enabled;
                        SystemReg::ICC_IGRPEN0_EL1
                    };
                    assert!(dist.write_sysreg(redist, reg, u64::from(enabled), no_wake));
                    format!("cpu_group{}={enabled}", u8::from(group1))
                }
                65..=72 => {
                    reference.dist_group0 = rng.boolean();
                    reference.dist_group1 = rng.boolean();
                    set_distributor_groups(&dist, reference.dist_group0, reference.dist_group1);
                    format!(
                        "dist_groups={},{}",
                        reference.dist_group0, reference.dist_group1
                    )
                }
                73..=79 => {
                    reference.cbpr = rng.boolean();
                    reference.eoimode = rng.boolean();
                    let value = u64::from(reference.cbpr) | (u64::from(reference.eoimode) << 1);
                    assert!(dist.write_sysreg(redist, SystemReg::ICC_CTLR_EL1, value, no_wake));
                    format!("ctlr cbpr={} eoimode={}", reference.cbpr, reference.eoimode)
                }
                80..=84 => {
                    let group1 = rng.boolean();
                    let value = rng.next() as u32;
                    let reg = if group1 {
                        reference.ap1r0 = value;
                        SystemReg::ICC_AP1R0_EL1
                    } else {
                        reference.ap0r0 = value;
                        SystemReg::ICC_AP0R0_EL1
                    };
                    assert!(dist.write_sysreg(redist, reg, u64::from(value), no_wake));
                    format!("ap{}={value:#x}", u8::from(group1))
                }
                85..=90 => {
                    let group1 = rng.boolean();
                    let reg = if group1 {
                        SystemReg::ICC_IAR1_EL1
                    } else {
                        SystemReg::ICC_IAR0_EL1
                    };
                    let actual = dist.read_sysreg(redist, reg).unwrap() as u32;
                    let expected = reference.ack(group1);
                    assert_eq!(
                        actual, expected,
                        "seed={seed:#x} step={step} ack group1={group1}"
                    );
                    format!("ack group1={group1} -> {actual}")
                }
                91..=94 => {
                    let group1 = rng.boolean();
                    let intid = if rng.below(8) == 0 {
                        1023
                    } else {
                        rng.below(TEST_INTIDS)
                    };
                    let reg = if group1 {
                        SystemReg::ICC_EOIR1_EL1
                    } else {
                        SystemReg::ICC_EOIR0_EL1
                    };
                    assert!(dist.write_sysreg(redist, reg, u64::from(intid), no_wake));
                    reference.eoi(group1, intid);
                    format!("eoi group1={group1} intid={intid}")
                }
                95..=97 => {
                    let intid = if rng.below(8) == 0 {
                        1023
                    } else {
                        rng.below(TEST_INTIDS)
                    };
                    assert!(dist.write_sysreg(
                        redist,
                        SystemReg::ICC_DIR_EL1,
                        u64::from(intid),
                        no_wake
                    ));
                    reference.dir(intid);
                    format!("dir intid={intid}")
                }
                _ => unreachable!(),
            };

            assert_eq!(
                real_snapshot(&dist, redist),
                reference.snapshot(),
                "seed={seed:#x} step={step} operation={operation}"
            );
        }
    }
}
