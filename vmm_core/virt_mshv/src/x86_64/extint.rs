// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PIC ExtINT delivery through LINT0 for non-isolated partitions.
//!
//! A LINT0 pulse from the userspace PIC marks the VP and kicks its run loop,
//! which requests an interrupt deliverability notification. Once the VP can
//! accept an interrupt, the PIC vector is injected as a pending ExtINT event.

use crate::MshvPartitionInner;
use crate::MshvProcessor;
use crate::VcpuFdExt;
use hvdef::HvMessage;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::HvRegisterAssoc;
use virt::VpIndex;
use virt::io::CpuIo;

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
        vp.extint_pending
            .store(true, std::sync::atomic::Ordering::Release);
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
        use std::sync::atomic::Ordering;

        let vpinner = self.inner;
        if vpinner.extint_pending.load(Ordering::Acquire)
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

        self.inner
            .extint_pending
            .store(false, std::sync::atomic::Ordering::Release);
        let notifications = self
            .deliverability_notifications
            .with_interrupt_notification(false);
        self.partition
            .vmfd
            .register_deliverabilty_notifications(self.vpindex.index(), u64::from(notifications))
            .expect("requesting deliverability is not a fallible operation");
        self.deliverability_notifications = notifications;

        if let Some(vector) = dev.acknowledge_pic_interrupt() {
            let event = hvdef::HvX64PendingExtIntEvent::new()
                .with_event_pending(true)
                .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXT_INT)
                .with_vector(vector);
            self.runner
                .vcpufd
                .set_hvdef_regs(&[HvRegisterAssoc::from((
                    HvX64RegisterName::PendingEvent0,
                    u128::from(event),
                ))])
                .expect("setting a pending ExtINT event is not a fallible operation");
        }
    }
}
