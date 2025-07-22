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

    /// Handle to a Consomme network endpoint.
    #[derive(MeshPayload)]
    pub struct ConsommeHandle {
        /// The CIDR of the network to use.
        pub cidr: Option<String>,
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
pub mod tap {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::NetEndpointHandleKind;

    /// A handle to a TAP device.
    #[derive(MeshPayload)]
    pub struct TapHandle {
        /// The name of the TAP device.
        ///
        /// FUTURE: change this to a pre-opened `File`.
        pub name: String,
    }

    impl ResourceId<NetEndpointHandleKind> for TapHandle {
        const ID: &'static str = "tap";
    }
}
