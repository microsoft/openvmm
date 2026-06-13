// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared NVMe controller register state and BAR0 I/O handling.
//!
//! Both PF ([`super::NvmeController`]) and VF
//! ([`super::vf::NvmeVirtualFunction`]) are NVMe controllers with identical
//! register layouts and largely identical register handling logic. This module
//! extracts the common parts into [`ControllerCore`], which each embeds.

use crate::CAP;
use crate::DOORBELL_STRIDE_BITS;
use crate::IOCQES;
use crate::IOSQES;
use crate::NVME_VERSION;
use crate::PAGE_MASK;
use crate::spec;
use crate::workers::EnablePoll;
use crate::workers::IoQueueEntrySizes;
use crate::workers::NvmeWorkers;
use chipset_device::io::IoError;
use chipset_device::io::IoError::InvalidRegister;
use chipset_device::io::IoResult;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;

/// NVMe controller register state.
#[derive(Inspect)]
pub(crate) struct RegState {
    #[inspect(hex)]
    pub interrupt_mask: u32,
    pub cc: spec::Cc,
    pub csts: spec::Csts,
    pub aqa: spec::Aqa,
    #[inspect(hex)]
    pub asq: u64,
    #[inspect(hex)]
    pub acq: u64,
}

impl RegState {
    pub fn new() -> Self {
        Self {
            interrupt_mask: 0,
            cc: spec::Cc::new(),
            csts: spec::Csts::new(),
            aqa: spec::Aqa::new(),
            asq: 0,
            acq: 0,
        }
    }
}

/// The common state machine shared by PF ([`super::NvmeController`]) and VF
/// ([`super::vf::NvmeVirtualFunction`]) NVMe controllers.
///
/// Both controllers have identical register layouts and an identical
/// [`NvmeWorkers`] coordinator; only their surrounding PCI/SR-IOV state
/// differs. Each embeds one of these and forwards BAR0 reads/writes to
/// [`ControllerCore::read_bar0`] / [`ControllerCore::write_bar0`].
#[derive(Inspect)]
pub(crate) struct ControllerCore {
    pub registers: RegState,
    #[inspect(skip)]
    pub qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(flatten)]
    pub workers: NvmeWorkers,
}

impl ControllerCore {
    pub fn new(qe_sizes: Arc<Mutex<IoQueueEntrySizes>>, workers: NvmeWorkers) -> Self {
        Self {
            registers: RegState::new(),
            qe_sizes,
            workers,
        }
    }
}

/// Read from NVMe BAR0 (controller registers).
impl ControllerCore {
    pub(crate) fn read_bar0(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        if data.len() < 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if addr & (data.len() as u64 - 1) != 0 {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        // 64-bit registers.
        let d: Option<u64> = match spec::Register(addr & !7) {
            spec::Register::CAP => Some(CAP.into()),
            spec::Register::ASQ => Some(self.registers.asq),
            spec::Register::ACQ => Some(self.registers.acq),
            spec::Register::BPMBL => Some(0),
            _ => None,
        };
        if let Some(d) = d {
            if data.len() == 8 {
                data.copy_from_slice(&d.to_ne_bytes());
            } else if addr & 7 == 0 {
                data.copy_from_slice(&(d as u32).to_ne_bytes());
            } else {
                data.copy_from_slice(&((d >> 32) as u32).to_ne_bytes());
            }
            return IoResult::Ok;
        }

        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        // 32-bit registers.
        let d: u32 = match spec::Register(addr) {
            spec::Register::VS => NVME_VERSION,
            spec::Register::INTMS => self.registers.interrupt_mask,
            spec::Register::INTMC => self.registers.interrupt_mask,
            spec::Register::CC => self.registers.cc.into(),
            spec::Register::RESERVED => 0,
            spec::Register::CSTS => self.get_csts(),
            spec::Register::NSSR => 0,
            spec::Register::AQA => self.registers.aqa.into(),
            spec::Register::CMBLOC => 0,
            spec::Register::CMBSZ => 0,
            spec::Register::BPINFO => 0,
            spec::Register::BPRSEL => 0,
            _ => return IoResult::Err(InvalidRegister),
        };
        data.copy_from_slice(&d.to_ne_bytes());
        IoResult::Ok
    }

    /// Write to NVMe BAR0 (controller registers + doorbells).
    pub(crate) fn write_bar0(&mut self, addr: u64, data: &[u8]) -> IoResult {
        if addr >= 0x1000 {
            // Doorbell write.
            let base = addr - 0x1000;
            let db_id = base >> DOORBELL_STRIDE_BITS;
            if (db_id << DOORBELL_STRIDE_BITS) != base {
                return IoResult::Err(InvalidRegister);
            }
            let Ok(data) = data.try_into() else {
                return IoResult::Err(IoError::InvalidAccessSize);
            };
            let value = u32::from_ne_bytes(data);
            let db_id = match u16::try_from(db_id) {
                Ok(id) => id,
                Err(_) => return IoResult::Err(InvalidRegister),
            };
            self.workers.doorbell(db_id, value);
            return IoResult::Ok;
        }

        if data.len() < 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if addr & (data.len() as u64 - 1) != 0 {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        let update_reg = |x: u64| {
            if data.len() == 8 {
                u64::from_ne_bytes(data.try_into().unwrap())
            } else {
                let data = u32::from_ne_bytes(data.try_into().unwrap()) as u64;
                if addr & 7 == 0 {
                    (x & !(u32::MAX as u64)) | data
                } else {
                    (x & u32::MAX as u64) | (data << 32)
                }
            }
        };

        // 64-bit registers.
        let handled = match spec::Register(addr & !7) {
            spec::Register::ASQ => {
                if !self.registers.cc.en() {
                    let asq = self.registers.asq;
                    self.registers.asq = update_reg(asq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set ASQ while enabled");
                }
                true
            }
            spec::Register::ACQ => {
                if !self.registers.cc.en() {
                    let acq = self.registers.acq;
                    self.registers.acq = update_reg(acq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set ACQ while enabled");
                }
                true
            }
            _ => false,
        };
        if handled {
            return IoResult::Ok;
        }

        let Ok(data) = data.try_into() else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };
        let data = u32::from_ne_bytes(data);

        // 32-bit registers.
        match spec::Register(addr) {
            spec::Register::INTMS => self.registers.interrupt_mask |= data,
            spec::Register::INTMC => self.registers.interrupt_mask &= !data,
            spec::Register::CC => self.set_cc(data.into()),
            spec::Register::AQA => self.registers.aqa = data.into(),
            _ => return IoResult::Err(InvalidRegister),
        }
        IoResult::Ok
    }

    /// Process a write to the CC register.
    fn set_cc(&mut self, cc: spec::Cc) {
        tracing::debug!(?cc, "set cc");

        if cc.mps() != 0 {
            tracelimit::warn_ratelimited!("only 4K page size supported");
            self.registers.csts.set_cfs(true);
            return;
        }

        if cc.css() != 0 {
            tracelimit::warn_ratelimited!("only NVM command set supported");
            self.registers.csts.set_cfs(true);
            return;
        }

        if let 2..=6 = cc.ams() {
            tracelimit::warn_ratelimited!("undefined arbitration mechanism");
            self.registers.csts.set_cfs(true);
        }

        let mask: u32 = u32::from(
            spec::Cc::new()
                .with_en(true)
                .with_shn(0b11)
                .with_iosqes(0b1111)
                .with_iocqes(0b1111),
        );
        let mut cc: spec::Cc = (u32::from(cc) & mask).into();

        if cc.shn() != 0 {
            // Complete shutdown immediately.
            self.registers.csts.set_shst(0b10);
        }

        if cc.en() != self.registers.cc.en() {
            if cc.en() {
                // Some drivers write zeros to IOSQES/IOCQES, assuming defaults.
                if cc.iocqes() == 0 {
                    cc.set_iocqes(IOCQES);
                } else if cc.iocqes() != IOCQES {
                    tracelimit::warn_ratelimited!("unsupported CQE size");
                    self.registers.csts.set_cfs(true);
                    return;
                }

                if cc.iosqes() == 0 {
                    cc.set_iosqes(IOSQES);
                } else if cc.iosqes() != IOSQES {
                    tracelimit::warn_ratelimited!("unsupported SQE size");
                    self.registers.csts.set_cfs(true);
                    return;
                }

                if self.registers.csts.rdy() {
                    tracelimit::warn_ratelimited!("enabling during reset");
                    return;
                }
                if cc.shn() == 0 {
                    self.registers.csts.set_shst(0);
                }

                // The online gate lives in the coordinator. If the controller
                // is offline (only possible for an SR-IOV VF whose secondary
                // controller has not been brought online), the enable is
                // rejected and `get_csts` sets CFS rather than RDY.
                let regs = &self.registers;
                let asq = regs.asq;
                let asqs = regs.aqa.asqs_z().max(1) + 1;
                let acq = regs.acq;
                let acqs = regs.aqa.acqs_z().max(1) + 1;
                self.workers.enable(asq, asqs, acq, acqs);
            } else if self.registers.csts.rdy() {
                self.workers.controller_reset();
            } else {
                tracelimit::warn_ratelimited!("disabling while not ready");
                return;
            }
        }

        self.registers.cc = cc;
        *self.qe_sizes.lock() = IoQueueEntrySizes {
            sqe_bits: cc.iosqes(),
            cqe_bits: cc.iocqes(),
        };
    }

    /// Poll CSTS, driving the enable/reset state machine forward.
    fn get_csts(&mut self) -> u32 {
        if !self.registers.cc.en() && self.registers.csts.rdy() {
            if self.workers.poll_controller_reset() {
                // AQA, ASQ, and ACQ are not reset by controller reset.
                let regs = &mut self.registers;
                regs.csts = 0.into();
                regs.cc = 0.into();
                regs.interrupt_mask = 0;
            }
        } else if self.registers.cc.en() && !self.registers.csts.rdy() && !self.registers.csts.cfs()
        {
            match self.workers.poll_enabled() {
                EnablePoll::Enabled => self.registers.csts.set_rdy(true),
                EnablePoll::Pending => {}
                EnablePoll::Rejected => {
                    // The controller is offline (an SR-IOV VF whose secondary
                    // controller is not online). Signal a fatal error and never
                    // reach RDY. The PF is always online, so it never lands
                    // here; if it somehow did, CFS makes the failure observable
                    // rather than hanging silently.
                    tracelimit::warn_ratelimited!("controller enable rejected: offline");
                    self.registers.csts.set_cfs(true);
                }
            }
        }

        self.registers.csts.into()
    }
}
