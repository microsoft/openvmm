// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A multi-waker that multiplexes multiple wakers onto a single waker.

// UNSAFETY: Implementing a `RawWakerVTable`.
#![allow(unsafe_code)]

use parking_lot::Mutex;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::RawWaker;
use std::task::RawWakerVTable;
use std::task::Waker;

/// Object to multiplex multiple wakers onto a single waker.
#[derive(Debug)]
pub struct MultiWaker<const N: usize> {
    inner: Arc<Inner<N>>,
}

#[derive(Debug)]
struct Inner<const N: usize> {
    wakers: Mutex<[Option<Waker>; N]>,
}

impl<const N: usize> Inner<N> {
    /// Sets the waker for index `i`.
    fn set(&self, i: usize, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        if !wakers[i].as_ref().is_some_and(|old| old.will_wake(waker)) {
            let _old = wakers[i].replace(waker.clone());
            drop(wakers);
        }
    }

    /// Wakes any wakers that have been set.
    fn wake(&self) {
        let wakers = std::mem::replace(&mut *self.wakers.lock(), [(); N].map(|_| None));
        for waker in wakers.into_iter().flatten() {
            waker.wake();
        }
    }
}

struct Ref<'a, 'b, const N: usize> {
    inner: &'a Arc<Inner<N>>,
    cx_waker: &'b Waker,
    index: usize,
}

impl<const N: usize> MultiWaker<N> {
    /// Creates a new instance.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                wakers: Mutex::new([(); N].map(|_| None)),
            }),
        }
    }

    /// Calls a poll function on behalf of entry `index`, passing a `Context` that
    /// ensures that each index's waker is called on wake.
    pub fn poll_wrapped<R>(
        &self,
        cx: &mut Context<'_>,
        index: usize,
        f: impl FnOnce(&mut Context<'_>) -> Poll<R>,
    ) -> Poll<R> {
        let waker_ref = Ref {
            inner: &self.inner,
            index,
            cx_waker: cx.waker(),
        };
        // SAFETY:
        // - waker_ref and its contents are valid for the duration of the call.
        // - The waker is only used for the duration of the call.
        // - Ref is Send + Sync, enforced by a test.
        // - All functions passed in the vtable expect a pointer to a Ref.
        // - All functions passed in the vtable perform only thread-safe operations.
        let waker = unsafe {
            Waker::from_raw(RawWaker::new(
                std::ptr::from_ref(&waker_ref).cast(),
                &RawWakerVTable::new(ref_clone::<N>, ref_wake::<N>, ref_wake::<N>, ref_drop),
            ))
        };
        let mut cx = Context::from_waker(&waker);
        f(&mut cx)
    }
}

unsafe fn ref_clone<const N: usize>(ptr: *const ()) -> RawWaker {
    // SAFETY: This function is only called through our own waker, which guarantees that the
    // pointer is valid and pointing to a Ref.
    let thing: &Ref<'_, '_, N> = unsafe { &*(ptr.cast()) };
    thing.inner.set(thing.index, thing.cx_waker);
    let waker = thing.inner.clone();
    RawWaker::new(
        Arc::into_raw(waker).cast(),
        &RawWakerVTable::new(
            val_clone::<N>,
            val_wake::<N>,
            val_wake_by_ref::<N>,
            val_drop::<N>,
        ),
    )
}

unsafe fn ref_wake<const N: usize>(ptr: *const ()) {
    // SAFETY: This function is only called through our own waker, which guarantees that the
    // pointer is valid and pointing to a Ref.
    let thing: &Ref<'_, '_, N> = unsafe { &*(ptr.cast()) };
    thing.inner.wake();
    thing.cx_waker.wake_by_ref();
}

unsafe fn ref_drop(_: *const ()) {}

unsafe fn val_drop<const N: usize>(ptr: *const ()) {
    // SAFETY: This function is only called through our own waker, which guarantees that the
    // pointer is valid and pointing to a Arc<Inner>.
    unsafe { Arc::decrement_strong_count(ptr.cast::<Inner<N>>()) };
}

unsafe fn val_wake_by_ref<const N: usize>(ptr: *const ()) {
    // SAFETY: This function is only called through our own waker, which guarantees that the
    // pointer is valid and pointing to a Arc<Inner>.
    let waker = unsafe { &*ptr.cast::<Inner<N>>() };
    waker.wake();
}

unsafe fn val_wake<const N: usize>(ptr: *const ()) {
    // SAFETY: This function is only called through our own waker, which guarantees that the
    // pointer is valid and pointing to a Arc<Inner>.
    let waker = unsafe { Arc::from_raw(ptr.cast::<Inner<N>>()) };
    waker.wake();
}

unsafe fn val_clone<const N: usize>(ptr: *const ()) -> RawWaker {
    // SAFETY: This function is only called through our own waker, which guarantees that the
    // pointer is valid and pointing to a Arc<Inner>.
    unsafe {
        Arc::increment_strong_count(ptr.cast::<Inner<N>>());
    }
    RawWaker::new(
        ptr,
        &RawWakerVTable::new(
            val_clone::<N>,
            val_wake::<N>,
            val_wake_by_ref::<N>,
            val_drop::<N>,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::MultiWaker;
    use futures::executor::block_on;
    use parking_lot::Mutex;
    use std::future::poll_fn;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;
    use std::time::Duration;

    #[derive(Default)]
    struct SlimEvent {
        state: Mutex<SlimEventState>,
    }

    #[derive(Default)]
    struct SlimEventState {
        done: bool,
        waker: Option<Waker>,
    }

    impl SlimEvent {
        fn signal(&self) {
            let mut state = self.state.lock();
            state.done = true;
            let waker = state.waker.take();
            drop(state);
            if let Some(waker) = waker {
                waker.wake();
            }
        }

        fn poll_wait(&self, cx: &mut Context<'_>) -> Poll<()> {
            let mut state = self.state.lock();
            if state.done {
                Poll::Ready(())
            } else {
                let _old = state.waker.insert(cx.waker().clone());
                drop(state);
                Poll::Pending
            }
        }
    }

    #[test]
    fn test_multiwaker() {
        let mw = Arc::new(MultiWaker::<2>::new());
        let event = Arc::new(SlimEvent::default());
        let f = |index| {
            let mw = mw.clone();
            let event = event.clone();
            move || {
                block_on(async {
                    poll_fn(|cx| mw.poll_wrapped(cx, index, |cx| event.poll_wait(cx))).await
                })
            }
        };
        let t1 = std::thread::spawn(f(0));
        let t2 = std::thread::spawn(f(1));
        std::thread::sleep(Duration::from_millis(100));
        event.signal();

        t1.join().unwrap();
        t2.join().unwrap();
    }

    #[test]
    fn ref_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<super::Ref<'_, '_, 1>>();
        assert_sync::<super::Ref<'_, '_, 1>>();
    }
}
