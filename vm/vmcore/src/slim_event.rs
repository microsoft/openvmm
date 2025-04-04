// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Event support.

use futures::Stream;
use parking_lot::Mutex;
use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

/// An event for signaling a task, without requiring an OS event.
#[derive(Debug, Default)]
pub struct SlimEvent {
    state: Mutex<State>,
}

#[derive(Debug, Clone, Default)]
struct State {
    signaled: bool,
    waker: Option<Waker>,
}

impl SlimEvent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals the event.
    pub fn signal(&self) {
        let waker = {
            let mut state = self.state.lock();
            state.signaled = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Polls the event.
    pub fn poll_wait(&self, cx: &mut Context<'_>) -> Poll<()> {
        let _dead_waker;
        let mut state = self.state.lock();
        if state.signaled {
            state.signaled = false;
            _dead_waker = state.waker.take();
            Poll::Ready(())
        } else {
            if !state
                .waker
                .as_ref()
                .map(|w| cx.waker().will_wake(w))
                .unwrap_or(false)
            {
                _dead_waker = state.waker.replace(cx.waker().clone());
            }
            Poll::Pending
        }
    }

    /// Waits for the event to be signaled.
    pub fn wait(&self) -> impl '_ + Unpin + Future<Output = ()> {
        poll_fn(move |cx| self.poll_wait(cx))
    }

    /// Returns a stream, with an entry for each received signal.
    pub fn as_stream(&self) -> SlimEventStream<'_> {
        SlimEventStream { wait: self }
    }
}

/// A stream of signals.
pub struct SlimEventStream<'a> {
    wait: &'a SlimEvent,
}

impl Stream for SlimEventStream<'_> {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        std::task::ready!(self.wait.poll_wait(cx));
        Poll::Ready(Some(()))
    }
}

#[cfg(test)]
mod tests {
    use crate::slim_event::SlimEvent;
    use futures::FutureExt;
    use futures::executor::block_on;

    #[test]
    fn test() {
        block_on(async {
            let e = SlimEvent::new();
            assert!(e.wait().now_or_never().is_none());
            e.signal();
            e.wait().await;
        })
    }
}
