// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common types shared between DNS resolver implementations.
//!

pub use smoltcp::wire::EthernetAddress;
pub use smoltcp::wire::IpProtocol;
pub use smoltcp::wire::Ipv4Address;

pub use crate::DnsResponse;
pub use crate::DropReason;
