// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure to support line interrupts.

#![warn(missing_docs)]

use inspect::Inspect;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
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
    fn set_irq(&self, vector: u32, high: bool);
}

#[derive(Debug)]
struct Line {
    debug_label: Cow<'static, str>,
    is_high: bool,
}

impl Line {
    fn new(debug_label: Cow<'static, str>) -> Self {
        Self {
            debug_label,
            is_high: false,
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
            write!(f, "{}({})", line.debug_label, line.is_high)?;
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
                for (_, line) in self.lines.iter() {
                    resp.field(&line.debug_label, line.is_high);
                }
            });
    }
}

impl LineInterruptInner {
    fn new(debug_label: Cow<'static, str>, vector: u32, targets: Vec<Target>) -> Self {
        Self {
            targets,
            lines: [(0, Line::new(debug_label))].into_iter().collect(),
            fresh_line_key: 1,
            vector,
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

    /// Sets the line level high or low.
    pub fn set_level(&self, high: bool) {
        let mut inner = self.inner.lock();
        inner
            .lines
            .get_mut(&self.line_key)
            .expect("line_key is always valid")
            .is_high = high;

        let is_high = inner.lines.iter().any(|(_, line)| line.is_high);

        if is_high && inner.targets.is_empty() {
            tracelimit::warn_ratelimited!(%inner, "LineInterrupt not hooked up to any targets!");
        }

        for target in inner.targets.iter() {
            target.inner.set_irq(target.vector, is_high);
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
    /// Creates a new line set.
    pub fn new() -> Self {
        Self {
            state: Default::default(),
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
                let is_high = {
                    let mut line = line.lock();
                    line.targets.push(Target {
                        debug_label: debug_label.clone(),
                        inner: target.clone(),
                        vector: target_vector,
                    });
                    line.lines.iter().any(|(_, line)| line.is_high)
                };
                if is_high {
                    target.set_irq(target_vector, true);
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
                )));
                entry.insert(inner.clone());
                LineInterrupt { inner, line_key: 0 }
            }
        };
        Ok(line)
    }
}

#[allow(missing_docs)] // self explanatory struct/functions
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
            self.state.lock().get(&vector).map_or(false, |s| s.is_high)
        }

        pub fn poll_high(&self, cx: &mut Context<'_>, vector: u32) -> Poll<()> {
            let mut state = self.state.lock();
            let state = state.get_mut(&vector).unwrap();
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

        for _ in 0..1000 {
            let line01 = line_set.new_line(0, "line01").unwrap();
            line01.set_level(true);
            assert!(intcon.is_high(0));
            drop(line01);
            assert!(!intcon.is_high(0));
        }
    }
}
