// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transmit path of a TAP queue.
//!
//! `tx_avail` writes the guest packets to the TAP interface synchronously and
//! completes all of them, including the packets that the interface drops.

use crate::TapQueue;
use crate::prepare_tx_frame;
use net_backend::BufferAccess;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::linearize;
use net_backend::next_packet;
use std::io::ErrorKind;
use std::io::Write;
use zerocopy::IntoBytes;

impl TapQueue {
    /// Implements `tx_avail`: writes the packets of `segments`.
    pub(crate) fn transmit(
        &mut self,
        pool: &mut dyn BufferAccess,
        segments: &mut &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        let n = segments.len();
        // Synchronously send packets received from the guest to host's network.
        if let Some(tap) = self.tap.as_mut() {
            while !segments.is_empty() {
                let (meta, _segs, _rest) = next_packet(segments);
                let mut packet = linearize(pool, segments)?;
                let hdr = prepare_tx_frame(&mut packet, meta);
                let hdr_bytes = hdr.as_bytes();

                let bufs = [
                    std::io::IoSlice::new(hdr_bytes),
                    std::io::IoSlice::new(&packet),
                ];
                match tap.write_vectored(&bufs) {
                    Ok(bytes_written) => {
                        assert_eq!(
                            bytes_written,
                            hdr_bytes.len() + packet.len(),
                            "TAP should never partial write"
                        );
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        // dropped packet: buffer is full

                        // TODO: return partial transmit here. This relies on
                        // remembering this condition and polling for POLLOUT in
                        // poll_ready().
                    }
                    Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                        // dropped packet: interface is not up
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = &err as &dyn std::error::Error,
                            "write to TAP interface failed"
                        );
                    }
                }
            }
        }
        let completed_synchronously = true;
        Ok((completed_synchronously, n))
    }

    /// Implements `tx_poll`.
    pub(crate) fn poll_tx_done(&mut self, _done: &mut [TxId]) -> Result<usize, TxError> {
        // Packets are sent synchronously so there is no no need to check here if
        // sending has been completed.
        Ok(0)
    }
}
