// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! `Stream` combinators shared across the vmbus channel/client stack.

use core::pin::Pin;
use core::task::Context;
use core::task::Poll;
use futures::StreamExt;

/// A [`futures::Stream`] adapter that tags each item produced by the
/// inner stream with a caller-supplied `T`. Yields `(tag, Some(item))`
/// for each inner item, then a single `(tag, None)` sentinel when the
/// inner stream ends, and `None` thereafter.
#[derive(Debug)]
pub struct TaggedStream<T, S>(Option<T>, S);

impl<T: Clone, S: futures::Stream + Unpin> TaggedStream<T, S> {
    /// Wraps `s` with the tag `t`.
    pub fn new(t: T, s: S) -> Self {
        Self(Some(t), s)
    }

    /// Returns the tag, or `None` if the inner stream has terminated
    /// and the terminal `(tag, None)` sentinel has already been
    /// yielded.
    pub fn value(&self) -> Option<&T> {
        self.0.as_ref()
    }
}

impl<T: Clone, S: futures::Stream + Unpin> futures::Stream for TaggedStream<T, S>
where
    Self: Unpin,
{
    type Item = (T, Option<S::Item>);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(t) = this.0.clone() {
            let v = core::task::ready!(this.1.poll_next_unpin(cx));
            if v.is_none() {
                // Return `None` next time poll_next is called.
                this.0 = None;
            }
            Poll::Ready(Some((t, v)))
        } else {
            Poll::Ready(None)
        }
    }
}
