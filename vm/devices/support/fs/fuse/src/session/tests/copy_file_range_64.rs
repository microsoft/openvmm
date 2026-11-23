// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for `FUSE_COPY_FILE_RANGE_64`, the 64-bit variant of
//! `FUSE_COPY_FILE_RANGE` that Linux sends first for each copy since 6.18,
//! and replaces with `FUSE_COPY_FILE_RANGE` only after `ENOSYS`.

use super::harness::dispatch_request;
use super::harness::initialized_session;
use super::harness::make_request;
use super::*;
use test_with_tracing::test;

/// The length of each copy.
const LEN: u32 = 4096;

/// A file system that does not copy, as `VirtioFs` does not.
struct NoCopyFs;

impl Fuse for NoCopyFs {}

/// A file system that copies all the bytes of each `FUSE_COPY_FILE_RANGE`,
/// whose reply holds a `u32` count, the only copy request that it expects.
struct CopyFs;

impl Fuse for CopyFs {
    fn copy_file_range(
        &self,
        request: &Request,
        arg: &fuse_copy_file_range_in,
    ) -> lx::Result<usize> {
        assert_eq!(request.opcode(), FUSE_COPY_FILE_RANGE);
        Ok(arg.len.try_into().unwrap())
    }
}

/// The argument of a copy of `LEN` bytes, which both variants carry.
fn copy_in() -> fuse_copy_file_range_in {
    fuse_copy_file_range_in {
        fh_in: 1,
        off_in: 0,
        nodeid_out: 3,
        fh_out: 2,
        off_out: 0,
        len: LEN.into(),
        flags: 0,
    }
}

/// Sends `request` to `session`, and returns the bytes of its reply, or the
/// error of its error reply.
fn reply(session: &Session, request: &[u8]) -> lx::Result<Vec<u8>> {
    let (header, body) = dispatch_request(session, request);
    if header.error != 0 {
        assert!(body.is_empty());
        return Err(lx::Error::from_lx(-header.error));
    }
    Ok(body)
}

/// A well-formed `FUSE_COPY_FILE_RANGE_64` fails with `ENOSYS`, as
/// `FUSE_COPY_FILE_RANGE` does for a file system that does not copy, which
/// makes Linux fall back from the first to the second, and from the second to
/// its own copy. A short one fails with `EINVAL`, as a short
/// `FUSE_COPY_FILE_RANGE` does.
#[test]
fn copy_64_fails_as_the_legacy_copy_does() {
    let session = initialized_session(NoCopyFs);
    let arg = copy_in();
    let whole = arg.as_bytes();
    let short = &whole[..whole.len() - 8];
    for (payload, error) in [(whole, lx::Error::ENOSYS), (short, lx::Error::EINVAL)] {
        let legacy = reply(
            &session,
            &make_request(FUSE_COPY_FILE_RANGE, 2, payload, &[]),
        );
        assert_eq!(legacy, Err(error), "{} bytes", payload.len());
        let copy_64 = reply(
            &session,
            &make_request(FUSE_COPY_FILE_RANGE_64, 3, payload, &[]),
        );
        assert_eq!(copy_64, legacy, "{} bytes", payload.len());
    }
}

/// `FUSE_COPY_FILE_RANGE_64` fails with `ENOSYS` even for a file system that
/// copies, and the `FUSE_COPY_FILE_RANGE` that Linux sends next copies.
#[test]
fn legacy_copy_copies_after_copy_64_fails() {
    let session = initialized_session(CopyFs);
    let arg = copy_in();
    assert_eq!(
        reply(
            &session,
            &make_request(FUSE_COPY_FILE_RANGE_64, 2, &arg, &[])
        ),
        Err(lx::Error::ENOSYS)
    );
    let out = fuse_write_out {
        size: LEN,
        padding: 0,
    };
    assert_eq!(
        reply(&session, &make_request(FUSE_COPY_FILE_RANGE, 3, &arg, &[])),
        Ok(out.as_bytes().to_vec())
    );
}
