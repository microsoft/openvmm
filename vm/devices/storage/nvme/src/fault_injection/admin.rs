// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::FaultFn;
use crate::NvmeController;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::queue::SubmissionQueue;
use guestmem::GuestMemory;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTask;
use task_control::StopTask;
use vmcore::vm_task::VmTaskDriver;

/// An admin handler built for fault injection.
#[derive(Inspect)]
pub(crate) struct AdminHandlerFaultInjection {
    driver: VmTaskDriver,
    config: AdminConfigFaultInjection,
}

#[derive(Inspect)]
pub(crate) struct AdminStateFaultInjection {
    pub admin_sq: SubmissionQueue,
    pub admin_sq_gpa: u64,
}

#[derive(Inspect)]
pub(crate) struct AdminConfigFaultInjection {
    #[inspect(skip)]
    pub mem: GuestMemory,
    #[inspect(skip)]
    pub controller: Arc<Mutex<NvmeController>>,
    pub admin_sq_doorbell_addr: u16,
    #[inspect(skip)]
    pub sq_fault_injector: FaultFn,
}

impl AsyncRun<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut AdminStateFaultInjection,
    ) -> Result<(), Cancelled> {
        loop {
            if let Err(err) = stop.until_stopped(self.process_next_command(state)).await? {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "admin fault injection queue failure"
                );
                break;
            }
        }
        Ok(())
    }
}

impl AdminHandlerFaultInjection {
    async fn process_next_command(
        &mut self,
        state: &mut AdminStateFaultInjection,
    ) -> Result<(), QueueError> {
        let original_head = state.admin_sq.sqhd();
        let command = state.admin_sq.next(&self.config.mem).await?;

        let fault_command = (self.config.sq_fault_injector)(self.driver.clone(), command).await;

        // Fault inject a changed Command
        if let Some(fault_command) = fault_command {
            let gpa = state.admin_sq_gpa.wrapping_add(original_head as u64 * 64);

            self.config
                .mem
                .write_plain(gpa, &fault_command)
                .map_err(QueueError::Memory)?;
        }

        let data = state.admin_sq.sqhd() as u32;
        let mut inner_controller = self.config.controller.lock();
        let data = u32::to_ne_bytes(data);

        // Write to inner doorbell register to process only 1 command
        let _ = inner_controller.write_bar0(self.config.admin_sq_doorbell_addr, &data);

        Ok(())
    }
}

impl AdminHandlerFaultInjection {
    pub fn new(driver: VmTaskDriver, config: AdminConfigFaultInjection) -> Self {
        Self { driver, config }
    }
}

impl InspectTask<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&AdminStateFaultInjection>) {
        req.respond().merge(self).merge(state);
    }
}

impl AdminStateFaultInjection {
    pub fn new(asq: u64, asqs: u16, admin_sq_doorbell: Arc<DoorbellRegister>) -> Self {
        Self {
            admin_sq: SubmissionQueue::new(admin_sq_doorbell, asq, asqs, None),
            admin_sq_gpa: asq,
        }
    }
}
