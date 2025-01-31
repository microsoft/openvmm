// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support routines for deferred actions.

use super::Hcl;
use crate::protocol;
use crate::protocol::hcl_run;
use std::ptr::addr_of_mut;
use std::ptr::NonNull;
use zerocopy::IntoBytes;

#[derive(Debug, Default)]
pub struct DeferredActions {
    actions: Vec<DeferredAction>,
}

const MAX_ACTIONS: usize = 8;

impl DeferredActions {
    /// Pushes the actions.
    pub fn push(&mut self, hcl: &Hcl, action: DeferredAction) {
        if self.actions.len() < MAX_ACTIONS {
            self.actions.push(action);
        } else {
            action.run(hcl);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Copies the queued actions to the slots in the run page. Issues any
    /// immediately that won't fit in the run page.
    pub fn copy_to_slots(&mut self, slots: &mut DeferredActionSlots, hcl: &Hcl) {
        for action in self.actions.drain(..) {
            if !action.post(slots) {
                action.run(hcl);
            }
        }
    }

    /// Runs actions immediately without deferring them to VTL return.
    pub fn run_actions(&mut self, hcl: &Hcl) {
        for action in self.actions.drain(..) {
            action.run(hcl);
        }
    }
}

/// A deferred action that can be handled by the hypervisor as part of switching
/// VTLs.
#[derive(Debug, Copy, Clone)]
pub enum DeferredAction {
    SignalEvent { vp: u32, sint: u8, flag: u16 },
}

impl DeferredAction {
    /// Run the action via a hypercall.
    fn run(&self, hcl: &Hcl) {
        match *self {
            DeferredAction::SignalEvent { vp, sint, flag } => {
                let _ = hcl.hvcall_signal_event_direct(vp, sint, flag);
            }
        }
    }

    /// Post the action to the HCL.
    fn post(&self, slots: &mut DeferredActionSlots) -> bool {
        match *self {
            DeferredAction::SignalEvent { vp, sint, flag } => slots.push(
                protocol::hv_vp_assist_page_signal_event {
                    action_type: protocol::HV_VP_ASSIST_PAGE_ACTION_TYPE_SIGNAL_EVENT,
                    vp,
                    vtl: 0,
                    sint,
                    flag,
                }
                .as_bytes(),
            ),
        }
    }
}

/// A reference to the HCL run data structure's deferred action slots.
pub struct DeferredActionSlots(NonNull<hcl_run>);

impl DeferredActionSlots {
    /// # Safety
    /// The caller must ensure that the return action fields in `run` remain
    /// valid and unaliased for the lifetime of this object.
    pub unsafe fn new(run: NonNull<hcl_run>) -> Self {
        Self(run)
    }

    fn push(&mut self, action: &[u8]) -> bool {
        let (used, buffer);
        // SAFETY: this thread is the only one concurrently accessing the
        // action-related portions of the run structure.
        unsafe {
            used = &mut *addr_of_mut!((*self.0.as_ptr()).vtl_ret_action_size);
            buffer = &mut *addr_of_mut!((*self.0.as_ptr()).vtl_ret_actions);
        }
        let offset = *used as usize;
        if let Some(buffer) = buffer.get_mut(offset..offset + action.len()) {
            buffer.copy_from_slice(action);
            *used += action.len() as u32;
            true
        } else {
            // The action buffer is full.
            false
        }
    }
}
