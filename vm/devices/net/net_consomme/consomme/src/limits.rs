// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Limits on the number of simultaneously active guest flows.
//!
//! Each protocol rejects a new guest flow once it has this many active flows,
//! before creating any host socket for it.

/// Default maximum number of simultaneously active guest TCP flows.
///
/// The limit is checked before creating a host TCP socket or allocating
/// per-flow TCP buffers.
pub const DEFAULT_MAX_ACTIVE_TCP_FLOWS: usize = 128;
/// Default maximum number of simultaneously active guest UDP flows.
///
/// The limit is checked before binding a host UDP socket.
pub const DEFAULT_MAX_ACTIVE_UDP_FLOWS: usize = 256;
/// Default maximum number of simultaneously active guest ICMP source flows.
///
/// The limit is checked before opening a host ICMP socket.
pub const DEFAULT_MAX_ACTIVE_ICMP_FLOWS: usize = 16;
