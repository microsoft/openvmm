// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall exit handling.

use crate::abi;
use crate::HvfProcessor;
use hv1_hypercall::Arm64RegisterState;
use hv1_hypercall::GetVpRegisters;
use hv1_hypercall::HvRepResult;
use hv1_hypercall::PostMessage;
use hv1_hypercall::SetVpRegisters;
use hv1_hypercall::SignalEvent;
use hvdef::hypercall::HvRegisterAssoc;
use hvdef::HvArm64RegisterName;
use hvdef::HvError;
use hvdef::Vtl;
use std::sync::atomic::Ordering;
use virt::io::CpuIo;

pub(crate) struct HvfHypercallHandler<'a, 'b, T> {
    vp: &'a mut HvfProcessor<'b>,
    cpu_io: &'a T,
}

impl<'a, 'b, T: CpuIo> HvfHypercallHandler<'a, 'b, T> {
    pub const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvGetVpRegisters,
            hv1_hypercall::HvSetVpRegisters,
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
        ]
    );

    pub fn new(vp: &'a mut HvfProcessor<'b>, cpu_io: &'a T) -> Self {
        Self { vp, cpu_io }
    }
}

impl<T: CpuIo> Arm64RegisterState for HvfHypercallHandler<'_, '_, T> {
    fn pc(&mut self) -> u64 {
        self.vp.vcpu.reg(abi::HvReg::PC).expect("cannot fail")
    }

    fn set_pc(&mut self, pc: u64) {
        tracing::trace!(pc, "set pc");
        self.vp
            .vcpu
            .set_reg(abi::HvReg::PC, pc)
            .expect("cannot fail");
    }

    fn x(&mut self, n: u8) -> u64 {
        self.vp.vcpu.gp(n).expect("cannot fail")
    }

    fn set_x(&mut self, n: u8, v: u64) {
        tracing::trace!(n, v, "set x");
        self.vp.vcpu.set_gp(n, v).expect("cannot fail")
    }
}

impl<T> GetVpRegisters for HvfHypercallHandler<'_, '_, T> {
    fn get_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        _vtl: Option<Vtl>,
        registers: &[hvdef::HvRegisterName],
        output: &mut [hvdef::HvRegisterValue],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF || vp_index != hvdef::HV_VP_INDEX_SELF {
            return Err((HvError::InvalidParameter, 0));
        }

        for (i, (&name, output)) in registers.iter().zip(output).enumerate() {
            *output = match name.into() {
                HvArm64RegisterName::TimeRefCount => {
                    self.vp.partition.vmtime.now().as_100ns().into()
                }
                HvArm64RegisterName::VpIndex => 0u32.into(),
                HvArm64RegisterName::GuestOsId => self
                    .vp
                    .partition
                    .hv1
                    .guest_os_id
                    .load(Ordering::Relaxed)
                    .into(),
                HvArm64RegisterName::Sipp => self.vp.hv1.simp().into(),
                HvArm64RegisterName::Sifp => self.vp.hv1.siefp().into(),
                HvArm64RegisterName::Scontrol => self.vp.hv1.scontrol().into(),
                r if (HvArm64RegisterName::Sint0..=HvArm64RegisterName::Sint15).contains(&r) => {
                    self.vp
                        .hv1
                        .sint((r.0 - HvArm64RegisterName::Sint0.0) as u8)
                        .into()
                }

                HvArm64RegisterName::HypervisorVersion => 0u128.into(),
                HvArm64RegisterName::PrivilegesAndFeaturesInfo => 0u128.into(),
                HvArm64RegisterName::FeaturesInfo => 0u128.into(),

                register => {
                    tracing::warn!(?register, "unsupported register get");
                    return Err((HvError::InvalidParameter, i));
                }
            }
        }
        Ok(())
    }
}

impl<T> SetVpRegisters for HvfHypercallHandler<'_, '_, T> {
    fn set_vp_registers(
        &mut self,
        partition_id: u64,
        vp_index: u32,
        _vtl: Option<Vtl>,
        registers: &[HvRegisterAssoc],
    ) -> HvRepResult {
        if partition_id != hvdef::HV_PARTITION_ID_SELF || vp_index != hvdef::HV_VP_INDEX_SELF {
            return Err((HvError::InvalidParameter, 0));
        }

        for (i, &HvRegisterAssoc { name, value, .. }) in registers.iter().enumerate() {
            match name.into() {
                HvArm64RegisterName::GuestOsId => self
                    .vp
                    .partition
                    .hv1
                    .guest_os_id
                    .store(value.as_u64(), Ordering::Relaxed),
                HvArm64RegisterName::Sipp => self
                    .vp
                    .hv1
                    .set_simp(&self.vp.partition.guest_memory, value.as_u64()),
                HvArm64RegisterName::Sifp => self
                    .vp
                    .hv1
                    .set_siefp(&self.vp.partition.guest_memory, value.as_u64()),
                HvArm64RegisterName::Scontrol => self.vp.hv1.set_scontrol(value.as_u64()),
                HvArm64RegisterName::Eom => {}
                r if (HvArm64RegisterName::Sint0..=HvArm64RegisterName::Sint15).contains(&r) => {
                    self.vp.hv1.set_sint(
                        (r.0 - HvArm64RegisterName::Sint0.0) as usize,
                        value.as_u64(),
                    )
                }
                register => {
                    tracing::warn!(?register, "unsupported register set");
                    return Err((HvError::InvalidParameter, i));
                }
            }
        }
        Ok(())
    }
}

impl<T: CpuIo> PostMessage for HvfHypercallHandler<'_, '_, T> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        self.cpu_io
            .post_synic_message(Vtl::Vtl0, connection_id, false, message)
    }
}

impl<T: CpuIo> SignalEvent for HvfHypercallHandler<'_, '_, T> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.cpu_io
            .signal_synic_event(Vtl::Vtl0, connection_id, flag)
    }
}
