// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of the replies that a `ReplySender` sends.

use super::*;
use test_with_tracing::test;
use zerocopy::FromBytes;

/// A `ReplySender` that keeps the bytes of the last reply that it sent.
#[derive(Default)]
struct CapturingSender {
    reply: Vec<u8>,
}

impl ReplySender for CapturingSender {
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        self.reply = bufs.iter().flat_map(|buf| buf.iter()).copied().collect();
        Ok(())
    }
}

/// The `len` of a reply's header is the length of the whole reply, header
/// included, which FUSE clients rely on: Linux refuses a reply written to
/// `/dev/fuse` whose `len` differs from the bytes written, and its virtio-fs
/// driver takes the length of a variable-length output, such as the data of
/// a `FUSE_IOCTL` reply, from `len`. A reply of an argument and data counts
/// the data too, whatever its length.
#[test]
fn arg_data_reply_len_includes_the_data() {
    for data_len in [0, 1, 4, 13, 4096] {
        let data: Vec<u8> = (1..=data_len).map(|i| i as u8).collect();
        let mut sender = CapturingSender::default();
        sender
            .send_arg_data(
                7,
                fuse_ioctl_out {
                    result: 42,
                    flags: 0,
                    in_iovs: 0,
                    out_iovs: 0,
                },
                &data,
            )
            .unwrap();

        let (header, rest) = fuse_out_header::read_from_prefix(&sender.reply).unwrap();
        assert_eq!(
            header.len as usize,
            sender.reply.len(),
            "reply with data of length {data_len}"
        );
        assert_eq!(header.error, 0);
        assert_eq!(header.unique, 7);
        let (arg, rest) = fuse_ioctl_out::read_from_prefix(rest).unwrap();
        assert_eq!(arg.result, 42);
        assert_eq!(rest, data);
    }
}

/// Sends an error reply with `error` and returns the error that the reply's
/// header carries, after checking that the reply is that header alone.
fn error_reply(error: i32) -> i32 {
    let mut sender = CapturingSender::default();
    sender.send_error(7, error).unwrap();
    let (header, rest) = fuse_out_header::read_from_prefix(&sender.reply).unwrap();
    assert!(rest.is_empty(), "error {error}");
    assert_eq!(header.len as usize, sender.reply.len(), "error {error}");
    assert_eq!(header.unique, 7, "error {error}");
    header.error
}

/// A reply carries its error code negated, and FUSE clients accept the codes
/// from 0, which is success, through 511: Linux refuses a reply whose error is
/// positive, or -512 (`-ERESTARTSYS`) or less.
#[test]
fn error_reply_carries_a_code_from_0_through_511() {
    for error in [0, 1, 511] {
        assert_eq!(error_reply(error), -error, "error {error}");
    }
}

/// Any other code, such as `i32::MIN`, whose negation overflows, fails the
/// request with `EIO` instead.
#[test]
fn error_reply_replaces_any_other_code_with_eio() {
    for error in [i32::MIN, -1, 512, i32::MAX] {
        assert_eq!(error_reply(error), -lx::EIO, "error {error}");
    }
}
