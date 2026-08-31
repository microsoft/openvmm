// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress and time abstractions for the IPMI KCS device.
//!
//! In OpenVMM, the SEL is kept purely in-memory and inspected via the `inspect`
//! tree. When this device is hosted inside OpenHCL (the paravisor), the SEL
//! entries written by the guest are diagnostic events that must be forwarded to
//! the host. [`SelSink`] is the injection point for that forwarding so the
//! device core stays free of any host-specific plumbing. [`BmcClock`] abstracts
//! the wall clock so paravisor builds can use the platform time source instead
//! of `std::time`.

use std::sync::Arc;

/// Size of a single SEL record in bytes (IPMI v2.0 Section 32).
const SEL_RECORD_SIZE: usize = 16;

/// Sink that receives SEL records as the guest adds them.
///
/// The default implementation is a no-op; hosts that want to collect SEL
/// (e.g. OpenHCL forwarding to host ETW) provide their own.
pub trait SelSink: Send + Sync {
    /// Called after a SEL entry is committed. `record` is the full 16-byte
    /// SEL record with the assigned record id and timestamp filled in.
    fn log_sel_entry(&self, record_id: u16, record: &[u8]);
}

/// No-op sink used when no host forwarding is configured.
pub struct NullSelSink;

impl SelSink for NullSelSink {
    fn log_sel_entry(&self, _record_id: u16, _record: &[u8]) {}
}

/// Sink that emits each SEL record as a trace event. In OpenHCL the tracing
/// pipeline is forwarded to the host, so this is the forwarding path with no
/// extra plumbing. The record is rendered as hex.
pub struct TracingSelSink;

impl SelSink for TracingSelSink {
    fn log_sel_entry(&self, record_id: u16, record: &[u8]) {
        // 16-byte record -> 32 hex chars; small fixed buffer, no alloc churn.
        let mut hex = [0u8; SEL_RECORD_SIZE * 2];
        for (i, b) in record.iter().take(SEL_RECORD_SIZE).enumerate() {
            const LUT: &[u8; 16] = b"0123456789abcdef";
            hex[i * 2] = LUT[(b >> 4) as usize];
            hex[i * 2 + 1] = LUT[(b & 0xf) as usize];
        }
        let hex = core::str::from_utf8(&hex).unwrap_or("");
        tracelimit::info_ratelimited!(record_id, record = hex, "ipmi sel entry");
    }
}

/// Wall-clock source for SEL timestamps, abstracted for paravisor builds.
pub trait BmcClock: Send + Sync {
    /// Current time as seconds since the Unix epoch (1970-01-01).
    fn now_unix_secs(&self) -> i64;
}

/// Default clock backed by `std::time::SystemTime`.
pub struct SystemClock;

impl BmcClock for SystemClock {
    fn now_unix_secs(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// Bundle of injectable dependencies for the device.
#[derive(Clone)]
pub struct SelDeps {
    /// Sink for forwarding SEL entries.
    pub sink: Arc<dyn SelSink>,
    /// Wall-clock source.
    pub clock: Arc<dyn BmcClock>,
}

impl Default for SelDeps {
    fn default() -> Self {
        Self {
            sink: Arc::new(NullSelSink),
            clock: Arc::new(SystemClock),
        }
    }
}
