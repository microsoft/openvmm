// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support routines for deferred actions.

use super::Hcl;
use crate::protocol;
use crate::protocol::hcl_run;
use cvm_tracing::CVM_ALLOWED;
use std::cell::Cell;
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use zerocopy::IntoBytes;

thread_local! {
    static DEFERRED_ACTIONS: DeferredActions = const { DeferredActions::new() };
}

struct DeferredActions {
    actions: [Cell<DeferredAction>; MAX_ACTIONS as usize],
    used: Cell<u8>,
}

const MAX_ACTIONS: u8 = 8;
const DISABLED: u8 = !0;

impl DeferredActions {
    const fn new() -> Self {
        Self {
            actions: [const { Cell::new(DeferredAction::Noop) }; MAX_ACTIONS as usize],
            used: Cell::new(DISABLED),
        }
    }

    fn drain(&self) -> &[Cell<DeferredAction>] {
        let used = self.used.replace(0);
        &self.actions[..used as usize]
    }
}

/// Pushes an action to the current thread's list of deferred actions. If the
/// list is full or there is no list for the current thread, the action will be
/// run immediately.
pub fn push_deferred_action(hcl: &Hcl, action: DeferredAction) {
    DEFERRED_ACTIONS.with(|deferred| {
        let used = deferred.used.get();
        if used < MAX_ACTIONS {
            deferred.actions[used as usize].set(action);
            deferred.used.set(used + 1);
        } else {
            // The action couldn't be deferred, so run it immediately.
            action.run(hcl)
        }
    });
}

/// A token representing that a deferred actions list has been registered for
/// the current thread.
///
/// When dropped, this will flush any deferred actions that were registered. The
/// owner can also call `flush` to run the actions immediately, if desired, and
/// `move_to_slots` to copy the actions to the HCL run structure's action slots
/// before running the VP.
//
// DEVNOTE: Use a PhantomData to ensure this isn't `Sync` or `Send`, so that it
// doesn't move to another thread.
pub struct RegisteredDeferredActions<'a>(&'a Hcl, PhantomData<*const ()>);

/// Registers a deferred actions list for the current thread.
pub fn register_deferred_actions(hcl: &Hcl) -> RegisteredDeferredActions<'_> {
    DEFERRED_ACTIONS.with(|deferred| {
        assert_eq!(deferred.used.replace(0), DISABLED);
    });
    RegisteredDeferredActions(hcl, PhantomData)
}

impl RegisteredDeferredActions<'_> {
    /// Moves the queued actions to the slots in the run page. Issues any
    /// immediately that won't fit in the run page.
    pub fn move_to_slots(&mut self, slots: &mut DeferredActionSlots<'_>) {
        self.with(|deferred, hcl| {
            for action in deferred.drain() {
                let action = action.get();
                if !action.post(slots) {
                    action.run(hcl);
                }
            }
        })
    }

    /// Runs actions immediately without deferring them to VTL return.
    pub fn flush(&mut self) {
        self.with(|deferred, hcl| {
            for action in deferred.drain() {
                action.get().run(hcl);
            }
        });
    }

    fn with(&mut self, f: impl FnOnce(&DeferredActions, &Hcl)) {
        DEFERRED_ACTIONS.with(|deferred| {
            debug_assert!(deferred.used.get() <= MAX_ACTIONS);
            f(deferred, self.0);
        });
    }
}

impl Drop for RegisteredDeferredActions<'_> {
    fn drop(&mut self) {
        self.flush();
        DEFERRED_ACTIONS.with(|deferred| {
            assert_eq!(deferred.used.replace(DISABLED), 0);
        })
    }
}

/// A deferred action that can be handled by the hypervisor as part of switching
/// VTLs.
#[derive(Debug, Copy, Clone)]
pub(crate) enum DeferredAction {
    Noop,
    SignalEvent { vp: u32, sint: u8, flag: u16 },
}

impl DeferredAction {
    /// Run the action via a hypercall.
    fn run(&self, hcl: &Hcl) {
        match *self {
            DeferredAction::Noop => {}
            DeferredAction::SignalEvent { vp, sint, flag } => {
                if let Err(err) = hcl.hvcall_signal_event_direct(vp, sint, flag) {
                    tracelimit::warn_ratelimited!(
                        CVM_ALLOWED,
                        error = &err as &dyn std::error::Error,
                        vp,
                        sint,
                        flag,
                        "failed to signal event"
                    );
                }
            }
        }
    }

    /// Post the action to the HCL.
    fn post(&self, slots: &mut DeferredActionSlots<'_>) -> bool {
        match *self {
            DeferredAction::Noop => true,
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
pub(crate) struct DeferredActionSlots<'a>(&'a UnsafeCell<hcl_run>);

impl<'a> DeferredActionSlots<'a> {
    /// # Safety
    /// The caller must ensure that the return action fields in `run` remain
    /// valid and unaliased for the lifetime of this object.
    pub unsafe fn new(run: &'a UnsafeCell<hcl_run>) -> Self {
        Self(run)
    }

    fn push(&mut self, action: &[u8]) -> bool {
        let (used, buffer);
        // SAFETY: this thread is the only one concurrently accessing the
        // action-related portions of the run structure.
        unsafe {
            used = &mut (*self.0.get()).vtl_ret_action_size;
            buffer = &mut (*self.0.get()).vtl_ret_actions;
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
