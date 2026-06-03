// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared NVMe controller register state and BAR0 I/O handling.
//!
//! Both PF ([`super::NvmeController`]) and VF
//! ([`super::vf::NvmeVirtualFunction`]) are NVMe controllers with identical
//! register layouts and largely identical register handling logic. This module
//! extracts the common parts behind the [`NvmeRegisterIo`] trait.

use crate::CAP;
use crate::DOORBELL_STRIDE_BITS;
use crate::IOCQES;
use crate::IOSQES;
use crate::NVME_VERSION;
use crate::PAGE_MASK;
use crate::spec;
use crate::workers::IoQueueEntrySizes;
use chipset_device::io::IoError;
use chipset_device::io::IoError::InvalidRegister;
use chipset_device::io::IoResult;
use inspect::Inspect;
use parking_lot::Mutex;

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

/// Operations that differ between PF and VF NVMe controllers.
///
/// The register read/write/CC handling is identical between PF and VF. The
/// differences are in doorbell dispatch, controller enable/disable, and
/// enable/reset completion polling. Implementors provide these; the shared
/// register handling is in [`read_bar0`] and [`write_bar0`].
pub(crate) trait NvmeRegisterIo {
    fn registers(&self) -> &RegState;
    fn registers_mut(&mut self) -> &mut RegState;
    fn qe_sizes(&self) -> &Mutex<IoQueueEntrySizes>;

    /// Dispatch a doorbell write. No-op if the controller is not enabled.
    fn doorbell(&self, db_id: u16, value: u32);

    /// Enable the controller (CC.EN 0→1).
    fn enable_controller(&mut self);

    /// Initiate controller reset (CC.EN 1→0).
    fn reset_controller(&mut self);

    /// Poll whether enable has completed. Returns true when ready.
    fn poll_enabled(&mut self) -> bool;

    /// Poll whether reset has completed. Returns true when done.
    /// Implementations may perform cleanup (e.g., dropping workers).
    fn poll_reset(&mut self) -> bool;
}

/// Read from NVMe BAR0 (controller registers).
pub(crate) fn read_bar0(ctrl: &mut impl NvmeRegisterIo, addr: u64, data: &mut [u8]) -> IoResult {
    if data.len() < 4 {
        return IoResult::Err(IoError::InvalidAccessSize);
    }
    if addr & (data.len() as u64 - 1) != 0 {
        return IoResult::Err(IoError::UnalignedAccess);
    }

    // 64-bit registers.
    let d: Option<u64> = match spec::Register(addr & !7) {
        spec::Register::CAP => Some(CAP.into()),
        spec::Register::ASQ => Some(ctrl.registers().asq),
        spec::Register::ACQ => Some(ctrl.registers().acq),
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
        spec::Register::INTMS => ctrl.registers().interrupt_mask,
        spec::Register::INTMC => ctrl.registers().interrupt_mask,
        spec::Register::CC => ctrl.registers().cc.into(),
        spec::Register::RESERVED => 0,
        spec::Register::CSTS => get_csts(ctrl),
        spec::Register::NSSR => 0,
        spec::Register::AQA => ctrl.registers().aqa.into(),
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
pub(crate) fn write_bar0(ctrl: &mut impl NvmeRegisterIo, addr: u64, data: &[u8]) -> IoResult {
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
        ctrl.doorbell(db_id, value);
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
            if !ctrl.registers().cc.en() {
                let asq = ctrl.registers().asq;
                ctrl.registers_mut().asq = update_reg(asq) & PAGE_MASK;
            } else {
                tracelimit::warn_ratelimited!("attempt to set ASQ while enabled");
            }
            true
        }
        spec::Register::ACQ => {
            if !ctrl.registers().cc.en() {
                let acq = ctrl.registers().acq;
                ctrl.registers_mut().acq = update_reg(acq) & PAGE_MASK;
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
        spec::Register::INTMS => ctrl.registers_mut().interrupt_mask |= data,
        spec::Register::INTMC => ctrl.registers_mut().interrupt_mask &= !data,
        spec::Register::CC => set_cc(ctrl, data.into()),
        spec::Register::AQA => ctrl.registers_mut().aqa = data.into(),
        _ => return IoResult::Err(InvalidRegister),
    }
    IoResult::Ok
}

/// Process a write to the CC register.
fn set_cc(ctrl: &mut impl NvmeRegisterIo, cc: spec::Cc) {
    tracing::debug!(?cc, "set cc");

    if cc.mps() != 0 {
        tracelimit::warn_ratelimited!("only 4K page size supported");
        ctrl.registers_mut().csts.set_cfs(true);
        return;
    }

    if cc.css() != 0 {
        tracelimit::warn_ratelimited!("only NVM command set supported");
        ctrl.registers_mut().csts.set_cfs(true);
        return;
    }

    if let 2..=6 = cc.ams() {
        tracelimit::warn_ratelimited!("undefined arbitration mechanism");
        ctrl.registers_mut().csts.set_cfs(true);
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
        ctrl.registers_mut().csts.set_shst(0b10);
    }

    if cc.en() != ctrl.registers().cc.en() {
        if cc.en() {
            // Some drivers write zeros to IOSQES/IOCQES, assuming defaults.
            if cc.iocqes() == 0 {
                cc.set_iocqes(IOCQES);
            } else if cc.iocqes() != IOCQES {
                tracelimit::warn_ratelimited!("unsupported CQE size");
                ctrl.registers_mut().csts.set_cfs(true);
                return;
            }

            if cc.iosqes() == 0 {
                cc.set_iosqes(IOSQES);
            } else if cc.iosqes() != IOSQES {
                tracelimit::warn_ratelimited!("unsupported SQE size");
                ctrl.registers_mut().csts.set_cfs(true);
                return;
            }

            if ctrl.registers().csts.rdy() {
                tracelimit::warn_ratelimited!("enabling during reset");
                return;
            }
            if cc.shn() == 0 {
                ctrl.registers_mut().csts.set_shst(0);
            }

            ctrl.enable_controller();
        } else if ctrl.registers().csts.rdy() {
            ctrl.reset_controller();
        } else {
            tracelimit::warn_ratelimited!("disabling while not ready");
            return;
        }
    }

    ctrl.registers_mut().cc = cc;
    *ctrl.qe_sizes().lock() = IoQueueEntrySizes {
        sqe_bits: cc.iosqes(),
        cqe_bits: cc.iocqes(),
    };
}

/// Poll CSTS, driving the enable/reset state machine forward.
fn get_csts(ctrl: &mut impl NvmeRegisterIo) -> u32 {
    if !ctrl.registers().cc.en() && ctrl.registers().csts.rdy() {
        if ctrl.poll_reset() {
            // AQA, ASQ, and ACQ are not reset by controller reset.
            let regs = ctrl.registers_mut();
            regs.csts = 0.into();
            regs.cc = 0.into();
            regs.interrupt_mask = 0;
        }
    } else if ctrl.registers().cc.en() && !ctrl.registers().csts.rdy() {
        if ctrl.poll_enabled() {
            ctrl.registers_mut().csts.set_rdy(true);
        }
    }

    ctrl.registers().csts.into()
}
