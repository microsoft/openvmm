// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VMRS dump file generation.

use super::LoadedVm;
use anyhow::Context;
use guestmem::GuestMemory;
use hyperv_dump::GuestMemoryReader;
use hyperv_dump::VmrsWriter;
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;

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

    /// Writes a `.vmrs` crash dump to `path`.
    ///
    /// Used by the automatic dump-on-crash path in the worker's halt handler.
    /// The VPs are already halted at that point, so this dumps directly
    /// without pausing/resuming. The dump is written to a sibling temporary
    /// file and renamed into place so readers never observe a partial dump.
    pub(super) async fn write_crash_dump(&mut self, path: &Path) -> anyhow::Result<()> {
        let mut tmp = OsString::from(path.as_os_str());
        tmp.push(".tmp");
        let tmp_path = Path::new(&tmp);

        let file = File::create(tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        if let Err(err) = self.dump_state_inner(file).await {
            let _ = std::fs::remove_file(tmp_path);
            return Err(err);
        }
        std::fs::rename(tmp_path, path)
            .with_context(|| format!("failed to rename crash dump to {}", path.display()))?;
        Ok(())
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
