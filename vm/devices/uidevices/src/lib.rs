// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Vmbus UI devices (mouse, keyboard, video).

#![forbid(unsafe_code)]

pub mod keyboard;
pub mod mouse;
pub mod resolver;
pub mod video;

#[cfg(test)]
mod test_helpers {
    use futures::FutureExt;
    use input_core::InputSource;
    use pal_async::DefaultDriver;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::task::Context;
    use std::task::Poll;
    use std::time::Duration;
    use task_control::AsyncRun;
    use task_control::TaskControl;

    pub struct GatedInput<T> {
        data: Option<T>,
        polls: Arc<AtomicUsize>,
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    pub struct InputGate {
        pub polls: Arc<AtomicUsize>,
        entered: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
    }

    pub fn gated_input<T>(data: Option<T>) -> (GatedInput<T>, InputGate) {
        let polls = Arc::new(AtomicUsize::new(0));
        let (entered, recv_entered) = mpsc::channel();
        let (release, recv_release) = mpsc::channel();
        (
            GatedInput {
                data,
                polls: polls.clone(),
                entered: Some(entered),
                release: recv_release,
            },
            InputGate {
                polls,
                entered: recv_entered,
                release,
            },
        )
    }

    impl<T: Copy + Unpin> futures::Stream for GatedInput<T> {
        type Item = T;

        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<T>> {
            let this = self.get_mut();
            let poll = this.polls.fetch_add(1, Ordering::Relaxed);
            // Let the test request stop after the active loop has consumed its
            // first checkpoint, but before its first input poll returns.
            if let Some(entered) = this.entered.take() {
                entered.send(()).unwrap();
                this.release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            if poll < 2 {
                if let Some(data) = this.data {
                    return Poll::Ready(Some(data));
                }
            }
            Poll::Pending
        }
    }

    impl<T: Copy + Unpin + Send> InputSource<T> for GatedInput<T> {
        fn set_active(&mut self, _: bool) -> Pin<Box<dyn '_ + Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }
    }

    impl InputGate {
        pub async fn stop<T: AsyncRun<()>>(
            &self,
            task: &mut TaskControl<T, ()>,
            driver: &DefaultDriver,
        ) {
            self.entered.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(task.stop().now_or_never().is_none());
            self.release.send(()).unwrap();
            let mut timer = pal_async::timer::PolledTimer::new(driver);
            match futures::future::select(
                std::pin::pin!(task.stop()),
                std::pin::pin!(timer.sleep(Duration::from_secs(5))),
            )
            .await
            {
                futures::future::Either::Left((stopped, _)) => assert!(stopped),
                futures::future::Either::Right(_) => panic!("active input worker did not stop"),
            }
        }
    }
}
