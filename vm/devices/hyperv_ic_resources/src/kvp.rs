// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resources for the KVP IC.

use mesh::MeshPayload;
use mesh::rpc::FailableRpc;
use vm_resource::ResourceId;
use vm_resource::kind::VmbusDeviceHandleKind;

/// A handle to the KVP IC.
#[derive(MeshPayload)]
pub struct KvpIcHandle {
    /// The receiver for KVP connect requests.
    pub recv: mesh::Receiver<KvpConnectRpc>,
}

impl ResourceId<VmbusDeviceHandleKind> for KvpIcHandle {
    const ID: &'static str = "kvp_ic";
}

/// A connect request.
#[derive(MeshPayload)]
pub enum KvpConnectRpc {
    /// Waits for the guest to connect, returning a sender for issuing KVP
    /// requests and a receiver for determining when the KVP channel is no
    /// longer available.
    WaitForGuest(FailableRpc<(), (mesh::Sender<KvpRpc>, mesh::OneshotReceiver<()>)>),
}

/// A KVP request.
#[derive(MeshPayload)]
pub enum KvpRpc {
    /// Sets a key/value pair in the KVP store.
    Set(FailableRpc<SetParams, ()>),
    /// Deletes a key/value pair from the KVP store.
    Delete(FailableRpc<DeleteParams, ()>),
    /// Enumerates the key/value pairs in the KVP store.
    Enumerate(FailableRpc<EnumerateParams, Option<KeyValue>>),
    /// Gets IP address information for a given adapter.
    GetIpInfo(FailableRpc<GetIpInfoParams, IpInfo>),
    /// Sets IP address information for a given adapter.
    SetIpInfo(FailableRpc<SetIpInfoParams, ()>),
}

/// Parameters for setting a key/value pair in the KVP store.
#[derive(MeshPayload, Clone, Debug)]
pub struct SetParams {
    /// The pool to use.
    pub pool: KvpPool,
    /// The key to set.
    pub key: String,
    /// The value.
    pub value: Value,
}

/// Parameters for deleting a key/value pair in the KVP store.
#[derive(MeshPayload, Clone, Debug)]
pub struct DeleteParams {
    /// The pool to use.
    pub pool: KvpPool,
    /// The key to delete.
    pub key: String,
}

/// Parameters for enumerating key/value pairs in the KVP store.
#[derive(MeshPayload, Clone, Debug)]
pub struct EnumerateParams {
    /// The pool to use.
    pub pool: KvpPool,
    /// The key to start enumerating from.
    pub index: u32,
}

/// Parameters for getting IP address information for a given adapter.
#[derive(MeshPayload, Clone, Debug)]
pub struct GetIpInfoParams {
    /// The MAC address to get the IP info for.
    pub adapter_id: String,
}

/// The result of getting IP address information for a given adapter.
#[derive(MeshPayload, Clone, Debug)]
pub struct IpInfo {
    /// Whether ipv4 is enabled.
    pub ipv4: bool,
    /// Whether ipv6 is enabled.
    pub ipv6: bool,
    /// Whether DHCP is enabled.
    pub dhcp_enabled: bool,
    /// The set of bound IPv4 addresses.
    pub ipv4_addresses: Vec<Ipv4AddressInfo>,
    /// The set of bound IPv6 addresses.
    pub ipv6_addresses: Vec<Ipv6AddressInfo>,
    /// The set of IPv4 gateways.
    pub ipv4_gateways: Vec<std::net::Ipv4Addr>,
    /// The set of IPv6 gateways.
    pub ipv6_gateways: Vec<std::net::Ipv6Addr>,
    /// The set of IPv4 DNS servers.
    pub ipv4_dns_servers: Vec<std::net::Ipv4Addr>,
    /// The set of IPv6 DNS servers.
    pub ipv6_dns_servers: Vec<std::net::Ipv6Addr>,
}

/// Parameters for setting IP address information for a given adapter.
#[derive(MeshPayload, Clone, Debug)]
pub struct SetIpInfoParams {
    /// The vmbus device ID of the adapter.
    pub adapter_id: String,
    /// The IP information to set.
    ///
    /// The IP origin information is ignored.
    pub info: IpInfo,
}

/// Information about an IPv4 address.
#[derive(MeshPayload, Clone, Debug)]
pub struct Ipv4AddressInfo {
    /// The IPv4 address.
    pub address: std::net::Ipv4Addr,
    /// The subnet mask.
    pub subnet: std::net::Ipv4Addr,
    /// The origin of the address.
    pub origin: AddressOrigin,
}

/// Information about an IPv6 address.
#[derive(MeshPayload, Clone, Debug)]
pub struct Ipv6AddressInfo {
    /// The IPv6 address.
    pub address: std::net::Ipv6Addr,
    /// The subnet prefix length.
    pub subnet: u32,
    /// The origin of the address.
    pub origin: AddressOrigin,
}

/// The origin of an address.
#[derive(MeshPayload, Clone, Debug)]
pub enum AddressOrigin {
    /// The origin is unknown.
    Unknown,
    /// The address was assigned statically.
    Static,
    /// The address was not assigned statically.
    Other,
}

/// A key/value pair.
#[derive(MeshPayload, Clone, Debug)]
pub struct KeyValue {
    /// The key.
    pub key: String,
    /// The value.
    pub value: Value,
}

/// A value.
#[derive(MeshPayload, Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A string value.
    String(String),
    /// A 32-bit integer value.
    U32(u32),
    /// A 64-bit integer value.
    U64(u64),
}

/// The pool to use for KVP operations.
#[derive(Copy, Clone, Debug, MeshPayload)]
pub enum KvpPool {
    /// The guest pool.
    Guest,
    /// The external pool.
    External,
    /// The automatic pool.
    Auto,
    /// The automatic external pool.
    AutoExternal,
}
