// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protocol definitions for the KVP (Key-Value Pair) protocol.

use crate::Version;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// Version 3.0.
pub const KVP_VERSION_3: Version = Version::new(3, 0);
/// Version 4.0.
pub const KVP_VERSION_4: Version = Version::new(4, 0);
/// Version 5.0.
pub const KVP_VERSION_5: Version = Version::new(5, 0);

/// The header for KVP messages.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KvpHeader {
    /// The operation to perform.
    pub operation: KvpOperation,
    /// The pool to use.
    pub pool: KvpPool,
}

open_enum! {
    /// The operation to perform.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum KvpOperation: u8 {
        /// Get a value.
        GET = 0,
        /// Set a value.
        SET = 1,
        /// Delete a value.
        DELETE = 2,
        /// Enumerate values.
        ENUMERATE = 3,
        /// Get IP address information.
        GET_IP_ADDRESS_INFO = 4,
        /// Set IP address information.
        SET_IP_ADDRESS_INFO = 5,
    }
}

open_enum! {
    /// The pool to use for a value.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum KvpPool: u8 {
        #![allow(missing_docs)] // TODO: figure out what the semantics of these actually are.
        EXTERNAL = 0,
        GUEST = 1,
        AUTO = 2,
        AUTO_EXTERNAL = 3,
        // There is an "internal" pool defined in some places, but this is never
        // exchanged between host and guest.
    }
}

/// The maximum key size, in bytes.
pub const MAX_KEY_BYTES: usize = 512;
/// The maximum value size, in bytes.
pub const MAX_VALUE_BYTES: usize = 2048;

/// A value request or response.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct Value {
    /// The type of the value.
    pub value_type: ValueType,
    /// The size of the key, in bytes (including the null terminator).
    pub key_size: u32,
    /// The size of the value, in bytes.
    pub value_size: u32,
    /// The key, as a null-terminated UTF-16 string.
    pub key: [u16; MAX_KEY_BYTES / 2],
    /// The value.
    pub value: [u8; MAX_VALUE_BYTES],
}

open_enum! {
    /// The type of the value.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum ValueType: u32 {
        /// A UTF-16 string.
        STRING = 1,         // REG_SZ
        /// A UTF-16 string, with environment variables expanded.
        EXPAND_STRING = 2,  // REG_EXPAND_SZ
        /// A 32-bit integer.
        DWORD = 4,          // REG_DWORD
        /// A 64-bit integer.
        QWORD = 11,         // REG_QWORD
    }
}

/// A message to get or set a key-value pair.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageGetSet {
    /// The value.
    pub value: Value,
}

/// A message to delete a key-value pair.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageDelete {
    /// The size of the key, in bytes (including the null terminator).
    pub key_size: u32,
    /// The key, as a null-terminated UTF-16 string.
    pub key: [u16; MAX_KEY_BYTES / 2],
}

/// A message to enumerate key-value pairs.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageEnumerate {
    /// The index of the enumeration.
    pub index: u32,
    /// The value.
    pub value: Value,
}

/// A get, set, enumerate, or delete message.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct KvpMessage {
    /// The header.
    pub header: KvpHeader,
    /// Zero padding.
    pub padding: [u8; 2],
    /// The body of the message.
    pub body: [u8; 2576],
}

/// IP address information, in UTF-16 string form.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IpAddressInfo {
    /// The adapter ID, as a null-terminated UTF-16 string.
    pub adapter_id: [u16; 128],
    /// The protocols this message applies to.
    pub address_family: AddressFamily,
    /// Whether DHCP is enabled for the adapter.
    pub dhcp_enabled: u8,
    /// The IP addresses, as a semicolon-delimited, null-terminated UTF-16 string.
    pub ip_address: [u16; 1024],
    /// The subnets, as a semicolon-delimited, null-terminated UTF-16 string.
    pub subnet: [u16; 1024],
    /// The gateways, as a semicolon-delimited, null-terminated UTF-16 string.
    pub gateway: [u16; 512],
    /// The DNS server addresses, as a semicolon-delimited, null-terminated
    /// UTF-16 string.
    pub dns_server_addresses: [u16; 1024],
}

open_enum! {
    /// The address family of a network protocol, for specifying the scope of a request.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum AddressFamily: u8 {
        /// No protocol.
        NONE = 0,
        /// IPv4.
        IPV4 = 1,
        /// IPv6.
        IPV6 = 2,
        /// Both IPv4 and IPv6.
        IPV4V6 = 3,
    }
}

/// IP address information, in binary form.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IpAddressValueBinary {
    /// The number of IPv4 addresses.
    pub ipv4_address_count: u32,
    /// The number of IPv6 addresses.
    pub ipv6_address_count: u32,
    /// The number of IPv4 subnets.
    pub ipv4_gateway_count: u32,
    /// The number of IPv6 subnets.
    pub ipv6_gateway_count: u32,
    /// The number of IPv4 gateways.
    pub ipv4_dns_server_count: u32,
    /// The number of IPv6 gateways.
    pub ipv6_dns_server_count: u32,
    /// The adapter ID, as a null-terminated UTF-16 string.
    pub adapter_id: [u16; 128],
    /// The protocols this message applies to.
    pub address_family: AddressFamily,
    /// Whether DHCP is enabled for the adapter.
    pub dhcp_enabled: u8,
    /// Zero padding.
    pub padding: u16,
    /// The IPv4 addresses.
    pub ipv4_addressese: [IpAddressV4; 64],
    /// The IPv6 addresses.
    pub ipv6_addressese: [IpAddressV6; 64],
    /// The IPv4 subnets.
    pub ipv4_subnets: [IpAddressV4; 64],
    /// The IPv6 subnets.
    pub ipv6_subnets: [IpAddressV6; 64],
    /// The IPv4 gateways.
    pub ipv4_gateways: [IpAddressV4; 5],
    /// The IPv6 gateways.
    pub ipv6_gateways: [IpAddressV6; 5],
    /// The IPv4 DNS servers.
    pub ipv4_dns_servers: [IpAddressV4; 64],
    /// The IPv6 DNS servers.
    pub ipv6_dns_servers: [IpAddressV6; 64],
    /// The IPv4 and IPv6 address origins. This is flattened into a single
    /// array, without gaps between the IPv4 and IPv6 addresses.
    pub ip_address_origins: [IpAddressOrigin; 128],
}

open_enum! {
    /// The origin of an IP address.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum IpAddressOrigin: u32 {
        /// Unknown origin.
        UNKNOWN = 0,
        /// Non-static assignment (probably DHCP).
        OTHER = 1,
        /// Static assignment.
        STATIC = 2,
    }
}

/// An IPv4 address, encoded as four octets in network byte order.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IpAddressV4(pub [u8; 4]);

/// An IPv6 address, encoded as sixteen segments in network byte order.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IpAddressV6(pub [u8; 16]);

/// A message for exchanging IP address information in string format.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageIpAddressInfo {
    /// The message header.
    pub header: KvpHeader,
    /// The IP address information.
    pub value: IpAddressInfo,
}

/// A message for exchanging IP address information in binary format.
#[repr(C)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct MessageIpAddressInfoBinary {
    /// The message header.
    pub header: KvpHeader,
    /// Zero padding.
    pub padding: u16,
    /// The IP address information.
    pub value: IpAddressValueBinary,
}
