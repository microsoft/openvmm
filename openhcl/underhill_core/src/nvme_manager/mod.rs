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

/// PCI vendor ID, as it appears in the sysfs `vendor` file (e.g. `0x0100`),
/// for NVMe devices that are incompatible with keepalive.
const KEEPALIVE_INCOMPATIBLE_VENDOR_ID: &str = "0x1414";

/// PCI device ID, as it appears in the sysfs `device` file (e.g. `0x0100`),
/// for NVMe devices that are incompatible with keepalive.
const KEEPALIVE_INCOMPATIBLE_DEVICE_ID: &str = "0xb111";

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
    ) -> Result<nvme_driver::NamespaceHandle, nvme_driver::NamespaceError>;
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

/// Returns whether the given PCI device is compatible with NVMe keepalive.
pub(crate) fn is_nvme_keepalive_compatible(pci_id: &str) -> bool {
    match read_pci_vendor_device_ids(pci_id) {
        Ok((vendor_id, device_id)) => {
            vendor_id != KEEPALIVE_INCOMPATIBLE_VENDOR_ID
                || device_id != KEEPALIVE_INCOMPATIBLE_DEVICE_ID
        }
        Err(err) => {
            tracing::warn!(
                pci_id = %pci_id,
                error = err.as_ref() as &dyn std::error::Error,
                "failed to read PCI vendor/device IDs; treating device as not keepalive-compatible"
            );
            false
        }
    }
}

/// Reads the sysfs `vendor` and `device` files for the given PCI device,
/// returning the trimmed contents (e.g. `"0x0100"`).
///
/// Callers should invoke this once per device and cache the result, since
/// the values do not change for the lifetime of the device.
fn read_pci_vendor_device_ids(pci_id: &str) -> anyhow::Result<(String, String)> {
    let devpath = std::path::Path::new("/sys/bus/pci/devices").join(pci_id);
    let vendor = fs_err::read_to_string(devpath.join("vendor"))?
        .trim_end()
        .to_owned();
    let device = fs_err::read_to_string(devpath.join("device"))?
        .trim_end()
        .to_owned();

    tracing::info!(
        pci_id = %pci_id,
        vendor = %vendor,
        device = %device,
        "read PCI vendor/device IDs"
    );

    Ok((vendor, device))
}
