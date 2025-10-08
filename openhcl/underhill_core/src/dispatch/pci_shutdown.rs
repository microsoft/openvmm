// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use blocking::unblock;
use cvm_tracing::CVM_ALLOWED;
use fs_err::PathExt;
use futures::future::try_join_all;
use std::os::unix::prelude::*;
use thiserror::Error;
use tracing::Instrument;

#[derive(Debug, Error)]
pub enum ShutdownError {
    #[error("failed operating on sysfs")]
    SysFs(#[source] std::io::Error),
    #[error("failed to unbind {driver} driver for {pci_id}")]
    Unbind {
        driver: String,
        pci_id: String,
        #[source]
        source: std::io::Error,
    },
}

/// Unbinds drivers from all PCI devices so that they reenter quiescent state
/// for the next kernel boot.
///
/// Skips vfio devices since those need to be managed in user mode, so unbinding
/// vfio from them won't help.
pub async fn shutdown_pci_devices() -> Result<(), ShutdownError> {
    let dir = fs_err::read_dir("/sys/bus/pci/devices").map_err(ShutdownError::SysFs)?;
    let ops = try_join_all(dir.map(async |entry| {
        let entry = entry.map_err(ShutdownError::SysFs)?;
        let driver_link = entry.path().join("driver");
        match driver_link.fs_err_read_link() {
            Ok(driver_path) => {
                let driver_name = driver_path.file_name().unwrap().to_string_lossy();
                let pci_id = entry.file_name();
                let pci_id_str = pci_id.to_string_lossy().into_owned();
                if driver_name == "vfio-pci" {
                    tracing::debug!(
                        pci_id = pci_id_str.as_str(),
                        "skipping unbind for vfio device"
                    );
                    return Ok(());
                }
                // Use unblock to run each unbind on a separate thread concurrently.
                unblock(move || fs_err::write(driver_link.join("unbind"), pci_id.as_bytes()))
                    .instrument(tracing::info_span!(
                        "unbind_pci_device",
                        CVM_ALLOWED,
                        driver = driver_name.as_ref(),
                        pci_id = pci_id_str.as_str(),
                    ))
                    .await
                    .map_err(|err| ShutdownError::Unbind {
                        driver: driver_name.into_owned(),
                        pci_id: pci_id_str,
                        source: err,
                    })?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // No driver bound to this device.
            }
            Err(err) => {
                return Err(ShutdownError::SysFs(err));
            }
        }
        Ok(())
    }));

    ops.await?;
    Ok(())
}
