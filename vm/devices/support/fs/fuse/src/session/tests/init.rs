// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the flags that a file system's `init` negotiates in `FUSE_INIT`,
//! and of the `destroy` that follows each `init`.

use super::harness::dispatch_request;
use super::harness::make_request;
use super::*;
use crate::session::saved_state::SessionInfoState;
use crate::session::saved_state::SessionState;
use std::sync::Barrier;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use test_with_tracing::test;

/// A file system whose `init` wants flags whether or not the client offered
/// them.
struct UnconditionalWantFs {
    want: u32,
    want2: u32,
}

impl Fuse for UnconditionalWantFs {
    fn init(&self, info: &mut SessionInfo) {
        info.want |= self.want;
        info.want2 |= self.want2;
    }
}

/// A file system that wants a flag or an extended flag that the client did
/// not offer fails `FUSE_INIT` with `EIO`, which leaves the session
/// uninitialized, without a negotiation to save.
#[test]
fn init_rejects_flags_the_client_did_not_offer() {
    // (flags, flags2) that the client offers, and (want, want2) that the file
    // system adds.
    let cases = [
        ((FUSE_ASYNC_READ, 0), (FUSE_DO_READDIRPLUS, 0)),
        (
            (FUSE_ASYNC_READ | FUSE_INIT_EXT, 0),
            (0, FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2),
        ),
    ];
    for ((flags, flags2), (want, want2)) in cases {
        let session = Session::new(UnconditionalWantFs { want, want2 });
        let request = make_init_request(7, 39, 131072, flags, flags2);
        let (header, reply) = dispatch_request(&session, &request);
        assert_eq!(header.error, -lx::EIO);
        assert!(reply.is_empty());
        assert!(!session.is_initialized());
        assert_eq!(session.save_state(), SessionState::default());
    }
}

/// A file system whose `init` replaces the session information with one that
/// claims another version and every capability.
struct ForgingFs {
    want: u32,
}

impl Fuse for ForgingFs {
    fn init(&self, info: &mut SessionInfo) {
        *info = SessionInfoState {
            major: FUSE_KERNEL_VERSION + 1,
            minor: 0,
            capable: !0,
            capable2: !0,
            want: info.want | self.want,
            ..SessionInfoState::from(*info)
        }
        .into();
    }
}

/// The session keeps the version and the capabilities that the client sent,
/// whatever `init` does to the session information: claiming every capability
/// neither lets the file system want a flag that the client did not offer,
/// nor changes what the session saves.
#[test]
fn init_keeps_the_client_version_and_capabilities() {
    let request = make_init_request(7, 31, 131072, FUSE_ASYNC_READ, 0);

    let session = Session::new(ForgingFs {
        want: FUSE_DO_READDIRPLUS,
    });
    let (header, reply) = dispatch_request(&session, &request);
    assert_eq!(header.error, -lx::EIO);
    assert!(reply.is_empty());
    assert!(!session.is_initialized());
    assert_eq!(session.save_state(), SessionState::default());

    let session = Session::new(ForgingFs { want: 0 });
    let (header, reply) = dispatch_request(&session, &request);
    assert_eq!(header.error, 0);
    let (out, _) = fuse_init_out::read_from_prefix(&reply).unwrap();
    assert_eq!(out.flags, FUSE_ASYNC_READ);
    let saved = session.save_state();
    assert!(saved.initialized);
    assert_eq!(
        (
            saved.info.major,
            saved.info.minor,
            saved.info.capable,
            saved.info.capable2
        ),
        (FUSE_KERNEL_VERSION, 31, FUSE_ASYNC_READ, 0)
    );
    saved.validate().unwrap();
}

/// A file system that records the session information that each `init`
/// receives, and that allows more background requests only when the client
/// offers asynchronous direct I/O.
#[derive(Default)]
struct OfferDependentFs {
    received: Arc<Mutex<Vec<SessionInfoState>>>,
}

impl Fuse for OfferDependentFs {
    fn init(&self, info: &mut SessionInfo) {
        self.received.lock().push((*info).into());
        if info.capable() & FUSE_ASYNC_DIO != 0 {
            info.max_background = 64;
            info.congestion_threshold = 48;
        }
    }
}

/// A sender that cannot send a reply, as a virtio-fs reply fails when the
/// client's buffers cannot hold it, and that counts the replies that the
/// session tries to send.
#[derive(Default)]
struct FailingSender {
    attempts: usize,
}

impl ReplySender for FailingSender {
    fn send(&mut self, _bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        self.attempts += 1;
        Err(io::ErrorKind::WriteZero.into())
    }
}

/// Sends `request` to `session` through a sender that cannot send the reply,
/// and checks that the session tries to send exactly one reply.
fn dispatch_request_failing_reply(session: &Session, request: &[u8]) {
    let mut sender = FailingSender::default();
    session.dispatch(Request::new(request).unwrap(), &mut sender, None);
    assert_eq!(sender.attempts, 1);
}

/// The `FUSE_INIT` that the tests send first, whose reply cannot be sent,
/// which offers asynchronous direct I/O and extended flags.
fn first_init_request() -> Vec<u8> {
    make_init_request(
        7,
        39,
        131072,
        FUSE_ASYNC_READ | FUSE_ASYNC_DIO | FUSE_INIT_EXT,
        FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2,
    )
}

/// A client that never received the reply to its `FUSE_INIT` agreed to no
/// negotiation, so the session stays uninitialized, without a negotiation to
/// save.
#[test]
fn init_whose_reply_fails_leaves_no_negotiation() {
    let session = Session::new(OfferDependentFs::default());
    dispatch_request_failing_reply(&session, &first_init_request());
    assert!(!session.is_initialized());
    assert_eq!(session.save_state(), SessionState::default());
}

/// A `FUSE_INIT` that follows one whose reply could not be sent negotiates as
/// on a new session: `init` receives, and the session replies and saves, only
/// what the new request offers.
#[test]
fn init_after_a_failed_reply_negotiates_afresh() {
    let retry = make_init_request(7, 31, 65536, FUSE_ASYNC_READ, 0);

    let fs = OfferDependentFs::default();
    let received = Arc::clone(&fs.received);
    let session = Session::new(fs);
    dispatch_request_failing_reply(&session, &first_init_request());
    let (header, reply) = dispatch_request(&session, &retry);

    let fresh_fs = OfferDependentFs::default();
    let fresh_received = Arc::clone(&fresh_fs.received);
    let fresh = Session::new(fresh_fs);
    let (_, fresh_reply) = dispatch_request(&fresh, &retry);

    assert_eq!(header.error, 0);
    let (out, _) = fuse_init_out::read_from_prefix(&reply).unwrap();
    assert_eq!((out.max_background, out.congestion_threshold), (0, 0));
    assert_eq!(reply, fresh_reply);
    assert_eq!(received.lock()[1..], fresh_received.lock()[..]);
    assert!(session.is_initialized());
    assert_eq!(session.save_state(), fresh.save_state());
}

/// A file system that records its calls to `init` and `destroy`, in order,
/// and whose `destroy` waits at `barrier` when it starts, and again before it
/// returns, which it records too.
struct BlockingDestroyFs {
    calls: Arc<Mutex<Vec<&'static str>>>,
    barrier: Arc<Barrier>,
}

impl Fuse for BlockingDestroyFs {
    fn init(&self, _info: &mut SessionInfo) {
        self.calls.lock().push("init");
    }

    fn destroy(&self) {
        self.calls.lock().push("destroy");
        self.barrier.wait();
        self.barrier.wait();
        self.calls.lock().push("destroyed");
    }
}

/// A `FUSE_INIT` that the session dispatches while it ends, once it has marked
/// itself uninitialized, as from another queue while the session handles
/// `FUSE_DESTROY`, waits until the session has ended: `init` runs only once
/// `destroy` has returned, and the session keeps the negotiation that it then
/// completes.
#[test]
fn init_waits_for_the_end_of_the_session() {
    let calls = Arc::default();
    let barrier = Arc::new(Barrier::new(2));
    let session = Session::new(BlockingDestroyFs {
        calls: Arc::clone(&calls),
        barrier: Arc::clone(&barrier),
    });
    let request = make_init_request(7, 39, 131072, FUSE_ASYNC_READ, 0);
    let (header, _) = dispatch_request(&session, &request);
    assert_eq!(header.error, 0);

    let (done, finished) = mpsc::channel();
    let (locked, finished_early, error) = thread::scope(|scope| {
        scope.spawn(|| session.destroy());
        // Wait until the file system's `destroy` has started, by which time the
        // session has marked itself uninitialized.
        barrier.wait();
        // FUSE_INIT negotiates under this lock, which the session must hold
        // until it has ended.
        let locked = session.info.try_read().is_none();
        let init = scope.spawn(|| {
            let (header, _) = dispatch_request(&session, &request);
            done.send(()).unwrap();
            header.error
        });
        let finished_early = finished.recv_timeout(Duration::from_millis(50)).is_ok();
        // Let `destroy` return.
        barrier.wait();
        (locked, finished_early, init.join().unwrap())
    });
    assert!(locked, "the session was not locked while it was ending");
    assert!(
        !finished_early,
        "FUSE_INIT completed while the session was ending"
    );
    assert_eq!(error, 0);
    assert_eq!(*calls.lock(), ["init", "destroy", "destroyed", "init"]);
    let saved = session.save_state();
    assert!(saved.initialized);
    saved.validate().unwrap();
}

/// A file system that records its calls to `init` and `destroy`, in order,
/// and whose `init` wants `want` whether or not the client offered it.
struct RecordingFs {
    calls: Arc<Mutex<Vec<&'static str>>>,
    want: u32,
}

impl Fuse for RecordingFs {
    fn init(&self, info: &mut SessionInfo) {
        self.calls.lock().push("init");
        info.want |= self.want;
    }

    fn destroy(&self) {
        self.calls.lock().push("destroy");
    }
}

/// Returns a session of a [`RecordingFs`] that wants `want`, and the calls
/// that the file system records.
fn recording_session(want: u32) -> (Session, Arc<Mutex<Vec<&'static str>>>) {
    let calls = Arc::default();
    let session = Session::new(RecordingFs {
        calls: Arc::clone(&calls),
        want,
    });
    (session, calls)
}

/// A session that refuses the negotiation of an `init` calls `destroy` before
/// the client can retry `FUSE_INIT`, so the retry's `init` starts from a
/// file system with nothing set up, and the session that the retry completes
/// calls `destroy` once when it ends.
#[test]
fn refused_init_is_destroyed_before_a_retry() {
    let (session, calls) = recording_session(FUSE_DO_READDIRPLUS);

    let request = make_init_request(7, 39, 131072, FUSE_ASYNC_READ, 0);
    let (header, _) = dispatch_request(&session, &request);
    assert_eq!(header.error, -lx::EIO);
    assert_eq!(*calls.lock(), ["init", "destroy"]);

    let request = make_init_request(7, 39, 131072, FUSE_ASYNC_READ | FUSE_DO_READDIRPLUS, 0);
    let (header, _) = dispatch_request(&session, &request);
    assert_eq!(header.error, 0);
    assert!(session.is_initialized());
    assert_eq!(*calls.lock(), ["init", "destroy", "init"]);

    session.destroy();
    assert_eq!(*calls.lock(), ["init", "destroy", "init", "destroy"]);
}

/// A session that refused the negotiation of an `init` has already called
/// `destroy` for it, so ending the session does not call it again.
#[test]
fn refused_init_is_not_destroyed_again_when_the_session_ends() {
    let (session, calls) = recording_session(FUSE_DO_READDIRPLUS);

    let request = make_init_request(7, 39, 131072, FUSE_ASYNC_READ, 0);
    let (header, _) = dispatch_request(&session, &request);
    assert_eq!(header.error, -lx::EIO);
    assert_eq!(*calls.lock(), ["init", "destroy"]);

    session.destroy();
    drop(session);
    assert_eq!(*calls.lock(), ["init", "destroy"]);
}

/// A session that cannot send its reply to `FUSE_INIT` does not adopt the
/// negotiation, as when it refuses it, so it calls `destroy` before the
/// client can retry `FUSE_INIT`.
#[test]
fn init_whose_reply_fails_is_destroyed_before_a_retry() {
    let (session, calls) = recording_session(0);

    let request = make_init_request(7, 39, 131072, FUSE_ASYNC_READ, 0);
    dispatch_request_failing_reply(&session, &request);
    assert!(!session.is_initialized());
    assert_eq!(*calls.lock(), ["init", "destroy"]);

    let (header, _) = dispatch_request(&session, &request);
    assert_eq!(header.error, 0);
    assert!(session.is_initialized());
    assert_eq!(*calls.lock(), ["init", "destroy", "init"]);
}

/// A session that adopts the negotiation of an `init` calls `destroy` once
/// when it ends, whether on `FUSE_DESTROY` or when its owner ends it, however
/// often its owner then ends it.
#[test]
fn adopted_init_is_destroyed_once_when_the_session_ends() {
    for fuse_destroy in [false, true] {
        let (session, calls) = recording_session(0);

        let request = make_init_request(7, 39, 131072, FUSE_ASYNC_READ, 0);
        let (header, _) = dispatch_request(&session, &request);
        assert_eq!(header.error, 0);
        assert_eq!(*calls.lock(), ["init"]);

        if fuse_destroy {
            let (header, reply) =
                dispatch_request(&session, &make_request(FUSE_DESTROY, 2, &(), &[]));
            assert_eq!(header.error, 0);
            assert!(reply.is_empty());
            assert!(!session.is_initialized());
        }
        session.destroy();
        session.destroy();
        drop(session);
        assert_eq!(*calls.lock(), ["init", "destroy"]);
    }
}
