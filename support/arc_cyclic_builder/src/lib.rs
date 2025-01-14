// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An extension to [`std::sync::Arc`] that adds
//! [`Arc::new_cyclic_builder`](ArcCyclicBuilderExt::new_cyclic_builder), and
//! [`ArcCyclicBuilder<T>`] - a generalization of [`Arc::new_cyclic`].
//!
//! This comes in handy when dealing with objects that have fallible / async
//! constructors. In these cases, the fact that `Arc::new_cyclic` takes an
//! infallible, synchronous closure precludes it from being used.
//!
//! # Example
//!
//! Constructing a self-referential `Gadget` with a fallible async constructor.
//!
//! ```
//! use arc_cyclic_builder::ArcCyclicBuilderExt;
//! use std::io;
//! use std::sync::{Arc, Weak};
//!
//! struct Gadget {
//!     me: Weak<Gadget>,
//! }
//!
//! impl Gadget {
//!     async fn new(me: Weak<Gadget>) -> io::Result<Self> {
//!         Ok(Gadget { me })
//!     }
//! }
//!
//! async fn create_gadget() -> io::Result<Arc<Gadget>> {
//!     let builder = Arc::new_cyclic_builder();
//!     let gadget = Gadget::new(builder.weak()).await?;
//!     Ok(builder.build(gadget))
//! }
//! ```
//!
//! # (Un)Safety
//!
//! At the time of writing (8/22/2022), the stable public APIs of `Arc` and
//! `Weak` are not sufficient to robustly implement `ArcCyclicBuilder` outside
//! the context of the std itself. Instead, we've had to do something quite
//! unsafe to get this code working...
//!
//! Namely, we've had to make assumptions about the internal representation of
//! `Arc` and `Weak`, and written the code assuming they will not change out
//! from under us.
//!
//! This is, by all accounts, a Really Bad Idea™️, since the std makes no
//! guarantees as to the stability of these type's _internal_ representations,
//! and could _silently_ change them at any point.
//!
//! # Road to Safety
//!
//! ...that said, we're willing to bet that it's _highly unlikely_ that the
//! representation of `Arc`/`Weak` is going to change in the near future, and
//! that this code will continue to work fine (at least for a while).
//!
//! Of course, leaving this kind of risk in the codebase isn't a
//! great idea, as while unit tests and MIRI tests serve as a reasonable
//! early-warning indicator if the `Arc`/`Weak` representations have changed,
//! ultimately, this code needs to land upstream in the std.
//!
//! TODO: add links to any upstream PRs we end up sending out

#![warn(missing_docs)]
// UNSAFETY: See crate-level doccomment.
#![expect(unsafe_code)]

use std::mem;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic;
use std::sync::atomic::Ordering::*;
use std::sync::Arc;
use std::sync::Weak;

// Matches the definition of `ArcInner` in the `std`
//
// The other important assumption: both `Arc` and `Weak` share the same repr:
//
// `struct Arc<T>  { ptr: NonNull<ArcInner<T>> }`
// `struct Weak<T> { ptr: NonNull<ArcInner<T>> }`
#[repr(C)]
struct ArcInner<T> {
    strong: atomic::AtomicUsize,
    weak: atomic::AtomicUsize,
    data: T,
}

/// Builder returned by [`Arc::new_cyclic_builder`](ArcCyclicBuilderExt::new_cyclic_builder)
pub struct ArcCyclicBuilder<T> {
    init_ptr: NonNull<ArcInner<T>>,
    weak: Weak<T>,
}

// DEVNOTE: the bodies of `new` and `build` are essentially identical to the
// implementation of `Arc::new_cyclic` in std, aside from the use of some
// transmutes in liu of using Weak/Arc::from_inner (as ArcInner is not a
// publicly exported type).
impl<T> ArcCyclicBuilder<T> {
    fn new() -> Self {
        // Construct the inner in the "uninitialized" state with a single
        // weak reference.
        // NOTE: `Box::new` is replaced with the `box` keyword in std
        let uninit_ptr: NonNull<_> = Box::leak(Box::new(ArcInner {
            strong: atomic::AtomicUsize::new(0),
            weak: atomic::AtomicUsize::new(1),
            data: mem::MaybeUninit::<T>::uninit(),
        }))
        .into();
        let init_ptr: NonNull<ArcInner<T>> = uninit_ptr.cast();

        // SAFETY: equivalent of calling `Weak { ptr: init_ptr }`
        let weak = unsafe { mem::transmute::<NonNull<ArcInner<T>>, Weak<T>>(init_ptr) };

        Self { init_ptr, weak }
    }

    /// Obtain a `Weak<T>` to the allocation. Attempting to
    /// [`upgrade`](Weak::upgrade) the weak reference prior to invoking
    /// [`build`](Self::build) will fail and result in a `None` value.
    pub fn weak(&self) -> Weak<T> {
        self.weak.clone()
    }

    /// Finish construction of the `Arc<T>`
    pub fn build(self, data: T) -> Arc<T> {
        // Now we can properly initialize the inner value and turn our weak
        // reference into a strong reference.
        // SAFETY: self.init_ptr is guaranteed to point to our ArcInner,
        // which has the same layout as std's.
        let strong = unsafe {
            let inner = self.init_ptr.as_ptr();
            ptr::write(ptr::addr_of_mut!((*inner).data), data);

            // The above write to the data field must be visible to any threads which
            // observe a non-zero strong count. Therefore we need at least "Release" ordering
            // in order to synchronize with the `compare_exchange_weak` in `Weak::upgrade`.
            //
            // "Acquire" ordering is not required. When considering the possible behaviours
            // of `data_fn` we only need to look at what it could do with a reference to a
            // non-upgradeable `Weak`:
            // - It can *clone* the `Weak`, increasing the weak reference count.
            // - It can drop those clones, decreasing the weak reference count (but never to zero).
            //
            // These side effects do not impact us in any way, and no other side effects are
            // possible with safe code alone.
            let prev_value = (*inner).strong.fetch_add(1, Release);
            debug_assert_eq!(prev_value, 0, "No prior strong references should exist");

            // SAFETY: equivalent of calling `Arc::from_inner`
            mem::transmute::<NonNull<ArcInner<T>>, Arc<T>>(self.init_ptr)
        };

        // Strong references should collectively own a shared weak reference,
        // so don't run the destructor for our old weak reference.
        mem::forget(self.weak);
        strong
    }
}

/// An extension trait to [`Arc`] that adds
/// [`new_cyclic_builder`](Self::new_cyclic_builder).
pub trait ArcCyclicBuilderExt<T> {
    /// Return a new [`ArcCyclicBuilder<T>`]
    fn new_cyclic_builder() -> ArcCyclicBuilder<T>;
}

impl<T> ArcCyclicBuilderExt<T> for Arc<T> {
    fn new_cyclic_builder() -> ArcCyclicBuilder<T> {
        ArcCyclicBuilder::new()
    }
}

#[allow(clippy::disallowed_types)] // requiring parking_lot just for a test? nah
#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Mutex;

    struct Gadget {
        this: Weak<Gadget>,
        inner: Mutex<usize>,

        inc_on_drop: Arc<Mutex<usize>>,
    }

    #[derive(Debug)]
    struct PassedZero;

    impl Gadget {
        fn new(this: Weak<Gadget>, inner: usize, inc_on_drop: Arc<Mutex<usize>>) -> Gadget {
            Gadget {
                this,
                inner: Mutex::new(inner),
                inc_on_drop,
            }
        }

        fn try_new(
            this: Weak<Gadget>,
            inner: usize,
            inc_on_drop: Arc<Mutex<usize>>,
        ) -> Result<Gadget, PassedZero> {
            if inner == 0 {
                Err(PassedZero)
            } else {
                Ok(Gadget::new(this, inner, inc_on_drop))
            }
        }

        async fn async_new(
            this: Weak<Gadget>,
            inner: usize,
            inc_on_drop: Arc<Mutex<usize>>,
        ) -> Gadget {
            Gadget {
                this,
                inner: Mutex::new(inner),
                inc_on_drop,
            }
        }

        fn val(&self) -> usize {
            *self.inner.lock().unwrap()
        }

        fn bump_self(&self) {
            *self.this.upgrade().unwrap().inner.lock().unwrap() += 1;
        }
    }

    impl Drop for Gadget {
        fn drop(&mut self) {
            *self.inc_on_drop.lock().unwrap() += 1
        }
    }

    #[test]
    fn smoke() {
        let inc_on_drop = Arc::new(Mutex::new(0));

        let builder = Arc::new_cyclic_builder();
        let gadget = Gadget::new(builder.weak(), 1, inc_on_drop.clone());
        assert!(builder.weak().upgrade().is_none());
        let gadget = builder.build(gadget);

        gadget.bump_self();
        assert_eq!(gadget.val(), 2);

        drop(gadget);
        assert_eq!(*inc_on_drop.lock().unwrap(), 1);
    }

    // showing off how the builder can be used to
    #[test]
    fn smoke_fallible_ok() {
        let inc_on_drop = Arc::new(Mutex::new(0));

        let builder = Arc::new_cyclic_builder();
        let gadget = Gadget::try_new(builder.weak(), 1, inc_on_drop.clone()).unwrap();
        assert!(builder.weak().upgrade().is_none());
        let gadget = builder.build(gadget);
        gadget.bump_self();
        assert_eq!(gadget.val(), 2);

        drop(gadget);
        assert_eq!(*inc_on_drop.lock().unwrap(), 1);
    }

    #[test]
    fn smoke_async_construction() {
        let inc_on_drop = Arc::new(Mutex::new(0));

        let builder = Arc::new_cyclic_builder();

        let gadget = futures_executor::block_on(async {
            Gadget::async_new(builder.weak(), 1, inc_on_drop.clone()).await
        });
        assert!(builder.weak().upgrade().is_none());
        let gadget = builder.build(gadget);
        gadget.bump_self();
        assert_eq!(gadget.val(), 2);

        drop(gadget);
        assert_eq!(*inc_on_drop.lock().unwrap(), 1);
    }

    #[test]
    fn drop_the_builder() {
        let builder: ArcCyclicBuilder<usize> = Arc::new_cyclic_builder();
        let weak = builder.weak();
        drop(builder);
        assert!(weak.upgrade().is_none());
        drop(weak);
    }
}
