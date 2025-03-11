// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A mechanism for efficiently selecting between futures.
//!
//! In async code, it is common to select between the completion of two or more
//! futures. In this case, a naive implementation of select will poll each
//! future during each wakeup. If the poll functions are expensive (because they
//! takes locks, makes syscalls, or otherwise performs some computationally
//! expensive task), then this can contribute to performance problems,
//! especially in heavily-nested async code.
//!
//! This crate contains an [implementation of select](FastSelect::select) that
//! constructs a separate waker for each alternative future, allowing `select`'s
//! poll implementation to identify exactly which futures are ready to be
//! polled.

// UNSAFETY: Using unchecked raw Arc, Pin, and Waker APIs.
#![expect(unsafe_code)]

use parking_lot::Mutex;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::pin::pin;
use std::pin::Pin;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::RawWaker;
use std::task::RawWakerVTable;
use std::task::Waker;

/// An object that can be used to efficiently select over alternative futures.
///
/// This allocates storage used by calls to [`select`](Self::select). Be careful
/// to preallocate any instances of this outside the hot path.
///
/// # Example
///
/// ```rust
/// # use futures::StreamExt;
/// # use futures::executor::block_on;
/// # use futures::channel::mpsc::unbounded;
/// # use fast_select::FastSelect;
/// # block_on(async {
/// let mut fast_select = FastSelect::new();
/// let (_cancel_send, mut cancel_recv) = unbounded::<()>();
/// loop {
///     let operation = async {
///         Some(5)
///     };
///     let cancelled = async {
///         let _ = cancel_recv.next().await;
///         None
///     };
///     if let Some(value) = fast_select.select((operation, cancelled)).await {
///         break value;
///     }
/// }
/// # });
/// ```
///
/// In cases where one future is much more common than the others, you can leave
/// that future out and use a traditional select macro or function to select
/// between the common future and the tuple with the remaining futures. This may
/// even be a tuple of length one. In this case, the common future will be
/// polled every iteration, while the uncommon futures will be only polled as
/// necessary.
///
/// For example:
///
/// ```rust
/// # use futures::FutureExt;
/// # use futures::executor::block_on;
/// # use std::future::pending;
/// # use fast_select::FastSelect;
/// # block_on(async {
/// let mut fast_select = FastSelect::new();
/// futures::select_biased! {
///     value = async { 5u32 }.fuse() => {
///         println!("{}", value);
///     }
///     _ = fast_select.select((pending::<u32>(),)).fuse() => {
///         unreachable!()
///     }
/// }
/// # });
/// ```
#[derive(Default, Debug)]
pub struct FastSelect {
    state: Arc<State>,
}

#[derive(Debug)]
struct SelectPoll<'a, T> {
    poll_state: PollState<'a>,
    futures: T,
}

impl FastSelect {
    /// Creates a new [`FastSelect`].
    pub fn new() -> Self {
        Default::default()
    }

    /// Selects between the futures in tuple `futures`.
    ///
    /// Returns the output of the first one that completes. All the other
    /// futures are dropped without being completed.
    ///
    /// The futures are polled in the order they are specified in the tuple, so
    /// there is a bias for earlier ones in the tuple.
    pub async fn select<T: Select>(&mut self, futures: T) -> T::Output {
        assert!(T::COUNT <= 32);

        SelectPoll {
            poll_state: PollState {
                state: &self.state,
                last_waker: Default::default(),
                poll: (1u32 << (T::COUNT % 32)).wrapping_sub(1),
            },
            futures: pin!(futures),
        }
        .await
    }
}

impl<T: Select> Future for SelectPoll<'_, Pin<&mut T>> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.futures.as_mut().poll_select(cx, &mut this.poll_state)
    }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct PollState<'a> {
    state: &'a Arc<State>,
    last_waker: LastWaker,
    poll: u32,
}

impl PollState<'_> {
    fn refill(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        while self.poll == 0 {
            if self.state.poll.load(Ordering::Relaxed) != 0 {
                self.poll = self.state.poll.swap(0, Ordering::Acquire);
            }
            if self.poll != 0 {
                // The waker in `state` was probably taken and dropped.
                self.last_waker.clear();
            } else {
                if let Some(waker) = self.last_waker.update_waker(cx) {
                    // Update the locked waker and loop around to check
                    // `state.poll` again.
                    *self.state.waker.lock() = Some(waker);
                } else {
                    // The waker is up to date, so do nothing.
                    return Poll::Pending;
                }
            }
        }
        Poll::Ready(())
    }
}

/// A sealed trait for tuple types that can be selected over with
/// [`FastSelect`].
pub trait Select: private::Sealed {
    #[doc(hidden)]
    /// The number of elements in the tuple.
    const COUNT: usize;
    #[doc(hidden)]
    /// The output type of the tuple futures.
    type Output;

    #[doc(hidden)]
    fn poll_select(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        state: &mut PollState<'_>,
    ) -> Poll<Self::Output>;
}

mod private {
    pub trait Sealed {}
}

macro_rules! gen_future {
    ( $count:expr, $(($t:tt, $n:tt)),* ) => {
        impl<R, $($t: Future<Output = R>,)*> private::Sealed for ($($t,)*) {}

        impl<R, $($t: Future<Output = R>,)*> Select for ($($t,)*) {
            const COUNT: usize = $count;
            type Output = R;

            fn poll_select(self: Pin<&mut Self>, cx: &mut Context<'_>, state: &mut PollState<'_>) -> Poll<R> {
                // SAFETY: unpinning in order to re-pin each tuple element one
                // at a time. This is safe because each element is only accessed
                // via a pinned pointer.
                let this = unsafe { self.get_unchecked_mut() };
                loop {
                    std::task::ready!(state.refill(cx));
                    $(
                    if state.poll & (1<<$n) != 0 {
                        state.poll &= !(1<<$n);
                        // SAFETY: repinning as described above.
                        if let Poll::Ready(r) = unsafe { Pin::new_unchecked(&mut this.$n) }
                            .poll(&mut Context::from_waker(&state.state.waker_ref($n)))
                        {
                            return Poll::Ready(r);
                        }
                    }
                    )*
                }
            }
        }
    };
}

gen_future!(1, (T0, 0));
gen_future!(2, (T0, 0), (T1, 1));
gen_future!(3, (T0, 0), (T1, 1), (T2, 2));
gen_future!(4, (T0, 0), (T1, 1), (T2, 2), (T3, 3));
gen_future!(5, (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4));
gen_future!(6, (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5));

#[derive(Debug, Default)]
struct LastWaker {
    last_waker: Option<RawWaker>,
}

// SAFETY: LastWaker contains a RawWaker (which is not inherently Send/Sync),
// but it is used only for comparisons.
unsafe impl Send for LastWaker {}
// SAFETY: LastWaker contains a RawWaker (which is not inherently Send/Sync),
// but it is used only for comparisons.
unsafe impl Sync for LastWaker {}

fn raw_waker_copy(waker: &Waker) -> RawWaker {
    // FUTURE: use Waker::as_raw and RawWaker::{data, vtable} once stabilized to
    // avoid unsafe here.
    //
    // SAFETY: Waker is repr(transparent) over RawWaker. RawWaker is safe to
    // copy because it is just a wrapper around two pointers, and it has no Drop
    // implementation.
    unsafe { std::ptr::from_ref(waker).cast::<RawWaker>().read() }
}

impl LastWaker {
    fn clear(&mut self) {
        self.last_waker = None;
    }

    fn update_waker(&mut self, cx: &Context<'_>) -> Option<Waker> {
        if self.last_waker == Some(raw_waker_copy(cx.waker())) {
            return None;
        }
        let waker = cx.waker().clone();
        self.last_waker = Some(raw_waker_copy(&waker));
        Some(waker)
    }
}

#[repr(C, align(4))]
#[derive(Default, Debug)]
struct State {
    poll: AtomicU32,
    waker: Mutex<Option<Waker>>,
}

impl State {
    fn wake(&self, i: usize) {
        let old = self.poll.fetch_or(1 << i, Ordering::Release);
        if old == 0 {
            let waker = self.waker.lock().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// Gets the pointer and wake index from the data pointer.
    unsafe fn from_ptr(data: *const ()) -> (ManuallyDrop<Arc<Self>>, usize) {
        let align_mask = align_of::<Self>() - 1;
        let i = (data as usize) & align_mask;
        let this = (data as usize & !align_mask) as *const Self;
        // SAFETY: caller guarantees that this is a valid reference.
        let this = unsafe { Arc::from_raw(this) };
        (ManuallyDrop::new(this), i)
    }

    unsafe fn clone_fn(data: *const ()) -> RawWaker {
        // SAFETY: caller guarantees this is a valid data pointer.
        let (this, _) = unsafe { Self::from_ptr(data) };
        let _ = Arc::into_raw(Arc::clone(&this));
        RawWaker::new(
            data,
            &RawWakerVTable::new(
                Self::clone_fn,
                Self::wake_fn,
                Self::wake_by_ref_fn,
                Self::drop_fn,
            ),
        )
    }

    unsafe fn wake_fn(data: *const ()) {
        // SAFETY: caller guarantees this is a valid data pointer.
        let (this, i) = unsafe { Self::from_ptr(data) };
        let this = ManuallyDrop::into_inner(this);
        this.wake(i);
    }

    unsafe fn wake_by_ref_fn(data: *const ()) {
        // SAFETY: caller guarantees this is a valid data pointer.
        let (this, i) = unsafe { Self::from_ptr(data) };
        this.wake(i);
    }

    unsafe fn drop_fn(data: *const ()) {
        // SAFETY: caller guarantees this is a valid data pointer.
        let (this, _) = unsafe { Self::from_ptr(data) };
        drop(ManuallyDrop::into_inner(this));
    }

    fn waker_ref<'a>(self: &'a Arc<Self>, i: usize) -> WakerRef<'a> {
        let data = ((Arc::as_ptr(self) as usize) | i) as *const ();
        let waker = RawWaker::new(
            data,
            &RawWakerVTable::new(
                Self::clone_fn,
                Self::wake_by_ref_fn,
                Self::wake_by_ref_fn,
                |_| (),
            ),
        );
        // SAFETY: the vtable methods implement the waker contract.
        let waker = unsafe { Waker::from_raw(waker) };
        WakerRef {
            waker,
            _phantom: PhantomData,
        }
    }
}

struct WakerRef<'a> {
    waker: Waker,
    _phantom: PhantomData<&'a ()>,
}

impl Deref for WakerRef<'_> {
    type Target = Waker;

    fn deref(&self) -> &Self::Target {
        &self.waker
    }
}

#[cfg(test)]
mod tests {
    use crate::FastSelect;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use pal_async::DefaultDriver;
    use std::future::pending;
    use std::time::Duration;

    #[async_test]
    async fn test_foo(driver: DefaultDriver) {
        let mut select = FastSelect::new();
        let mut timer = PolledTimer::new(&driver);
        select
            .select((pending(), pending(), timer.sleep(Duration::from_millis(30))))
            .await;
    }
}
