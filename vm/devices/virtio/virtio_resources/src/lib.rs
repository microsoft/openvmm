// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM resource definitions for virtio devices.
//!
//! Device-specific definitions are here to avoid needing to pull in a device
//! implementation crate just to construct the device's config.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::kind::VirtioDeviceHandle;

/// A resource for mapping a virtio device as a PCI device.
#[derive(MeshPayload)]
pub struct VirtioPciDeviceHandle(pub Resource<VirtioDeviceHandle>);

impl ResourceId<PciDeviceHandleKind> for VirtioPciDeviceHandle {
    const ID: &'static str = "virtio";
}

pub mod p9 {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioPlan9Handle {
        pub tag: String,
        pub root_path: String,
        pub debug: bool,
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioPlan9Handle {
        const ID: &'static str = "virtio-9p";
    }
}

pub mod fs {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioFsHandle {
        pub tag: String,
        pub fs: VirtioFsBackend,
    }

    #[derive(MeshPayload)]
    pub enum VirtioFsBackend {
        HostFs {
            root_path: String,
            mount_options: String,
        },
        SectionFs {
            root_path: String,
        },
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioFsHandle {
        const ID: &'static str = "virtiofs";
    }
}

pub mod pmem {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioPmemHandle {
        pub path: String,
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioPmemHandle {
        const ID: &'static str = "virtio-pmem";
    }
}

pub mod rng {
    use mesh::MeshPayload;
    use vm_resource::ResourceId;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioRngHandle;

    impl ResourceId<VirtioDeviceHandle> for VirtioRngHandle {
        const ID: &'static str = "virtio-rng";
    }
}

pub mod blk {
    use mesh::MeshPayload;
    use vm_resource::Resource;
    use vm_resource::ResourceId;
    use vm_resource::kind::DiskHandleKind;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioBlkHandle {
        pub disk: Resource<DiskHandleKind>,
        pub read_only: bool,
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioBlkHandle {
        const ID: &'static str = "virtio-blk";
    }
}

pub mod net {
    use mesh::MeshPayload;
    use net_backend_resources::mac_address::MacAddress;
    use vm_resource::Resource;
    use vm_resource::ResourceId;
    use vm_resource::kind::NetEndpointHandleKind;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioNetHandle {
        pub max_queues: Option<u16>,
        pub mac_address: MacAddress,
        pub endpoint: Resource<NetEndpointHandleKind>,
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioNetHandle {
        const ID: &'static str = "virtio-net";
    }
}

pub mod console {
    use mesh::MeshPayload;
    use vm_resource::Resource;
    use vm_resource::ResourceId;
    use vm_resource::kind::SerialBackendHandle;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioConsoleHandle {
        pub backend: Resource<SerialBackendHandle>,
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioConsoleHandle {
        const ID: &'static str = "virtio-console";
    }
}

#[cfg(unix)]
pub mod vhost_user {
    use mesh::MeshPayload;
    use std::os::fd::OwnedFd;
    use vm_resource::ResourceId;
    use vm_resource::kind::VirtioDeviceHandle;

    /// Handle for a generic vhost-user device backed by an external process.
    ///
    /// The socket must already be connected. The CLI layer connects
    /// to the backend and passes the connected fd here.
    ///
    /// For device types with specific handles (FS, BLK), use those
    /// instead. This handle is for devices identified only by their
    /// numeric virtio device ID.
    #[derive(MeshPayload)]
    pub struct VhostUserGenericHandle {
        /// Connected Unix socket fd to the vhost-user backend.
        pub socket: OwnedFd,
        /// Virtio device ID (e.g., 2 for block, 1 for net).
        pub device_id: u16,
        /// Per-queue sizes. Length determines the queue count.
        /// Required — must be non-empty.
        pub queue_sizes: Vec<u16>,
    }

    impl ResourceId<VirtioDeviceHandle> for VhostUserGenericHandle {
        const ID: &'static str = "vhost-user-generic";
    }

    /// Handle for a vhost-user virtio-fs device.
    ///
    /// The frontend owns the config space (tag + num_request_queues)
    /// and does not negotiate `VHOST_USER_PROTOCOL_F_CONFIG` with the
    /// backend. The tag is specified by the host, matching the
    /// behavior of cloud-hypervisor.
    #[derive(MeshPayload)]
    pub struct VhostUserFsHandle {
        /// Connected Unix socket fd to the vhost-user backend.
        pub socket: OwnedFd,
        /// The mount tag exposed to the guest (max 36 bytes).
        pub tag: String,
        /// Number of request queues (default 1 in resolver).
        pub num_queues: Option<u16>,
        /// Queue size for all queues (default 1024 in resolver).
        pub queue_size: Option<u16>,
    }

    impl ResourceId<VirtioDeviceHandle> for VhostUserFsHandle {
        const ID: &'static str = "vhost-user-fs";
    }

    /// Handle for a vhost-user virtio-blk device.
    #[derive(MeshPayload)]
    pub struct VhostUserBlkHandle {
        /// Connected Unix socket fd to the vhost-user backend.
        pub socket: OwnedFd,
        /// Number of queues (default 1 in resolver).
        pub num_queues: Option<u16>,
        /// Queue size for all queues (default 128 in resolver).
        pub queue_size: Option<u16>,
    }

    impl ResourceId<VirtioDeviceHandle> for VhostUserBlkHandle {
        const ID: &'static str = "vhost-user-blk";
    }
}

pub mod vsock {
    use mesh::MeshPayload;
    use unix_socket::UnixListener;
    use vm_resource::ResourceId;
    use vm_resource::kind::VirtioDeviceHandle;

    #[derive(MeshPayload)]
    pub struct VirtioVsockHandle {
        pub guest_cid: u64,
        pub base_path: String,
        pub listener: UnixListener,
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioVsockHandle {
        const ID: &'static str = "virtio-vsock";
    }
}
