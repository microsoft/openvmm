// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transmit path of a TAP queue.
//!
//! Each `tx_avail` call writes at most one guest packet. When the TAP
//! interface applies backpressure, the queue keeps the packet, and with it
//! the ownership of its descriptors, until `poll_ready` writes it; `tx_poll`
//! then completes it. Packets that the run-scoped egress policy denies are
//! completed without being written.

use crate::TapQueue;
use crate::VirtioNetHdr;
use crate::build_vnet_hdr;
use crate::fixup_ipv4_header_checksum;
use crate::fixup_ipv6_payload_length;
use anyhow::Context as _;
use futures::io::AsyncWrite;
use net_backend::BufferAccess;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxSegment;
use net_backend::linearize;
use net_backend::next_packet;
use net_backend_resources::egress::EgressPolicy;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::io::Write;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use zerocopy::IntoBytes;

/// Transmit state of a [`TapQueue`].
pub(crate) struct TxState {
    pub(crate) pending: Option<PendingTx>,
    pub(crate) ready: VecDeque<TxId>,
    pub(crate) error: Option<std::io::Error>,
    egress_policy: Option<EgressPolicy>,
}

/// A packet accepted from the guest and not yet written to the TAP interface.
pub(crate) struct PendingTx {
    id: TxId,
    header: VirtioNetHdr,
    packet: Vec<u8>,
}

impl TxState {
    pub(crate) fn new(egress_policy: Option<EgressPolicy>) -> Self {
        Self {
            pending: None,
            ready: VecDeque::new(),
            error: None,
            egress_policy,
        }
    }
}

impl TapQueue {
    /// Writes the pending packet, if any, and reports whether TX completions or a
    /// TX error are ready.
    pub(crate) fn poll_tx(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.poll_pending_tx(cx);
        if !self.tx.ready.is_empty() || self.tx.error.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    pub(crate) fn poll_pending_tx(&mut self, cx: &mut Context<'_>) {
        let Some(pending) = self.tx.pending.as_ref() else {
            return;
        };
        let Some(tap) = self.tap.as_mut() else {
            return;
        };
        let header = pending.header.as_bytes();
        let bufs = [
            std::io::IoSlice::new(header),
            std::io::IoSlice::new(&pending.packet),
        ];
        match Pin::new(tap).poll_write_vectored(cx, &bufs) {
            Poll::Ready(Ok(bytes_written))
                if bytes_written == header.len() + pending.packet.len() =>
            {
                let pending = self.tx.pending.take().unwrap();
                self.tx.ready.push_back(pending.id);
            }
            Poll::Ready(Ok(bytes_written)) => {
                self.tx.error = Some(std::io::Error::new(
                    ErrorKind::WriteZero,
                    format!(
                        "partial TAP packet write: wrote {bytes_written} of {} bytes",
                        header.len() + pending.packet.len()
                    ),
                ));
            }
            Poll::Ready(Err(error)) => self.tx.error = Some(error),
            Poll::Pending => {}
        }
    }

    /// Implements `tx_avail`: writes the next packet of `segments`.
    pub(crate) fn transmit(
        &mut self,
        pool: &mut dyn BufferAccess,
        segments: &mut &[TxSegment],
    ) -> anyhow::Result<(bool, usize)> {
        if segments.is_empty() || self.tx.pending.is_some() {
            return Ok((false, 0));
        }

        let (metadata, packet_segments, _) = next_packet(segments);
        let segment_count = packet_segments.len();
        let id = metadata.id;
        let header = build_vnet_hdr(metadata);
        let mut packet = linearize(pool, segments)?;
        if metadata.flags.offload_ip_header_checksum() && metadata.flags.is_ipv4() {
            fixup_ipv4_header_checksum(&mut packet, metadata.l2_len as usize);
        }
        if metadata.flags.offload_tcp_segmentation() && metadata.flags.is_ipv6() {
            fixup_ipv6_payload_length(&mut packet, metadata.l2_len as usize);
        }
        if let Some(policy) = &self.tx.egress_policy
            && policy.authorize_frame(&packet, packet.len()).is_err()
        {
            return Ok((true, segment_count));
        }

        let tap = self.tap.as_mut().context("TAP queue is unavailable")?;
        let header_bytes = header.as_bytes();
        let bufs = [
            std::io::IoSlice::new(header_bytes),
            std::io::IoSlice::new(&packet),
        ];
        match tap.write_vectored(&bufs) {
            Ok(bytes_written) if bytes_written == header_bytes.len() + packet.len() => {
                Ok((true, segment_count))
            }
            Ok(bytes_written) => anyhow::bail!(
                "partial TAP packet write: wrote {bytes_written} of {} bytes",
                header_bytes.len() + packet.len()
            ),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                self.tx.pending = Some(PendingTx { id, header, packet });
                Ok((false, segment_count))
            }
            Err(error) => Err(error).context("failed to write TAP packet"),
        }
    }

    /// Implements `tx_poll`.
    pub(crate) fn poll_tx_done(&mut self, done: &mut [TxId]) -> Result<usize, TxError> {
        if let Some(error) = self.tx.error.take() {
            return Err(TxError::Fatal(error.into()));
        }
        let count = done.len().min(self.tx.ready.len());
        for (destination, id) in done.iter_mut().zip(self.tx.ready.drain(..count)) {
            *destination = id;
        }
        Ok(count)
    }
}
