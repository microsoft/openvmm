// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test harness that sends requests to a session and reads its replies.
//!
//! [`initialized_session`] starts a session of a file system with
//! `FUSE_INIT`, [`make_request`] encodes a request, and [`dispatch_request`]
//! sends a request to a session and returns its reply.

use super::CapturingSender;
use crate::Fuse;
use crate::protocol::FUSE_ROOT_ID;
use crate::protocol::fuse_in_header;
use crate::protocol::fuse_out_header;
use crate::request::Request;
use crate::request::tests::FUSE_INIT_REQUEST;
use crate::session::Session;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

/// Returns a session of `fs` that `FUSE_INIT` initialized.
pub(super) fn initialized_session(fs: impl 'static + Fuse + Send + Sync) -> Session {
    let session = Session::new(fs);
    dispatch_request(&session, FUSE_INIT_REQUEST);
    assert!(session.is_initialized());
    session
}

/// Encodes request `unique` of `opcode` on the root node, whose payload is
/// `arg` followed by `data`.
pub(super) fn make_request(
    opcode: u32,
    unique: u64,
    arg: &(impl IntoBytes + Immutable + ?Sized),
    data: &[u8],
) -> Vec<u8> {
    let header = fuse_in_header {
        len: (size_of::<fuse_in_header>() + size_of_val(arg) + data.len())
            .try_into()
            .unwrap(),
        opcode,
        unique,
        nodeid: FUSE_ROOT_ID,
        uid: 0,
        gid: 0,
        pid: 0,
        padding: 0,
    };
    [header.as_bytes(), arg.as_bytes(), data].concat()
}

/// Sends `request` to `session`, and returns the header of the reply and the
/// bytes that follow it.
pub(super) fn dispatch_request(session: &Session, request: &[u8]) -> (fuse_out_header, Vec<u8>) {
    let mut sender = CapturingSender::default();
    session.dispatch(Request::new(request).unwrap(), &mut sender, None);
    let (header, body) = fuse_out_header::read_from_prefix(&sender.data).unwrap();
    (header, body.to_vec())
}
