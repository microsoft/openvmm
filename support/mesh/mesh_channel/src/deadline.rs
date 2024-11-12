// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tools to manage waiting on deadlines.

use crate::cancel::Deadline;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::OnceLock;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

static GLOBAL: OnceLock<DeadlineSet> = OnceLock::new();

/// A set of deadlines that are being waited on. Backed by a thread.
#[derive(Debug)]
pub struct DeadlineSet {
    inner: Arc<Mutex<Inner>>,
    thread: std::thread::JoinHandle<()>,
}

#[derive(Debug)]
struct Inner {
    next: OptDeadline,
    entries: Vec<Entry>,
    terminate: bool,
}

#[derive(Debug)]
enum Entry {
    Vacant,
    Allocated,
    Active(OptDeadline, Waker),
}

impl Entry {
    fn is_vacant(&self) -> bool {
        matches!(self, Entry::Vacant)
    }
}

/// An optional deadline.
///
/// This is different from Option<Deadline> because Some(d) must compare less than None.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum OptDeadline {
    Some(Deadline),
    None,
}

#[derive(Debug, Default)]
pub struct DeadlineId(Option<usize>);

impl DeadlineSet {
    /// Returns the global deadline set.
    pub fn global() -> &'static Self {
        GLOBAL.get_or_init(Self::new)
    }

    /// Creates a new deadline set, starting a new thread to back it.
    //
    // TODO: Ideally instead of burning a separate thread for this, we would use the
    // executor's timeout infrastructure. But this would tie a mesh CancelContext to a
    // specific executor, which we want to avoid...
    pub fn new() -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            next: OptDeadline::None,
            entries: Vec::new(),
            terminate: false,
        }));
        let thread = std::thread::Builder::new()
            .name("deadline".to_string())
            .spawn({
                let inner = inner.clone();
                move || run(&inner)
            })
            .unwrap();
        Self { inner, thread }
    }

    /// Polls on a deadline.
    ///
    /// `id` should initially be created via `default`. Once this function has
    /// returned Poll::Pending at least once for a given id, the caller must
    /// eventually call `remove` to free the associated memory to avoid leaking
    /// the deadline entry.
    pub fn poll(&self, cx: &mut Context<'_>, id: &mut DeadlineId, deadline: Deadline) -> Poll<()> {
        let now = Deadline::now();
        if deadline <= now {
            return Poll::Ready(());
        }

        let mut inner = self.inner.lock();
        let i = *id.0.get_or_insert_with(|| {
            inner
                .entries
                .iter()
                .position(|p| p.is_vacant())
                .unwrap_or_else(|| {
                    inner.entries.push(Entry::Vacant);
                    inner.entries.len() - 1
                })
        });

        let deadline = OptDeadline::Some(deadline);
        let entry = &mut inner.entries[i];
        match entry {
            Entry::Vacant | Entry::Allocated => {
                *entry = Entry::Active(deadline, cx.waker().clone());
            }
            Entry::Active(old_deadline, old_waker) => {
                old_waker.clone_from(cx.waker());
                *old_deadline = deadline;
            }
        }

        if deadline < inner.next {
            inner.next = deadline;
            drop(inner);
            self.thread.thread().unpark();
        }

        Poll::Pending
    }

    /// Removes a deadline entry established via `poll`.
    pub fn remove(&self, id: &mut DeadlineId) {
        if let Some(i) = id.0.take() {
            let mut inner = self.inner.lock();
            assert!(!inner.entries[i].is_vacant());
            inner.entries[i] = Entry::Vacant;
        }
    }
}

/// Runs the deadline thread.
fn run(inner: &Mutex<Inner>) {
    loop {
        let next = {
            let inner = inner.lock();
            if inner.terminate {
                break;
            }
            inner.next
        };
        let now = Deadline::now();
        if next > OptDeadline::Some(now) {
            if let OptDeadline::Some(next) = next {
                std::thread::park_timeout(next - now);
            } else {
                std::thread::park();
            }
        } else {
            let mut next = OptDeadline::None;
            let mut inner = inner.lock();
            for entry in &mut inner.entries {
                if let Entry::Active(deadline, _) = entry {
                    if *deadline <= OptDeadline::Some(now) {
                        if let Entry::Active(_, waker) = std::mem::replace(entry, Entry::Allocated)
                        {
                            waker.wake();
                        } else {
                            unreachable!();
                        }
                    } else if *deadline < next {
                        next = *deadline;
                    }
                }
            }
            inner.next = next;
        }
    }
}

impl Drop for DeadlineSet {
    fn drop(&mut self) {
        self.inner.lock().terminate = true;
        self.thread.thread().unpark();
    }
}

#[cfg(test)]
mod tests {
    use super::DeadlineSet;
    use crate::cancel::Deadline;
    use pal_async::async_test;
    use std::future::poll_fn;
    use std::time::Duration;

    #[async_test]
    async fn deadline_set() {
        let set = DeadlineSet::new();
        let mut id = Default::default();
        let deadline = Deadline::now() + Duration::from_millis(10);
        poll_fn(|cx| set.poll(cx, &mut id, deadline)).await;
        assert!(Deadline::now() >= deadline);
        set.remove(&mut id);
    }
}
