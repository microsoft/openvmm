// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a tracing filter to restrict logging of events that are marked
//! as [`CVM_CONFIDENTIAL`].

// How it works:
// The magic value [`tracing::field::Empty`] will cause that field to be omitted
// from the tracing output. This allows us to use a field with that value as a
// metadata tag on individual events without polluting the output.

#![forbid(unsafe_code)]

use tracing::Subscriber;
use tracing::field::Empty;
use tracing_subscriber::filter::FilterFn;
use tracing_subscriber::layer::Filter;

/// A marker that can be used to tag events that are safe to log out of a
/// confidential environment.
pub const CVM_ALLOWED: Empty = Empty;

/// A marker that can be used to tag events that are confidential and should
/// not be logged out of a confidential environment.
pub const CVM_CONFIDENTIAL: Empty = Empty;

/// A tracing filter that will block events that are marked as [`CVM_CONFIDENTIAL`].
pub fn confidential_event_filter<S: Subscriber>() -> impl Filter<S> {
    FilterFn::new(move |m| m.fields().field("CVM_CONFIDENTIAL").is_none())
}

#[cfg(test)]
mod test {
    use crate::CVM_ALLOWED;
    use crate::CVM_CONFIDENTIAL;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use tracing::Subscriber;
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    struct TestLayer {
        count: Arc<AtomicU32>,
    }

    impl<S: Subscriber> Layer<S> for TestLayer {
        fn on_event(
            &self,
            _event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn create_test_subscriber() -> (Arc<AtomicU32>, impl Subscriber + Send + Sync + 'static) {
        let filter = crate::confidential_event_filter();
        let count = Arc::new(AtomicU32::new(0));
        let layer = TestLayer {
            count: count.clone(),
        };
        (
            count,
            tracing_subscriber::registry().with(layer.with_filter(filter)),
        )
    }

    fn log_test_events(subscriber: impl Subscriber + Send + Sync + 'static) {
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(foo = 4, "unknown trace");
            tracing::debug!(bar = 82, "unknown debug");
            tracing::info!("unknown info");
            tracing::warn!("unknown warn");
            tracing::event!(tracing::Level::ERROR, "unknown error");

            tracing::trace!(foo = 4, CVM_ALLOWED, "safe trace");
            tracing::debug!(CVM_ALLOWED, bar = 82, "safe debug");
            tracing::info!(?CVM_ALLOWED, "safe info");
            tracing::warn!(CVM_ALLOWED, "safe warn");
            tracing::event!(tracing::Level::ERROR, CVM_ALLOWED, "safe error");

            tracing::trace!(foo = 4, CVM_CONFIDENTIAL, "confidential trace");
            tracing::debug!(CVM_CONFIDENTIAL, bar = 82, "confidential debug");
            tracing::info!(?CVM_CONFIDENTIAL, "confidential info");
            tracing::warn!(CVM_CONFIDENTIAL, "confidential warn");
            tracing::event!(
                tracing::Level::ERROR,
                CVM_CONFIDENTIAL,
                "confidential error"
            );
        });
    }

    #[test]
    fn it_works() {
        let (count, subscriber) = create_test_subscriber();
        log_test_events(subscriber);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 10);
    }
}
