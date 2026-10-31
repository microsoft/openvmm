// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure to support line interrupts.
//!
//! Devices own their output levels and must establish them during construction,
//! reset, and restore. Targets own interrupt recognition and its saved history.
//! A stopped [`LineSet`] caches publications without calling targets. Starting
//! it replays the final levels after targets have started and before producers
//! start. Pulses completed while the set is stopped are not replayed.

#![forbid(unsafe_code)]

use inspect::Inspect;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt::Debug;
use std::fmt::Display;
use std::ops::RangeInclusive;
use std::sync::Arc;
use thiserror::Error;

/// An error occurred while creating a new line interrupt.
#[derive(Debug, Error)]
pub enum NewLineError {
    /// The line interrupt has been shared too many times.
    #[error("irq {0} has been shared too many times")]
    TooMany(u32),
}

/// Unless you're implementing an interrupt controller (e.g: the PIC, IOAPIC),
/// you shouldn't be using this trait!
///
/// **NOTE: Individual devices should not use this trait directly!**
///
/// Devices are expected to use [`LineInterrupt`], which decouples the details
/// of IRQ numbers and assignment from concrete device implementations.
///
/// The alternative, where devices get handed an interface that allows them to
/// assert arbitrary IRQ lines, can lead to multiple devices inadvertently
/// trampling on one another's IRQ lines if you're not careful.
pub trait LineSetTarget: Send + Sync {
    /// Set an interrupt line state.
    ///
    /// Called with the source wire locked. A target must not synchronously
    /// publish back onto that same wire.
    fn set_irq(&self, vector: u32, high: bool);

    /// Initializes an input with no connected source when replaying line state.
    ///
    /// Pure line controllers should deassert the input. The default leaves it
    /// untouched because a target's namespace can also contain non-line
    /// interrupts whose pending state must not be cleared.
    fn set_unconnected_irq(&self, _vector: u32) {}
}

#[derive(Debug)]
struct Line {
    debug_label: Cow<'static, str>,
    is_high: Option<bool>,
}

impl Line {
    fn new(debug_label: Cow<'static, str>) -> Self {
        Self {
            debug_label,
            is_high: None,
        }
    }
}

struct Target {
    debug_label: Arc<str>,
    inner: Arc<dyn LineSetTarget>,
    vector: u32,
}

impl Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("debug_label", &self.debug_label)
            .field("vector", &self.vector)
            .finish()
    }
}

#[derive(Debug)]
struct LineInterruptInner {
    targets: Vec<Target>,
    lines: BTreeMap<u8, Line>,
    fresh_line_key: u8,
    vector: u32,
    running: bool,
}

impl Display for LineInterruptInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "targets[")?;
        for (i, target) in self.targets.iter().enumerate() {
            if i != 0 {
                write!(f, ",")?;
            }
            write!(f, "{}({})", target.debug_label, target.vector)?;
        }
        write!(f, "],lines[")?;
        for (i, (_, line)) in self.lines.iter().enumerate() {
            if i != 0 {
                write!(f, ",")?;
            }
            write!(f, "{}({:?})", line.debug_label, line.is_high)?;
        }
        write!(f, "]")?;
        Ok(())
    }
}

impl Inspect for LineInterruptInner {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .child("targets", |req| {
                let mut resp = req.respond();
                for target in self.targets.iter() {
                    resp.field(target.debug_label.as_ref(), target.vector);
                }
            })
            .child("lines", |req| {
                let mut resp = req.respond();
                for line in self.lines.values() {
                    resp.field(&line.debug_label, line.is_high);
                }
            });
    }
}

impl LineInterruptInner {
    fn new(
        debug_label: Cow<'static, str>,
        vector: u32,
        targets: Vec<Target>,
        running: bool,
    ) -> Self {
        Self {
            targets,
            lines: [(0, Line::new(debug_label))].into_iter().collect(),
            fresh_line_key: 1,
            vector,
            running,
        }
    }

    fn is_high(&self) -> bool {
        self.lines.values().any(|line| line.is_high == Some(true))
    }

    fn unpublished_sources(&self) -> impl Iterator<Item = &Line> {
        self.lines
            .values()
            .filter(|line| !self.targets.is_empty() && line.is_high.is_none())
    }

    fn publish(&self) {
        let is_high = self.is_high();
        if is_high && self.targets.is_empty() {
            tracelimit::warn_ratelimited!(%self, "LineInterrupt not hooked up to any targets!");
        }
        for target in &self.targets {
            target.inner.set_irq(target.vector, is_high);
        }
    }

    fn add_line(&mut self, debug_label: Cow<'static, str>) -> Option<u8> {
        const MAX_SHARED: usize = 16; // arbitrary choice
        if self.lines.len() >= MAX_SHARED {
            return None;
        }

        let line_key = self.fresh_line_key;
        // this loop would only go off in the unlikely case that a single
        // LineInterrupt is being constantly shared + dropped in a loop, and
        // is bounded to MAX_SHARED iterations.
        self.fresh_line_key = self.fresh_line_key.wrapping_add(1);
        while self.lines.contains_key(&self.fresh_line_key) {
            self.fresh_line_key = self.fresh_line_key.wrapping_add(1);
        }

        let existing = self.lines.insert(line_key, Line::new(debug_label));
        assert!(existing.is_none());

        Some(line_key)
    }
}

/// A line interrupt, representing a (virtually) physical wire between a device
/// and an interrupt controller.
//
// DEVNOTE: while it's tempting to provide an `impl Clone` for this type which
// returns a new LineInterrupt that gets OR'd with the existing LineInterrupt,
// doing so would violate the principle of least surprise, as the `Clone` trait
// in Rust isn't supposed to change the semantics of the underlying type. This
// could become a problem if, say, the LineInterrupt was stored in a `Vec` which
// was absentmindedly cloned.
pub struct LineInterrupt {
    inner: Arc<Mutex<LineInterruptInner>>,
    line_key: u8,
}

impl Debug for LineInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LineInterrupt")
            .field("line_key", &self.line_key)
            .field("inner", &*self.inner.lock())
            .finish()
    }
}

impl Display for LineInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.inner.lock(), f)
    }
}

impl Inspect for LineInterrupt {
    fn inspect(&self, req: inspect::Request<'_>) {
        let inner = self.inner.lock();
        let line = inner
            .lines
            .get(&self.line_key)
            .expect("line_key is always valid");

        req.respond()
            .field("debug_label", line.debug_label.as_ref())
            .field("is_high", line.is_high)
            .child("targets", |req| {
                let mut resp = req.respond();
                for target in inner.targets.iter() {
                    resp.field(target.debug_label.as_ref(), target.vector);
                }
            });
    }
}

impl LineInterrupt {
    /// Creates a line that is not attached to any line set or target.
    ///
    /// This is useful for testing purposes.
    pub fn detached() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LineInterruptInner {
                targets: Vec::new(),
                lines: [(0, Line::new("detached".into()))].into_iter().collect(),
                fresh_line_key: 1,
                vector: 0,
                running: true,
            })),
            line_key: 0,
        }
    }

    /// Creates a new line interrupt associated with provided target.
    ///
    /// This is a shorthand helper method for:
    ///
    /// ```rust
    /// # use vmcore::line_interrupt::*;
    /// # let f = || -> Result<LineInterrupt, NewLineError> {
    /// # let (label, target, vector) = ("", todo!(), 0);
    /// let set = LineSet::new();
    /// set.add_target(0..=0, vector, "target", target);
    /// set.start();
    /// set.new_line(0, label)
    /// # };
    /// ```
    pub fn new_with_target(
        debug_label: impl Into<Cow<'static, str>>,
        target: Arc<dyn LineSetTarget>,
        vector: u32,
    ) -> LineInterrupt {
        let set = LineSet::new();
        let debug_label = debug_label.into();
        set.add_target(0..=0, vector, debug_label.as_ref(), target);
        set.start();
        set.new_line(0, debug_label).unwrap()
    }

    /// Creates a new line interrupt sharing the same vector.
    pub fn new_shared(
        &self,
        debug_label: impl Into<Cow<'static, str>>,
    ) -> Result<Self, NewLineError> {
        let mut inner = self.inner.lock();
        let line_key = inner
            .add_line(debug_label.into())
            .ok_or(NewLineError::TooMany(inner.vector))?;

        Ok(Self {
            inner: self.inner.clone(),
            line_key,
        })
    }

    /// Sets the device's output high or low.
    ///
    /// This establishes the output even when the value is low. While the owning
    /// line set is stopped, the value is cached without notifying targets.
    pub fn set_level(&self, high: bool) {
        let mut inner = self.inner.lock();
        inner
            .lines
            .get_mut(&self.line_key)
            .expect("line_key is always valid")
            .is_high = Some(high);
        if inner.running {
            inner.publish();
        }
    }
}

impl Drop for LineInterrupt {
    fn drop(&mut self) {
        // make sure to deassert the line if this was the last shared line that
        // was being asserted.
        self.set_level(false);

        let mut inner = self.inner.lock();
        inner
            .lines
            .remove(&self.line_key)
            .expect("line_key is always valid");
    }
}

/// A set of line interrupts and their target mappings.
#[derive(Inspect)]
pub struct LineSet {
    #[inspect(flatten)]
    state: Mutex<LineSetState>,
}

#[derive(Inspect, Default)]
struct LineSetState {
    running: bool,
    #[inspect(with = "inspect_mappings")]
    targets: Vec<TargetMapping>,
    #[inspect(iter_by_key)]
    lines: BTreeMap<u32, Arc<Mutex<LineInterruptInner>>>,
}

#[derive(Clone)]
struct TargetMapping {
    source_range: RangeInclusive<u32>,
    target_start: u32,
    debug_label: Arc<str>,
    target: Arc<dyn LineSetTarget>,
}

fn inspect_mappings(mappings: &[TargetMapping]) -> impl '_ + Inspect {
    inspect::iter_by_key(mappings.iter().map(|mapping| {
        (
            format!(
                "{}:{}-{}",
                mapping.debug_label,
                mapping.target_start,
                mapping.target_start + (mapping.source_range.end() - mapping.source_range.start())
            ),
            format!(
                "{}-{}",
                mapping.source_range.start(),
                mapping.source_range.end(),
            ),
        )
    }))
}

impl LineSet {
    /// Creates a stopped line set with no published outputs.
    pub fn new() -> Self {
        Self {
            state: Default::default(),
        }
    }

    /// Replays current levels and enables synchronous forwarding.
    ///
    /// Targets must already be started, and producers must not start until this
    /// returns. Unpublished attached sources are warned about and treated as
    /// low. Replay is serialized with publications and target attachment.
    pub fn start(&self) {
        let mut state = self.state.lock();
        assert!(!state.running);
        state.running = true;
        for line in state.lines.values() {
            let mut line = line.lock();
            for source in line.unpublished_sources() {
                tracelimit::warn_ratelimited!(
                    vector = line.vector,
                    source = %source.debug_label,
                    "interrupt source did not publish a level before start"
                );
            }
            line.running = true;
            line.publish();
        }
        // Unwired inputs are inactive, including inputs whose source was removed
        // when restoring into a different device topology.
        for mapping in &state.targets {
            for vector in mapping.source_range.clone() {
                if !state.lines.contains_key(&vector) {
                    mapping.target.set_unconnected_irq(
                        vector - mapping.source_range.start() + mapping.target_start,
                    );
                }
            }
        }
    }

    /// Disables forwarding without changing outputs or publication validity.
    ///
    /// Producers must stop before this is called, and targets stop afterward.
    pub fn stop(&self) {
        let mut state = self.state.lock();
        assert!(state.running);
        state.running = false;
        for line in state.lines.values() {
            line.lock().running = false;
        }
    }

    /// Marks outputs unpublished for a new reset epoch.
    ///
    /// Must be called while stopped, before producers reset and publish their
    /// outputs. Restoring must follow construction or reset, not old VM state.
    pub fn reset(&self) {
        let state = self.state.lock();
        assert!(!state.running);
        for line in state.lines.values() {
            for source in line.lock().lines.values_mut() {
                source.is_high = None;
            }
        }
    }

    /// Adds a target mapping to the set.
    ///
    /// The mapping is over a portion of the line set as specified by
    /// `source_range`, and it is mapped into the target's vector space starting
    /// at `target_start`.
    pub fn add_target(
        &self,
        source_range: RangeInclusive<u32>,
        target_start: u32,
        debug_label: impl Into<Arc<str>>,
        target: Arc<dyn LineSetTarget>,
    ) {
        let debug_label = debug_label.into();
        let mut state = self.state.lock();
        // Add this target to any existing lines that overlap with
        // `source_range`.
        for (&vector, line) in &mut state.lines {
            if source_range.contains(&vector) {
                let target_vector = vector - source_range.start() + target_start;
                let mut line = line.lock();
                line.targets.push(Target {
                    debug_label: debug_label.clone(),
                    inner: target.clone(),
                    vector: target_vector,
                });
                if line.running {
                    target.set_irq(target_vector, line.is_high());
                }
            }
        }
        if state.running {
            for vector in source_range.clone() {
                if !state.lines.contains_key(&vector) {
                    target.set_unconnected_irq(vector - source_range.start() + target_start);
                }
            }
        }
        state.targets.push(TargetMapping {
            source_range,
            target_start,
            target,
            debug_label,
        });
    }

    /// Adds a new line interrupt to the set.
    pub fn new_line(
        &self,
        vector: u32,
        debug_label: impl Into<Cow<'static, str>>,
    ) -> Result<LineInterrupt, NewLineError> {
        self.new_line_(vector, debug_label.into())
    }

    fn new_line_(
        &self,
        vector: u32,
        debug_label: Cow<'static, str>,
    ) -> Result<LineInterrupt, NewLineError> {
        let mut state = self.state.lock();
        let state = &mut *state;
        let line = match state.lines.entry(vector) {
            Entry::Occupied(entry) => {
                let inner = entry.get();
                let line_key = inner
                    .lock()
                    .add_line(debug_label)
                    .ok_or(NewLineError::TooMany(vector))?;

                LineInterrupt {
                    inner: inner.clone(),
                    line_key,
                }
            }
            Entry::Vacant(entry) => {
                let inner = Arc::new(Mutex::new(LineInterruptInner::new(
                    debug_label,
                    vector,
                    state
                        .targets
                        .iter()
                        .filter(|&mapping| mapping.source_range.contains(&vector))
                        .map(|mapping| Target {
                            debug_label: mapping.debug_label.clone(),
                            inner: mapping.target.clone(),
                            vector: vector - mapping.source_range.start() + mapping.target_start,
                        })
                        .collect(),
                    state.running,
                )));
                entry.insert(inner.clone());
                LineInterrupt { inner, line_key: 0 }
            }
        };
        Ok(line)
    }
}

#[expect(missing_docs)] // self explanatory struct/functions
pub mod test_helpers {
    use crate::line_interrupt::LineSetTarget;
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    pub struct TestLineInterruptTarget {
        state: Mutex<BTreeMap<u32, LineState>>,
    }

    #[derive(Default)]
    struct LineState {
        is_high: bool,
        waker: Option<Waker>,
    }

    impl TestLineInterruptTarget {
        pub fn new_arc() -> Arc<TestLineInterruptTarget> {
            Arc::new(TestLineInterruptTarget {
                state: Default::default(),
            })
        }

        pub fn is_high(&self, vector: u32) -> bool {
            self.state.lock().get(&vector).is_some_and(|s| s.is_high)
        }

        pub fn poll_high(&self, cx: &mut Context<'_>, vector: u32) -> Poll<()> {
            let mut state = self.state.lock();
            let state = state.entry(vector).or_default();
            if state.is_high {
                Poll::Ready(())
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    impl LineSetTarget for TestLineInterruptTarget {
        fn set_irq(&self, vector: u32, high: bool) {
            let mut state = self.state.lock();
            let state = &mut state.entry(vector).or_default();
            state.is_high = high;
            if high {
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::line_interrupt::test_helpers::TestLineInterruptTarget;
    use test_with_tracing::test;

    #[derive(Default)]
    struct RecordingTarget(Mutex<Vec<bool>>);

    impl LineSetTarget for RecordingTarget {
        fn set_irq(&self, _: u32, high: bool) {
            self.0.lock().push(high);
        }

        fn set_unconnected_irq(&self, vector: u32) {
            self.set_irq(vector, false);
        }
    }

    #[test]
    fn stopped_publications_replay_only_the_final_level() {
        let target = Arc::new(RecordingTarget::default());
        let set = LineSet::new();
        let line = set.new_line(0, "device").unwrap();
        line.set_level(true);
        line.set_level(false);
        set.add_target(0..=0, 0, "target", target.clone());
        assert!(target.0.lock().is_empty());

        set.start();
        assert_eq!(*target.0.lock(), [false]);
        line.set_level(true);
        assert_eq!(*target.0.lock(), [false, true]);

        set.stop();
        line.set_level(false);
        line.set_level(true);
        assert_eq!(*target.0.lock(), [false, true]);
        set.start();
        assert_eq!(*target.0.lock(), [false, true, true]);
    }

    #[test]
    fn stopped_drop_and_unwired_inputs_replay_low() {
        let target = Arc::new(RecordingTarget::default());
        let set = LineSet::new();
        set.add_target(0..=1, 0, "target", target.clone());
        let line = set.new_line(0, "device").unwrap();
        line.set_level(true);
        set.start();
        assert_eq!(*target.0.lock(), [true, false]);

        set.stop();
        drop(line);
        assert_eq!(*target.0.lock(), [true, false]);
        set.start();
        assert_eq!(*target.0.lock(), [true, false, false, false]);
    }

    #[test]
    fn replay_does_not_clear_non_line_target_state() {
        let target = TestLineInterruptTarget::new_arc();
        target.set_irq(0, true);
        let set = LineSet::new();
        set.add_target(0..=1, 0, "shared namespace", target.clone());
        let line = set.new_line(1, "device").unwrap();
        line.set_level(false);
        set.start();
        assert!(target.is_high(0));
        assert!(!target.is_high(1));
    }

    #[test]
    fn publication_validity_is_per_source_and_survives_pause() {
        let set = LineSet::new();
        set.add_target(0..=0, 0, "target", Arc::new(RecordingTarget::default()));
        let a = set.new_line(0, "a").unwrap();
        let b = set.new_line(0, "b").unwrap();
        a.set_level(true);
        {
            let inner = a.inner.lock();
            assert!(inner.is_high());
            assert_eq!(
                inner
                    .unpublished_sources()
                    .map(|line| line.debug_label.as_ref())
                    .collect::<Vec<_>>(),
                ["b"]
            );
        }
        b.set_level(false);
        set.start();
        set.stop();
        assert!(a.inner.lock().unpublished_sources().next().is_none());

        set.reset();
        assert_eq!(a.inner.lock().unpublished_sources().count(), 2);
        a.set_level(false);
        b.set_level(false);
        assert!(a.inner.lock().unpublished_sources().next().is_none());
    }

    #[test]
    fn attachment_and_start_replay_hold_the_wire_lock() {
        use std::sync::mpsc;

        struct BlockingTarget {
            entered: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }

        impl LineSetTarget for BlockingTarget {
            fn set_irq(&self, _: u32, _: bool) {
                self.entered.send(()).unwrap();
                self.release.lock().recv().unwrap();
            }
        }

        for running in [false, true] {
            let set = Arc::new(LineSet::new());
            let line = set.new_line(0, "device").unwrap();
            line.set_level(true);
            if running {
                set.start();
            }
            let (entered_send, entered_recv) = mpsc::sync_channel(0);
            let (release_send, release_recv) = mpsc::sync_channel(0);
            let target = Arc::new(BlockingTarget {
                entered: entered_send,
                release: Mutex::new(release_recv),
            });
            let task = std::thread::spawn({
                let set = set.clone();
                move || {
                    set.add_target(0..=0, 0, "target", target);
                    if !running {
                        set.start();
                    }
                }
            });
            entered_recv.recv().unwrap();
            let wire_locked = line.inner.try_lock().is_none();
            release_send.send(()).unwrap();
            task.join().unwrap();
            assert!(wire_locked);
            // Do not notify the blocking target again during cleanup.
            set.stop();
        }
    }

    #[test]
    fn basic() {
        let intcon = TestLineInterruptTarget::new_arc();

        let line0 = LineInterrupt::new_with_target("line0", intcon.clone(), 0);
        let line1 = LineInterrupt::new_with_target("line1", intcon.clone(), 1);

        line0.set_level(true);
        assert!(intcon.is_high(0));
        line0.set_level(false);
        assert!(!intcon.is_high(0));

        line1.set_level(true);
        assert!(intcon.is_high(1));
        line1.set_level(false);
        assert!(!intcon.is_high(1));
    }

    #[test]
    fn multi_target() {
        let intcon1 = TestLineInterruptTarget::new_arc();
        let intcon2 = TestLineInterruptTarget::new_arc();

        let line_set = LineSet::new();

        let line = line_set.new_line(2, "line").unwrap();

        line_set.add_target(1..=5, 7, "intcon1", intcon1.clone());
        line.set_level(false);
        line_set.start();
        line.set_level(true);
        line_set.add_target(2..=2, 3, "intcon2", intcon2.clone());

        assert!(intcon1.is_high(8));
        assert!(intcon2.is_high(3));
    }

    #[test]
    fn shared_line() {
        let intcon = TestLineInterruptTarget::new_arc();

        let line_set = LineSet::new();
        line_set.add_target(0..=0, 0, "intcon", intcon.clone());

        let line00 = line_set.new_line(0, "line00").unwrap();
        let line01 = line_set.new_line(0, "line01").unwrap();

        line00.set_level(false);
        line01.set_level(false);
        line_set.start();
        line00.set_level(true);
        assert!(intcon.is_high(0));
        line01.set_level(true);
        assert!(intcon.is_high(0));
        line00.set_level(false);
        assert!(intcon.is_high(0)); // still high, since line01 is still asserting
        line01.set_level(false);
        assert!(!intcon.is_high(0)); // finally low
    }

    #[test]
    fn drop_impl() {
        let intcon = TestLineInterruptTarget::new_arc();

        let line_set = LineSet::new();
        line_set.add_target(0..=0, 0, "intcon", intcon.clone());

        let line00 = line_set.new_line(0, "line00").unwrap();
        let line01 = line_set.new_line(0, "line01").unwrap();

        line00.set_level(false);
        line01.set_level(false);
        line_set.start();
        line00.set_level(true);
        assert!(intcon.is_high(0));
        line01.set_level(true);
        assert!(intcon.is_high(0));
        line00.set_level(false);
        assert!(intcon.is_high(0)); // still high, since line01 is still asserting
        drop(line01);
        assert!(!intcon.is_high(0)); // low, because line01 disappeared
    }

    #[test]
    fn share_drop_loop() {
        let intcon = TestLineInterruptTarget::new_arc();

        let line_set = LineSet::new();
        line_set.add_target(0..=0, 0, "intcon", intcon.clone());

        let _line00 = line_set.new_line(0, "line00").unwrap();
        _line00.set_level(false);
        line_set.start();

        for _ in 0..1000 {
            let line01 = line_set.new_line(0, "line01").unwrap();
            line01.set_level(true);
            assert!(intcon.is_high(0));
            drop(line01);
            assert!(!intcon.is_high(0));
        }
    }
}
