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
        /// Expose multiple host folders behind a single device, each as a named
        /// child of a synthetic root. Lets one virtio-fs device (one tag, one
        /// PCI/MMIO footprint) serve many shares.
        Aggregate {
            children: Vec<VirtioFsAggregateChild>,
        },
    }

    /// A single host folder exposed as a named child of a [`VirtioFsBackend::Aggregate`].
    #[derive(MeshPayload)]
    pub struct VirtioFsAggregateChild {
        /// Name of this child's directory under the aggregate's synthetic root;
        /// the guest bind-mounts `<aggregate-mount>/<name>` onto the user's
        /// target path.
        pub name: String,
        pub root_path: String,
        pub mount_options: String,
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
        pub serial: Option<String>,
        /// Force owned block staging, even when guest memory supports kernel locks.
        /// Kernel-denied guest memory requires staging regardless of this flag.
        pub bounce_io: bool,
        /// Reviewed alternative selected when staging is required or forced.
        pub bounce_disk: Option<Resource<DiskHandleKind>>,
    }

    impl ResourceId<VirtioDeviceHandle> for VirtioBlkHandle {
        const ID: &'static str = "virtio-blk";
    }

    /// Backends reviewed for virtio-blk bounce I/O.
    ///
    /// File handles must refer to regular buffered files. On Linux, neither
    /// `O_DIRECT` nor `O_APPEND` is supported. Other disk resources, including
    /// arbitrary layered disks and host block devices, are not supported.
    #[derive(MeshPayload)]
    pub enum VirtioBlkBounceDiskHandle {
        File(std::fs::File),
        Ram { len: u64 },
    }

    impl ResourceId<DiskHandleKind> for VirtioBlkBounceDiskHandle {
        const ID: &'static str = "virtio-blk-bounce-disk";
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use test_with_tracing::test;

        #[derive(MeshPayload)]
        struct LegacyVirtioBlkHandle {
            disk: Resource<DiskHandleKind>,
            read_only: bool,
            serial: Option<String>,
        }

        #[test]
        fn missing_bounce_field_defaults_off() {
            let legacy = LegacyVirtioBlkHandle {
                disk: Resource::new(VirtioBlkBounceDiskHandle::Ram { len: 512 }),
                read_only: true,
                serial: Some("legacy".into()),
            };
            let handle: VirtioBlkHandle = mesh::OwnedMessage::new(legacy).parse().unwrap();
            assert!(!handle.bounce_io);
            assert!(handle.bounce_disk.is_none());
            assert!(handle.read_only);
            assert_eq!(handle.serial.as_deref(), Some("legacy"));
        }

        #[test]
        fn bounce_field_round_trips() {
            for bounce_io in [false, true] {
                let handle = VirtioBlkHandle {
                    disk: Resource::new(VirtioBlkBounceDiskHandle::Ram { len: 512 }),
                    read_only: false,
                    serial: None,
                    bounce_io,
                    bounce_disk: None,
                };
                let handle: VirtioBlkHandle = mesh::OwnedMessage::new(handle).parse().unwrap();
                assert_eq!(handle.bounce_io, bounce_io);
            }
        }

        #[test]
        fn automatic_backing_preserves_frontend_identity() {
            let handle = VirtioBlkHandle {
                disk: Resource::new(VirtioBlkBounceDiskHandle::Ram { len: 512 }),
                read_only: true,
                serial: Some("automatic".into()),
                bounce_io: false,
                bounce_disk: Some(Resource::new(VirtioBlkBounceDiskHandle::Ram { len: 512 })),
            };
            let handle: VirtioBlkHandle = mesh::OwnedMessage::new(handle).parse().unwrap();
            assert!(!handle.bounce_io);
            assert!(handle.read_only);
            assert_eq!(handle.serial.as_deref(), Some("automatic"));
            assert_eq!(
                handle.bounce_disk.as_ref().unwrap().id(),
                VirtioBlkBounceDiskHandle::ID
            );
            let resource = Resource::<VirtioDeviceHandle>::new(handle);
            assert_eq!(resource.id(), "virtio-blk");
            assert_eq!(format!("{}-4096", resource.id()), "virtio-blk-4096");
            assert_eq!(format!("{}-pci", resource.id()), "virtio-blk-pci");
        }
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
    #[cfg(target_os = "linux")]
    use std::os::fd::OwnedFd;
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

    /// A virtio-vsock device backed by the Linux kernel's `vhost_vsock`
    /// implementation.
    #[cfg(target_os = "linux")]
    #[derive(MeshPayload)]
    pub struct VirtioVsockVhostHandle {
        /// A pre-opened `/dev/vhost-vsock` file descriptor.
        ///
        /// The device resolver takes ownership and configures the vhost owner
        /// and guest CID.
        pub vhost: OwnedFd,
        /// The CID used to address the guest from the host's `AF_VSOCK`
        /// namespace.
        pub guest_cid: u32,
    }

    #[cfg(target_os = "linux")]
    impl ResourceId<VirtioDeviceHandle> for VirtioVsockVhostHandle {
        const ID: &'static str = "virtio-vsock-vhost";
    }
}
