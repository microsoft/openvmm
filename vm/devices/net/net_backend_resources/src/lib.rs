// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for network backends (endpoints).
//!
//! TODO: move the resource definitions to separate crates for each endpoint.

#![forbid(unsafe_code)]

pub mod mac_address;

/// Null backend.
pub mod null {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::NetEndpointHandleKind;

    /// Handle to a null network endpoint, which drops sent packets and never
    /// receives packets.
    #[derive(MeshPayload)]
    pub struct NullHandle;

    impl ResourceId<NetEndpointHandleKind> for NullHandle {
        const ID: &'static str = "null";
    }
}

/// Consomme backend.
pub mod consomme {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::NetEndpointHandleKind;

    /// Protocol for host port forwarding.
    #[derive(Clone, Debug, MeshPayload)]
    pub enum HostPortProtocol {
        /// TCP protocol.
        Tcp,
        /// UDP protocol.
        Udp,
    }

    /// An IP address, suitable for serialization via mesh.
    #[derive(Clone, Debug, MeshPayload)]
    pub enum HostIpAddress {
        /// IPv4 address.
        Ipv4(std::net::Ipv4Addr),
        /// IPv6 address.
        Ipv6(std::net::Ipv6Addr),
    }

    impl From<std::net::IpAddr> for HostIpAddress {
        fn from(addr: std::net::IpAddr) -> Self {
            match addr {
                std::net::IpAddr::V4(v4) => HostIpAddress::Ipv4(v4),
                std::net::IpAddr::V6(v6) => HostIpAddress::Ipv6(v6),
            }
        }
    }

    impl From<HostIpAddress> for std::net::IpAddr {
        fn from(addr: HostIpAddress) -> Self {
            match addr {
                HostIpAddress::Ipv4(v4) => std::net::IpAddr::V4(v4),
                HostIpAddress::Ipv6(v6) => std::net::IpAddr::V6(v6),
            }
        }
    }

    /// Configuration for forwarding a host port into the guest.
    #[derive(Clone, Debug, MeshPayload)]
    pub struct HostPortConfig {
        /// The protocol to forward.
        pub protocol: HostPortProtocol,
        /// The host IP address to bind to, or `None` to bind to all interfaces.
        pub host_address: Option<HostIpAddress>,
        /// The host port to listen on.
        pub host_port: u16,
        /// The guest port to forward to.
        pub guest_port: u16,
    }

    /// Handle to a Consomme network endpoint.
    #[derive(MeshPayload)]
    pub struct ConsommeHandle {
        /// The CIDR of the network to use.
        pub cidr: Option<String>,
        /// Ports to forward from the host into the guest.
        pub ports: Vec<HostPortConfig>,
    }

    impl ResourceId<NetEndpointHandleKind> for ConsommeHandle {
        const ID: &'static str = "consomme";
    }
}

/// Windows vmswitch DirectIO backend.
pub mod dio {
    use guid::Guid;
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::NetEndpointHandleKind;

    /// A Hyper-V networking switch port ID.
    #[derive(Copy, Clone, MeshPayload)]
    pub struct SwitchPortId {
        /// The switch ID.
        pub switch: Guid,
        /// The allocated port ID.
        pub port: Guid,
    }

    /// Handle to a DirectIO network endpoint.
    #[derive(MeshPayload)]
    pub struct WindowsDirectIoHandle {
        /// The allocated switch port ID.
        pub switch_port_id: SwitchPortId,
    }

    impl ResourceId<NetEndpointHandleKind> for WindowsDirectIoHandle {
        const ID: &'static str = "dio";
    }
}

/// Linux TAP backend.
#[cfg(target_os = "linux")]
pub mod tap {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::NetEndpointHandleKind;

    /// A handle to a TAP device.
    #[derive(MeshPayload)]
    pub struct TapHandle {
        /// A pre-opened TAP file descriptor, configured with
        /// `IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`.
        pub fd: std::os::fd::OwnedFd,
    }

    impl ResourceId<NetEndpointHandleKind> for TapHandle {
        const ID: &'static str = "tap";
    }
}
