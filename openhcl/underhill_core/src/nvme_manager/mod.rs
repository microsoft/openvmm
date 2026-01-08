// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Multi-threaded NVMe device manager for user-mode VFIO drivers.
//!
//! # Architecture Overview
//!
//! This module implements a multi-threaded actor-based architecture for managing NVMe devices:
//!
//! ```text
//! NvmeManager (coordinator)
//!   ├── NvmeManagerWorker (device registry via mesh RPC)
//!   │   └── Arc<RwLock<HashMap<String, NvmeDriverManager>>> (device lookup)
//!   │
//!   └── Per-device: NvmeDriverManager
//!       └── NvmeDriverManagerWorker (serialized per device via mesh RPC)
//!           └── VfioNvmeDevice (wraps nvme_driver::NvmeDriver<VfioDevice>)
//! ```
//!
//! # Key Objects
//!
//! - **`NvmeManager`**: Main coordinator, creates worker task and provides client interface
//! - **`NvmeManagerWorker`**: Handles device registry, spawns tasks for concurrent operations  
//! - **`NvmeDriverManager`**: Per-device manager with dedicated worker task for serialization
//! - **`NvmeDriverManagerWorker`**: Serializes requests per device, handles driver lifecycle
//! - **`VfioNvmeDevice`**: Implements `NvmeDevice` trait, wraps actual NVMe VFIO driver
//! - **`VfioNvmeDriverSpawner`**: Implements `CreateNvmeDriver` trait for device creation
//! - **`NvmeDiskResolver`**: Resource resolver for converting NVMe configs to resolved disks
//! - **`NvmeDiskConfig`**: Configuration for NVMe disk resources (PCI ID + namespace ID)
//!
//! # Concurrency Model
//!
//! - **Cross-device operations**: Run concurrently via spawned tasks
//! - **Same-device operations**: Serialized through per-device worker tasks
//! - **Device registry**: Protected by `Arc<RwLock<HashMap<String, NvmeDriverManager>>>`
//! - **Shutdown coordination**: `Arc<AtomicBool>` prevents new operations during shutdown
//!
//! # Lock Order
//!
//! 1. `context.devices.read()` - Fast path for existing devices
//! 2. `context.devices.write()` - Only for device creation/removal
//! 3. No nested locks - mesh RPC calls made outside lock scope
//!
//! # Subtle Behaviors
//!
//! - **Idempotent operations**: Multiple `load_driver()` calls are safe (mesh serialization)
//! - **Graceful shutdown**: Mesh RPC handles shutdown races, devices drain before exit
//! - **Error propagation**: Mesh channel errors indicate shutdown
//! - **Save/restore**: Supported when `save_restore_supported=true`, enables nvme_keepalive
//!

use async_trait::async_trait;
use inspect::Inspect;
use std::sync::Arc;
use thiserror::Error;
use vmcore::vm_task::VmTaskDriverSource;

pub mod device;
pub mod manager;
pub mod save_restore;
pub mod save_restore_helpers;

#[derive(Debug, Error)]
#[error("nvme device {pci_id} error")]
pub struct NamespaceError {
    pci_id: String,
    #[source]
    source: NvmeSpawnerError,
}

#[derive(Debug, Error)]
pub enum NvmeSpawnerError {
    #[error("failed to initialize vfio device")]
    Vfio(#[source] anyhow::Error),
    #[error("failed to initialize nvme device")]
    DeviceInitFailed(#[source] anyhow::Error),
    #[error("failed to create dma client for device")]
    DmaClient(#[source] anyhow::Error),
    #[error("failed to get namespace {nsid}")]
    Namespace {
        nsid: u32,
        #[source]
        source: nvme_driver::NamespaceError,
    },
    #[cfg(test)]
    #[error("failed to create mock nvme driver")]
    MockDriverCreationFailed(#[source] anyhow::Error),
}

/// Abstraction over NVMe device drivers that the [`NvmeManager`] manages.
/// This trait provides a uniform interface for different NVMe driver implementations,
/// making it easier to test the [`NvmeManager`] with mock drivers.
#[async_trait]
pub trait NvmeDevice: Inspect + Send + Sync {
    async fn namespace(
        &mut self,
        nsid: u32,
    ) -> Result<Arc<nvme_driver::Namespace>, nvme_driver::NamespaceError>;
    async fn save(&mut self) -> anyhow::Result<nvme_driver::save_restore::NvmeDriverSavedState>;
    async fn shutdown(mut self: Box<Self>);
    fn update_servicing_flags(&mut self, keep_alive: bool);
}

#[async_trait]
pub trait CreateNvmeDriver: Inspect + Send + Sync {
    async fn create_driver(
        &self,
        driver_source: &VmTaskDriverSource,
        pci_id: &str,
        vp_count: u32,
        save_restore_supported: bool,
        saved_state: Option<&nvme_driver::save_restore::NvmeDriverSavedState>,
    ) -> Result<Box<dyn NvmeDevice>, NvmeSpawnerError>;
}
