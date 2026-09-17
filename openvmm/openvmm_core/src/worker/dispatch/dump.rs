// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMRS dump file generation.

use super::LoadedVm;
use anyhow::Context;
use guestmem::GuestMemory;
use hyperv_dump::GuestMemoryReader;
use hyperv_dump::VmrsWriter;
use std::fs::File;

impl LoadedVm {
    /// Dumps VM state (VP registers + memory) to a `.vmrs` file.
    ///
    /// Pauses the VM if running, collects VP state and streams memory,
    /// then restores the prior running state.
    pub(super) async fn dump_state(&mut self, file: File) -> anyhow::Result<()> {
        let was_running = self.pause().await;
        let result = self.dump_state_inner(file).await;
        if was_running {
            self.resume().await;
        }
        result
    }

    async fn dump_state_inner(&mut self, file: File) -> anyhow::Result<()> {
        tracing::info!("dumping VM state to VMRS");

        // Build the partition state blob (VP registers as HV chunks).
        let partition_state_blob = self
            .inner
            .partition_unit
            .build_dump_partition_state()
            .await
            .context("failed to build partition state")?;

        // Write the VMRS file. BufWriter reduces syscalls for the many small
        // key table / header writes interspersed with large memory blocks.
        let file = std::io::BufWriter::with_capacity(256 * 1024, file);
        let mut vmrs = VmrsWriter::new(file).context("failed to initialize VMRS writer")?;

        // Add memory ranges from the VM topology.
        for ram_range in self.inner.mem_layout.ram() {
            vmrs.add_memory_range(ram_range.range);
        }

        // Stream guest memory to disk.
        let gm = self.inner.gm.clone();
        struct GmReader(GuestMemory);
        impl GuestMemoryReader for GmReader {
            fn read_gpa(&mut self, gpa: u64, buf: &mut [u8]) -> std::io::Result<()> {
                self.0.read_at(gpa, buf).map_err(std::io::Error::other)
            }
        }
        let mut reader = GmReader(gm);
        vmrs.finish(&partition_state_blob, &mut reader)
            .context("failed to write VMRS file")?;

        tracing::info!("VMRS dump complete");
        Ok(())
    }
}
