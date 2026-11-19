// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the replies to `FUSE_IOCTL`.

use super::harness::dispatch_request;
use super::harness::initialized_session;
use super::harness::make_request;
use super::*;
use test_with_tracing::test;

/// A file system whose `ioctl` expects `input` and returns `result` and
/// `output`.
struct IoctlFs {
    input: Vec<u8>,
    result: i32,
    output: Vec<u8>,
}

impl Fuse for IoctlFs {
    fn ioctl(
        &self,
        _request: &Request,
        _arg: &fuse_ioctl_in,
        data: &[u8],
    ) -> lx::Result<(i32, Vec<u8>)> {
        assert_eq!(data, self.input);
        Ok((self.result, self.output.clone()))
    }
}

/// Sends `fs` a `FUSE_IOCTL` with the input that it expects and room for
/// `out_size` bytes of output. Returns the `fuse_ioctl_out` of the successful
/// reply and the data that follows it.
fn send_ioctl(fs: IoctlFs, out_size: u32) -> (fuse_ioctl_out, Vec<u8>) {
    let arg = fuse_ioctl_in {
        fh: 1,
        flags: 0,
        cmd: 0,
        arg: 0,
        in_size: fs.input.len().try_into().unwrap(),
        out_size,
    };
    let request = make_request(FUSE_IOCTL, 2, &arg, &fs.input);
    let (header, reply) = dispatch_request(&initialized_session(fs), &request);
    assert_eq!(header.error, 0);
    assert_eq!(header.unique, 2);
    let (out, data) = fuse_ioctl_out::read_from_prefix(&reply).unwrap();
    (out, data.to_vec())
}

/// The reply to a `FUSE_IOCTL` carries the file system's result and output,
/// not the request's input, even when the input is larger than the room for
/// the output.
#[test]
fn reply_carries_the_output_instead_of_the_input() {
    let (out, data) = send_ioctl(
        IoctlFs {
            input: vec![1, 2, 3, 4, 5, 6, 7, 8],
            result: 42,
            output: vec![9, 9, 9, 9],
        },
        4,
    );
    assert_eq!(out.result, 42);
    assert_eq!(data, [9, 9, 9, 9]);
}

/// The reply to an ioctl without input carries all of the output that the
/// file system returns, which can be shorter than the room for it.
#[test]
fn reply_carries_output_shorter_than_its_room() {
    let (out, data) = send_ioctl(
        IoctlFs {
            input: Vec::new(),
            result: 3,
            output: vec![5, 6, 7],
        },
        8,
    );
    assert_eq!(out.result, 3);
    assert_eq!(data, [5, 6, 7]);
}
