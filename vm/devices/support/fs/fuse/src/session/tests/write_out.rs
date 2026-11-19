// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the replies to `FUSE_WRITE` and `FUSE_COPY_FILE_RANGE`, whose
//! `fuse_write_out` carries the file system's byte count as a `u32`.

use super::harness::dispatch_request;
use super::harness::initialized_session;
use super::harness::make_request;
use super::*;
use test_with_tracing::test;

/// The smallest byte count that a `u32` cannot hold.
const BEYOND_U32: usize = u32::MAX as usize + 1;

/// The data of each write.
const DATA: &[u8] = b"hello";

/// A file system that reports each write as `written` bytes, and copies all
/// the bytes of each copy.
#[derive(Default)]
struct CountFs {
    written: usize,
}

impl Fuse for CountFs {
    fn write(&self, _request: &Request, _arg: &fuse_write_in, _data: &[u8]) -> lx::Result<usize> {
        Ok(self.written)
    }

    fn copy_file_range(
        &self,
        _request: &Request,
        arg: &fuse_copy_file_range_in,
    ) -> lx::Result<usize> {
        Ok(arg.len.try_into().unwrap())
    }
}

/// Encodes `FUSE_WRITE` request `unique` of `DATA`.
fn write_request(unique: u64) -> Vec<u8> {
    let arg = fuse_write_in {
        fh: 1,
        offset: 0,
        size: DATA.len().try_into().unwrap(),
        write_flags: 0,
        lock_owner: 0,
        flags: 0,
        padding: 0,
    };
    make_request(FUSE_WRITE, unique, &arg, DATA)
}

/// Encodes `FUSE_COPY_FILE_RANGE` request `unique` of `len` bytes, a length
/// that Linux passed on above `u32::MAX` until 6.17.
fn copy_request(unique: u64, len: u64) -> Vec<u8> {
    let arg = fuse_copy_file_range_in {
        fh_in: 1,
        off_in: 0,
        nodeid_out: 3,
        fh_out: 2,
        off_out: 0,
        len,
        flags: 0,
    };
    make_request(FUSE_COPY_FILE_RANGE, unique, &arg, &[])
}

/// Sends `request` to `session`, and returns the `size` of its
/// `fuse_write_out` reply, or the error of its error reply.
fn reply(session: &Session, request: &[u8]) -> lx::Result<u32> {
    let (header, body) = dispatch_request(session, request);
    let (request_header, _) = fuse_in_header::read_from_prefix(request).unwrap();
    assert_eq!(header.unique, request_header.unique);
    if header.error != 0 {
        assert!(body.is_empty());
        return Err(lx::Error::from_lx(-header.error));
    }
    let out = fuse_write_out::read_from_bytes(&body).unwrap();
    assert_eq!(out.padding, 0);
    Ok(out.size)
}

/// A write replies with the count that the file system wrote.
#[test]
fn write_replies_with_the_count_written() {
    let session = initialized_session(CountFs {
        written: DATA.len(),
    });
    assert_eq!(
        reply(&session, &write_request(2)),
        Ok(DATA.len().try_into().unwrap())
    );
}

/// A write count beyond `u32::MAX`, which the reply cannot hold, fails the
/// write with `EIO`.
#[test]
fn write_count_beyond_u32_fails_with_eio() {
    let session = initialized_session(CountFs {
        written: BEYOND_U32,
    });
    assert_eq!(reply(&session, &write_request(2)), Err(lx::Error::EIO));
}

/// A copy replies with the count that the file system copied, up to
/// `u32::MAX`.
#[test]
fn copy_replies_with_counts_up_to_u32_max() {
    let session = initialized_session(CountFs::default());
    for (unique, len) in (2..).zip([4096, u32::MAX]) {
        assert_eq!(reply(&session, &copy_request(unique, len.into())), Ok(len));
    }
}

/// A copy count beyond `u32::MAX`, which the reply cannot hold, fails the
/// copy with `EIO`, and the session goes on to answer the next copy.
#[test]
fn copy_count_beyond_u32_fails_with_eio() {
    let session = initialized_session(CountFs::default());
    assert_eq!(
        reply(&session, &copy_request(2, BEYOND_U32 as u64)),
        Err(lx::Error::EIO)
    );
    assert_eq!(reply(&session, &copy_request(3, 4096)), Ok(4096));
}
