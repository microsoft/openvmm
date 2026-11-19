// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of replies longer than their FUSE header can describe.

use super::harness::dispatch_request;
use super::harness::initialized_session;
use super::harness::make_request;
use super::*;
use test_with_tracing::test;

/// A file system whose `read` returns as many bytes as the request asks for.
struct FullReadFs;

impl Fuse for FullReadFs {
    fn read(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        Ok(vec![0; arg.size as usize])
    }
}

/// Encodes `FUSE_READ` request `unique` for `size` bytes.
fn read_request(unique: u64, size: u32) -> Vec<u8> {
    let arg = fuse_read_in {
        fh: 1,
        offset: 0,
        size,
        read_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    };
    make_request(FUSE_READ, unique, &arg, &[])
}

/// A read of `u32::MAX` bytes that the file system honors in full has no
/// reply that a FUSE header can describe, since the `len` of the header, a
/// `u32`, counts the header too. The session sends no reply for it, rather
/// than aborting or sending one whose `len` is wrong, and serves the requests
/// that follow. (The data is zeroed lazily and never read.)
#[test]
fn read_reply_longer_than_its_header_can_describe_is_not_sent() {
    let session = initialized_session(FullReadFs);

    let mut sender = CapturingSender::default();
    session.dispatch(
        Request::new(read_request(2, u32::MAX).as_slice()).unwrap(),
        &mut sender,
        None,
    );
    assert!(sender.data.is_empty(), "a reply is sent");

    let (header, data) = dispatch_request(&session, &read_request(3, 4096));
    assert_eq!((header.error, header.unique), (0, 3));
    assert_eq!(data.len(), 4096);
}
