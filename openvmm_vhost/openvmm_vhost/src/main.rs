// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! openvmm_vhost: a vhost-user backend binary that hosts OpenVMM virtio
//! devices over a Unix domain socket.
//!
//! This is primarily a test vehicle at this time, but it could be extended to
//! support all OpenVMM virtio devices for use in production scenarios in the
//! future.
//!
//! This binary is Linux-only (vhost-user requires Unix domain sockets with
//! SCM_RIGHTS fd passing).

#![forbid(unsafe_code)]

fn main() {
    #[cfg(target_os = "linux")]
    if let Err(e) = linux::main() {
        eprintln!("openvmm_vhost failed: {e:#}");
        std::process::exit(1);
    }

    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("openvmm_vhost is only supported on Linux");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::Context as _;
    use clap::Parser;
    use clap::Subcommand;
    use disk_backend_resources::FileDiskHandle;
    use pal_async::DefaultPool;
    use std::path::PathBuf;
    use vhost_user_backend::VhostUserDeviceServer;
    use virtio::resolve::VirtioResolveInput;
    use virtio_resources::blk::VirtioBlkHandle;
    use vm_resource::Resource;
    use vm_resource::ResourceResolver;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    // Register the resolvers needed by this binary.
    vm_resource::register_static_resolvers! {
        virtio_blk::resolver::VirtioBlkResolver,
        disk_file::FileDiskResolver,
    }

    /// openvmm_vhost: vhost-user backend for OpenVMM virtio devices.
    #[derive(Parser)]
    #[command(name = "openvmm_vhost")]
    struct Cli {
        /// Path to the Unix domain socket.
        #[arg(long)]
        socket: PathBuf,

        /// Device type to expose.
        #[command(subcommand)]
        device: DeviceCommand,
    }

    #[derive(Subcommand)]
    enum DeviceCommand {
        /// Expose a virtio-blk device.
        Blk {
            /// Path to the disk image file.
            #[arg(long)]
            disk: PathBuf,

            /// Open the disk as read-only.
            #[arg(long, default_value_t = false)]
            read_only: bool,
        },
    }

    pub fn main() -> anyhow::Result<()> {
        // Default to info-level logging so server lifecycle events are visible.
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();

        let cli = Cli::parse();

        DefaultPool::run_with(|driver: pal_async::DefaultDriver| async move {
            let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
            let resolver = ResourceResolver::new();

            let server = match &cli.device {
                DeviceCommand::Blk { disk, read_only } => {
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(!read_only)
                        .open(disk)
                        .with_context(|| format!("failed to open disk: {}", disk.display()))?;

                    let virtio_handle = VirtioBlkHandle {
                        disk: Resource::new(FileDiskHandle(file)),
                        read_only: *read_only,
                    };

                    let resolved = resolver
                        .resolve(
                            Resource::new(virtio_handle),
                            VirtioResolveInput {
                                driver_source: &driver_source,
                            },
                        )
                        .await
                        .context("failed to resolve virtio-blk device")?;

                    VhostUserDeviceServer::new(resolved.0)
                }
            };

            server
                .run(&driver, &cli.socket)
                .await
                .context("vhost-user server failed")
        })
    }
}
