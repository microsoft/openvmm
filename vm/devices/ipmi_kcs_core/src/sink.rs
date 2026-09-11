// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Egress and time abstractions for the IPMI KCS device.
//!
//! The SEL store is pure protocol state; forwarding entries to a host and
//! reading wall-clock time are injected so the `no_std` core stays free of any
//! host- or platform-specific dependencies. Consumers provide implementations:
//! the OpenVMM device uses a `SystemClock`, the C FFI uses callbacks.

use alloc::sync::Arc;

/// Sink that receives SEL records as the guest adds them.
///
/// The default [`NullSelSink`] is a no-op; hosts that want to collect SEL
/// (e.g. OpenHCL forwarding to host ETW, or Legacy HCL via the FFI callback)
/// provide their own.
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

/// Wall-clock source for SEL timestamps.
///
/// Abstracted so the core needs neither `std::time` nor a platform clock.
pub trait BmcClock: Send + Sync {
    /// Current time as seconds since the Unix epoch (1970-01-01).
    fn now_unix_secs(&self) -> i64;
}

/// Bundle of injectable dependencies for the device.
#[derive(Clone)]
pub struct SelDeps {
    /// Sink for forwarding SEL entries.
    pub sink: Arc<dyn SelSink>,
    /// Wall-clock source.
    pub clock: Arc<dyn BmcClock>,
}

impl SelDeps {
    /// Bundle a sink and clock.
    pub fn new(sink: Arc<dyn SelSink>, clock: Arc<dyn BmcClock>) -> Self {
        Self { sink, clock }
    }
}
