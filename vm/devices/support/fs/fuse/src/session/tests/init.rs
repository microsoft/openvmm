// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the flags that a file system's `init` negotiates in `FUSE_INIT`.

use super::harness::dispatch_request;
use super::*;
use crate::session::saved_state::SessionInfoState;
use crate::session::saved_state::SessionState;
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
