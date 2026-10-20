// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PIC ExtINT delivery through LINT0 for non-isolated partitions.
//!
//! A LINT0 pulse from the userspace PIC marks the VP and kicks its run loop,
//! which requests an interrupt deliverability notification. Once the VP can
//! accept an interrupt, the PIC vector is injected as a pending ExtINT event.

use crate::KernelError;
use crate::MshvPartitionInner;
use crate::MshvProcessor;
use crate::VcpuFdExt;
use hvdef::HvMessage;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::HvRegisterAssoc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use virt::VpIndex;
use virt::io::CpuIo;

#[derive(Default)]
pub(crate) struct ExtintState {
    pending: AtomicBool,
}

impl ExtintState {
    fn pulse(&self) {
        self.pending.store(true, Ordering::Release);
    }

    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    fn acknowledge(&self, acknowledge: impl FnOnce() -> Option<u8>) -> Option<u8> {
        self.pending.store(false, Ordering::Release);
        acknowledge()
    }
}

fn inject(vcpufd: &impl VcpuFdExt, vector: u8) -> Result<(), KernelError> {
    let event = hvdef::HvX64PendingExtIntEvent::new()
        .with_event_pending(true)
        .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXT_INT)
        .with_vector(vector);
    vcpufd.set_hvdef_regs(&[HvRegisterAssoc::from((
        HvX64RegisterName::PendingEvent0,
        u128::from(event),
    ))])
}

impl MshvPartitionInner {
    pub(super) fn pulse_lint(&self, vp_index: VpIndex, vtl: Vtl, lint: u8) {
        if self.isolation.snp().is_some() {
            // MSHV isolated VPs cannot receive PIC ExtINT through LINT0.
            tracelimit::warn_ratelimited!(?vp_index, ?vtl, lint, "ignored isolated lint pulse");
            return;
        }
        if vtl != Vtl::Vtl0 || lint != 0 {
            tracelimit::warn_ratelimited!(?vp_index, ?vtl, lint, "unsupported lint pulse");
            return;
        }
        let Some(vp) = self.vps.get(vp_index.index() as usize) else {
            tracelimit::warn_ratelimited!(?vp_index, "lint pulse for invalid VP");
            return;
        };
        vp.extint.pulse();
        if let Some(waker) = &*vp.waker.read() {
            waker.wake_by_ref();
        }
        if let Some(thread) = *vp.thread.read() {
            crate::run_vp::cancel(thread).expect("thread interrupt signal failed");
        }
    }
}

impl MshvProcessor<'_> {
    /// Requests an interrupt deliverability notification while a LINT0
    /// ExtINT is pending.
    pub(crate) fn request_extint_notification(&mut self) {
        let vpinner = self.inner;
        if vpinner.extint.is_pending()
            && !self.deliverability_notifications.interrupt_notification()
        {
            let notifications = self
                .deliverability_notifications
                .with_interrupt_notification(true);
            self.partition
                .vmfd
                .register_deliverabilty_notifications(
                    self.vpindex.index(),
                    u64::from(notifications),
                )
                .expect("requesting deliverability is not a fallible operation");
            self.deliverability_notifications = notifications;
        }
    }

    pub(super) fn handle_interrupt_deliverable(&mut self, message: &HvMessage, dev: &impl CpuIo) {
        let message = message.as_message::<hvdef::HvX64InterruptionDeliverableMessage>();
        if message.deliverable_type != hvdef::HvX64PendingInterruptionType::HV_X64_PENDING_INTERRUPT
        {
            tracelimit::warn_ratelimited!(
                deliverable_type = ?message.deliverable_type,
                "unsupported interruption deliverability notification"
            );
            return;
        }

        let extint = &self.inner.extint;
        let notifications = &mut self.deliverability_notifications;
        let vmfd = &self.partition.vmfd;
        let vp_index = self.vpindex.index();
        let vector = extint.acknowledge(|| {
            let next = notifications.with_interrupt_notification(false);
            vmfd.register_deliverabilty_notifications(vp_index, u64::from(next))
                .expect("requesting deliverability is not a fallible operation");
            *notifications = next;
            dev.acknowledge_pic_interrupt()
        });

        if let Some(vector) = vector {
            inject(self.runner.vcpufd, vector)
                .expect("setting a pending ExtINT event is not a fallible operation");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ExtintState;
    use super::inject;
    use crate::KernelError;
    use crate::VcpuFdExt;
    use crate::x86_64::vp_state::get_register_state;
    use crate::x86_64::vp_state::set_register_state;
    use hvdef::HvRegisterValue;
    use hvdef::hypercall::HvRegisterAssoc;
    use parking_lot::Mutex;
    use test_with_tracing::test;
    use virt::x86::vp::Activity;
    use virt::x86::vp::PendingEvent;

    const VECTOR: u8 = 0x20;

    #[derive(Default)]
    struct TestVpRegisters {
        regs: Mutex<Vec<HvRegisterAssoc>>,
    }

    impl VcpuFdExt for TestVpRegisters {
        fn get_hvdef_regs(&self, regs: &mut [HvRegisterAssoc]) -> Result<(), KernelError> {
            let state = self.regs.lock();
            for reg in regs {
                reg.value = state
                    .iter()
                    .find(|saved| saved.name == reg.name)
                    .map_or_else(|| HvRegisterValue::from(0_u64), |saved| saved.value);
            }
            Ok(())
        }

        fn set_hvdef_regs(&self, regs: &[HvRegisterAssoc]) -> Result<(), KernelError> {
            let mut state = self.regs.lock();
            for reg in regs {
                if let Some(saved) = state.iter_mut().find(|saved| saved.name == reg.name) {
                    saved.value = reg.value;
                } else {
                    state.push(*reg);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn lint0_reasserted_during_acknowledgement_remains_pending() {
        let extint = ExtintState::default();
        extint.pulse();

        assert_eq!(
            extint.acknowledge(|| {
                extint.pulse();
                Some(VECTOR)
            }),
            Some(VECTOR)
        );
        assert!(extint.is_pending());

        assert_eq!(extint.acknowledge(|| Some(VECTOR + 1)), Some(VECTOR + 1));
        assert!(!extint.is_pending());
    }

    #[test]
    fn injected_extint_survives_state_capture_and_restore() {
        let source = TestVpRegisters::default();
        inject(&source, VECTOR).unwrap();

        let saved: Activity = get_register_state(&source).unwrap();
        assert_eq!(
            saved.pending_event,
            Some(PendingEvent::ExtInt { vector: VECTOR })
        );

        let restored = TestVpRegisters::default();
        set_register_state(&restored, &saved).unwrap();
        let restored: Activity = get_register_state(&restored).unwrap();
        assert_eq!(restored.pending_event, saved.pending_event);
    }
}
