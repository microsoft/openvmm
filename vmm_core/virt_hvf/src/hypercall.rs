// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall exit handling.

use crate::HvfProcessor;
use hv1_hypercall::Arm64RegisterState;
use hv1_hypercall::GetVpRegisters;
use hv1_hypercall::HvRepResult;
use hv1_hypercall::PostMessage;
use hv1_hypercall::SetVpRegisters;
use hv1_hypercall::SignalEvent;
use hvdef::HvArm64RegisterName;
use hvdef::HvError;
use hvdef::Vtl;
use hvdef::hypercall::HvRegisterAssoc;
use std::sync::atomic::Ordering;

pub(crate) struct HvfHypercallHandler<'a, 'b> {
    vp: &'a mut HvfProcessor<'b>,
}

impl<'a, 'b> HvfHypercallHandler<'a, 'b> {
    pub const DISPATCHER: hv1_hypercall::Dispatcher<Self> = hv1_hypercall::dispatcher!(
        Self,
        [
            hv1_hypercall::HvGetVpRegisters,
            hv1_hypercall::HvSetVpRegisters,
            hv1_hypercall::HvPostMessage,
            hv1_hypercall::HvSignalEvent,
        ]
    );

    pub fn new(vp: &'a mut HvfProcessor<'b>) -> Self {
        Self { vp }
    }
}

impl Arm64RegisterState for HvfHypercallHandler<'_, '_> {
    fn pc(&mut self) -> u64 {
        self.vp.vcpu.pc()
    }

    fn set_pc(&mut self, pc: u64) {
        tracing::trace!(pc, "set pc");
        self.vp.vcpu.set_pc(pc)
    }

    fn x(&mut self, n: u8) -> u64 {
        self.vp.vcpu.gp(n)
    }

    fn set_x(&mut self, n: u8, v: u64) {
        tracing::trace!(n, v, "set x");
        self.vp.vcpu.set_gp(n, v)
    }
}

impl GetVpRegisters for HvfHypercallHandler<'_, '_> {
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

impl SetVpRegisters for HvfHypercallHandler<'_, '_> {
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
                    .set_simp(
                        value.as_u64(),
                        &mut HvfNoVtlProtections(&self.vp.partition.guest_memory),
                    )
                    .map_err(|_| (HvError::InvalidParameter, 1))?,
                HvArm64RegisterName::Sifp => self
                    .vp
                    .hv1
                    .set_siefp(
                        value.as_u64(),
                        &mut HvfNoVtlProtections(&self.vp.partition.guest_memory),
                    )
                    .map_err(|_| (HvError::InvalidParameter, 1))?,
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

struct HvfNoVtlProtections<'a>(&'a guestmem::GuestMemory);
impl<'a> hv1_emulator::VtlProtectAccess for HvfNoVtlProtections<'a> {
    fn check_modify_and_lock_overlay_page(
        &mut self,
        gpn: u64,
        _check_perms: hvdef::HvMapGpaFlags,
        _new_perms: Option<hvdef::HvMapGpaFlags>,
    ) -> Result<guestmem::UserLockedPages, HvError> {
        // Overlay pages are written through the returned locked pages, so lock
        // them for write.
        self.0
            .lock_user_gpns(guestmem::AccessType::Write, &[gpn])
            .map_err(|_| HvError::OperationDenied)
    }

    fn unlock_overlay_page(&mut self, _gpn: u64) -> Result<(), HvError> {
        Ok(())
    }
}

impl PostMessage for HvfHypercallHandler<'_, '_> {
    fn post_message(&mut self, connection_id: u32, message: &[u8]) -> hvdef::HvResult<()> {
        self.vp
            .partition
            .synic_ports
            .handle_post_message(Vtl::Vtl0, connection_id, false, message)
    }
}

impl SignalEvent for HvfHypercallHandler<'_, '_> {
    fn signal_event(&mut self, connection_id: u32, flag: u16) -> hvdef::HvResult<()> {
        self.vp
            .partition
            .synic_ports
            .handle_signal_event(Vtl::Vtl0, connection_id, flag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hv1_emulator::VtlProtectAccess;
    use std::ptr::NonNull;
    use test_with_tracing::test;

    struct NoUserLockMemory;

    // SAFETY: no raw mapping is exposed, and the default fallbacks return errors.
    unsafe impl guestmem::GuestMemoryAccess for NoUserLockMemory {
        fn mapping(&self) -> Option<NonNull<u8>> {
            None
        }

        fn max_address(&self) -> u64 {
            0x1000
        }
    }

    #[test]
    fn overlay_lock_preserves_valid_ram_and_rejects_invalid_gpn() {
        let memory = guestmem::GuestMemory::allocate(0x1000);
        let mut access = HvfNoVtlProtections(&memory);
        let permissions = hvdef::HvMapGpaFlags::new()
            .with_readable(true)
            .with_writable(true);
        assert!(
            access
                .check_modify_and_lock_overlay_page(0, permissions, None)
                .is_ok()
        );
        assert!(matches!(
            access.check_modify_and_lock_overlay_page(1, permissions, None),
            Err(HvError::OperationDenied)
        ));
    }

    #[test]
    fn overlay_lock_returns_error_when_user_locks_are_denied() {
        let memory = guestmem::GuestMemory::new("no-user-locks", NoUserLockMemory);
        assert!(!memory.supports_user_lock());
        assert!(matches!(
            HvfNoVtlProtections(&memory).check_modify_and_lock_overlay_page(
                0,
                hvdef::HvMapGpaFlags::new()
                    .with_readable(true)
                    .with_writable(true),
                None,
            ),
            Err(HvError::OperationDenied)
        ));
    }
}
