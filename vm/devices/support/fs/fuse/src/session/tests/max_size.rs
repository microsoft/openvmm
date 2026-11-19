// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the replies whose size the request bounds: those to `FUSE_READ`,
//! `FUSE_READDIR`, `FUSE_READDIRPLUS`, `FUSE_GETXATTR`, and `FUSE_LISTXATTR`.

use super::harness::dispatch_request;
use super::harness::initialized_session;
use super::harness::make_request;
use super::*;
use test_with_tracing::test;

/// The operations whose request bounds the size of the reply.
const SIZED_OPCODES: [u32; 5] = [
    FUSE_READ,
    FUSE_READDIR,
    FUSE_READDIRPLUS,
    FUSE_GETXATTR,
    FUSE_LISTXATTR,
];

/// The size that each request allows its reply.
const SIZE: u32 = 4096;

/// The byte that fills each reply.
const FILL: u8 = 0x5a;

/// A file system whose replies hold `extra` bytes more than the request
/// allows.
struct SizedReplyFs {
    extra: usize,
}

impl SizedReplyFs {
    fn reply(&self, size: u32) -> Vec<u8> {
        vec![FILL; size as usize + self.extra]
    }
}

impl Fuse for SizedReplyFs {
    fn read(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        Ok(self.reply(arg.size))
    }

    fn read_dir(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        Ok(self.reply(arg.size))
    }

    fn read_dir_plus(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        Ok(self.reply(arg.size))
    }

    fn get_xattr(&self, _request: &Request, _name: &lx::LxStr, size: u32) -> lx::Result<Vec<u8>> {
        Ok(self.reply(size))
    }

    fn list_xattr(&self, _request: &Request, size: u32) -> lx::Result<Vec<u8>> {
        Ok(self.reply(size))
    }
}

/// Encodes request `unique` of `opcode`, which allows a reply of `size`
/// bytes.
fn sized_request(opcode: u32, unique: u64, size: u32) -> Vec<u8> {
    let xattr = fuse_getxattr_in { size, padding: 0 };
    match opcode {
        FUSE_GETXATTR => make_request(opcode, unique, &xattr, b"user.test\0"),
        FUSE_LISTXATTR => make_request(opcode, unique, &xattr, &[]),
        _ => {
            let arg = fuse_read_in {
                fh: 1,
                offset: 0,
                size,
                read_flags: 0,
                lock_owner: 0,
                flags: 0,
                padding: 0,
            };
            make_request(opcode, unique, &arg, &[])
        }
    }
}

/// A file system's reply that is longer than the request allows fails the
/// request with `EIO`, as the Linux FUSE driver fails such a reply, and sends
/// none of its data. The session goes on to serve the next request.
#[test]
fn reply_longer_than_the_request_allows_fails_with_eio() {
    let session = initialized_session(SizedReplyFs { extra: 1 });
    for (unique, opcode) in (2..).zip(SIZED_OPCODES) {
        let (header, data) = dispatch_request(&session, &sized_request(opcode, unique, SIZE));
        assert_eq!(header.error, -lx::EIO, "opcode {opcode}");
        assert_eq!(header.unique, unique, "opcode {opcode}");
        assert!(data.is_empty(), "opcode {opcode}");
    }
}

/// A file system's reply of exactly the size that the request allows is sent
/// whole.
#[test]
fn reply_of_the_size_that_the_request_allows_is_sent() {
    let session = initialized_session(SizedReplyFs { extra: 0 });
    for (unique, opcode) in (2..).zip(SIZED_OPCODES) {
        let (header, data) = dispatch_request(&session, &sized_request(opcode, unique, SIZE));
        assert_eq!(header.error, 0, "opcode {opcode}");
        assert_eq!(header.unique, unique, "opcode {opcode}");
        assert_eq!(data, [FILL; SIZE as usize], "opcode {opcode}");
    }
}
