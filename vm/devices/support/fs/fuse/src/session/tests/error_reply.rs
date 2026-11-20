// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the error replies to failed requests.

use super::harness::dispatch_request;
use super::harness::initialized_session;
use super::*;
use test_with_tracing::test;

/// A file system whose `get_attr` fails with `i32::MIN`, a code that no reply
/// can carry.
struct OutOfRangeErrorFs;

impl Fuse for OutOfRangeErrorFs {
    fn get_attr(&self, _request: &Request, _flags: u32, _fh: u64) -> lx::Result<fuse_attr_out> {
        Err(lx::Error::from_lx(i32::MIN))
    }
}

/// A request that fails with a code that a reply cannot carry gets an `EIO`
/// reply.
#[test]
fn failure_with_an_out_of_range_code_replies_eio() {
    let session = initialized_session(OutOfRangeErrorFs);
    let (header, reply) = dispatch_request(&session, FUSE_GETATTR_REQUEST);
    assert_eq!(header.error, -lx::EIO);
    assert_eq!(header.unique, 2);
    assert!(reply.is_empty());
}
