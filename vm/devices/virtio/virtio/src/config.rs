// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use vmcore::slim_event::SlimEvent;

/// Config generation and pending notification shared by a device and its transport.
#[derive(Clone, Default, Inspect)]
#[inspect(transparent)]
pub struct ConfigGeneration(Arc<Mutex<State>>);

#[derive(Default, Inspect)]
struct State {
    generation: u8,
    #[inspect(skip)]
    changed: Arc<SlimEvent>,
}

impl ConfigGeneration {
    /// Publish an update and advance the generation if it changed.
    ///
    /// Return whether the config changed. The callback must not reenter
    /// this handle because generation reads wait for the update to finish.
    pub fn update(&self, update: impl FnOnce() -> bool) {
        let changed = {
            let mut state = self.0.lock();
            if !update() {
                return;
            }
            state.generation = state.generation.wrapping_add(1);
            state.changed.clone()
        };
        changed.signal();
    }

    pub(crate) fn bump_generation(&self) {
        let mut state = self.0.lock();
        state.generation = state.generation.wrapping_add(1);
    }

    pub(crate) fn generation(&self) -> u8 {
        self.0.lock().generation
    }

    pub(crate) fn set_status(&self, generation: u8, pending: bool) {
        let changed = Arc::new(SlimEvent::new());
        if pending {
            changed.signal();
        }
        let old = {
            let mut state = self.0.lock();
            state.generation = generation;
            std::mem::replace(&mut state.changed, changed)
        };
        drop(old);
    }

    pub(crate) fn poll_change(&self, cx: &mut Context<'_>) -> Poll<()> {
        let changed = self.0.lock().changed.clone();
        changed.poll_wait(cx)
    }

    pub(crate) fn clear_notification(&self) {
        let old = std::mem::take(&mut self.0.lock().changed);
        drop(old);
    }
}
