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

/// The length of the longest reply, header included: the most that the `u32`
/// `len` of its header can describe.
const MAX_REPLY_LEN: usize = u32::MAX as usize;

/// The length of the longest payload that can follow a reply's header.
const MAX_PAYLOAD_LEN: usize = MAX_REPLY_LEN - size_of::<fuse_out_header>();

/// A `ReplySender` that keeps the header and the length of the last reply
/// that it sent, without copying the reply.
#[derive(Default)]
struct LengthSender {
    sent: Option<(fuse_out_header, usize)>,
}

impl ReplySender for LengthSender {
    fn send(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        let (header, _) = fuse_out_header::read_from_prefix(&bufs[0]).unwrap();
        self.sent = Some((header, bufs.iter().map(|buf| buf.len()).sum()));
        Ok(())
    }
}

/// Sends a reply with `send`, and returns the `len` of its header, after
/// checking that it is the length of the reply, or the kind of the error with
/// which `send` failed, after checking that it sent nothing.
fn reply_len(send: impl FnOnce(&mut LengthSender) -> io::Result<()>) -> Result<u32, io::ErrorKind> {
    let mut sender = LengthSender::default();
    match send(&mut sender) {
        Ok(()) => {
            let (header, len) = sender.sent.expect("a reply is sent");
            assert_eq!(
                header.len as usize, len,
                "the reply's len is not its length"
            );
            assert_eq!((header.error, header.unique), (0, 7));
            Ok(header.len)
        }
        Err(err) => {
            assert!(sender.sent.is_none(), "{err}, but a reply is sent");
            Err(err.kind())
        }
    }
}

/// The `len` of a reply's header counts the whole reply, header included, and
/// is a `u32`. The longest reply that it can describe is sent with that
/// length, while a longer one fails with `InvalidInput` before anything is
/// sent, since its `len` would be wrong. A reply of an argument and data
/// counts both, and a string reply counts the NUL that ends the string. (The
/// payloads are zeroed lazily and never read.)
#[test]
fn reply_longer_than_its_header_can_describe_is_refused() {
    let too_long = Err(io::ErrorKind::InvalidInput);
    let data = vec![0u8; MAX_PAYLOAD_LEN + 1];
    assert_eq!(reply_len(|s| s.send_data(7, &data[1..])), Ok(u32::MAX));
    assert_eq!(reply_len(|s| s.send_data(7, &data)), too_long);

    let arg = || fuse_ioctl_out {
        result: 42,
        flags: 0,
        in_iovs: 0,
        out_iovs: 0,
    };
    let arg_len = size_of::<fuse_ioctl_out>();
    assert_eq!(
        reply_len(|s| s.send_arg_data(7, arg(), &data[arg_len + 1..])),
        Ok(u32::MAX)
    );
    assert_eq!(
        reply_len(|s| s.send_arg_data(7, arg(), &data[arg_len..])),
        too_long
    );
    // Free the data before the strings, so that the test holds one payload at
    // a time.
    drop(data);

    let string = |len| lx::LxString::from_vec(vec![0u8; len]);
    assert_eq!(
        reply_len(|s| s.send_string(7, string(MAX_PAYLOAD_LEN - 1))),
        Ok(u32::MAX)
    );
    assert_eq!(
        reply_len(|s| s.send_string(7, string(MAX_PAYLOAD_LEN))),
        too_long
    );
}
