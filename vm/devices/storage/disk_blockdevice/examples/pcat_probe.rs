// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Read-only IDE-style I/O probe for disposable Linux test storage.

#[cfg(target_os = "linux")]
mod linux {
    use disk_backend::DiskIo;
    use disk_blockdevice::BlockDevice;
    use disk_blockdevice::open_file_for_block;
    use futures::executor::block_on;
    use guestmem::AlignedHeapMemory;
    use guestmem::GuestMemory;
    use guestmem::ranges::PagedRange;
    use pal_uring::IoUringPool;
    use scsi_buffers::RequestBuffers;
    use std::collections::hash_map::DefaultHasher;
    use std::error::Error;
    use std::hash::Hash;
    use std::hash::Hasher;
    use std::path::Path;
    use std::sync::Arc;

    async fn nonzero_sectors(
        disk: &BlockDevice,
        memory: &GuestMemory,
    ) -> Result<Vec<u64>, Box<dyn Error>> {
        let pages: Vec<u64> = (0..16).collect();
        let limit = disk.sector_count().min(131072);
        let mut selected = Vec::new();
        let mut start = 0;
        while start < limit {
            let sectors = (limit - start).min(128);
            let len = sectors as usize * 512;
            let range = PagedRange::new(0, len, &pages).ok_or("invalid scan buffer range")?;
            disk.read_vectored(&RequestBuffers::new(memory, range, true), start)
                .await?;
            let mut data = vec![0; len];
            memory.read_at(0, &mut data)?;
            for index in 0..sectors as usize {
                let sector = start + index as u64;
                if sector <= disk.sector_count() - 128
                    && data[index * 512..(index + 1) * 512]
                        .iter()
                        .any(|&byte| byte != 0)
                {
                    selected.push(sector);
                }
            }
            start += sectors;
        }
        if selected.is_empty() {
            return Err("no nonzero test sectors found in the first 64 MiB".into());
        }
        Ok(selected)
    }

    async fn exercise(
        disk: &BlockDevice,
        memory: &GuestMemory,
        selected: &[u64],
        iterations: u64,
    ) -> Result<(u64, u64), Box<dyn Error>> {
        let pages: Vec<u64> = (0..16).collect();
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut digest = DefaultHasher::new();
        let mut nonzero = 0;
        for i in 0..iterations {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let sectors = [1, 8, 128][(i % 3) as usize];
            let sector = selected[(seed % selected.len() as u64) as usize];
            let len = sectors as usize * 512;
            let range = PagedRange::new(0, len, &pages).ok_or("invalid probe buffer range")?;
            let buffers = RequestBuffers::new(memory, range, true);
            disk.read_vectored(&buffers, sector).await?;
            let mut data = vec![0; len];
            memory.read_at(0, &mut data)?;
            nonzero += u64::from(data.iter().any(|&byte| byte != 0));
            data.hash(&mut digest);
        }
        Ok((digest.finish(), nonzero))
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let mut args = std::env::args().skip(1);
        let path = args.next().ok_or("usage: pcat_probe DEVICE [ITERATIONS]")?;
        let iterations = args.next().map(|n| n.parse()).transpose()?.unwrap_or(3000);
        if iterations == 0 || args.next().is_some() {
            return Err("expected a positive iteration count and no extra arguments".into());
        }

        let pool = IoUringPool::new("pcat-probe", 16)?;
        let client = pool.client().clone();
        std::thread::spawn(move || pool.run());
        let memory = GuestMemory::new("ata_buffer", Arc::new(AlignedHeapMemory::new(65536)));
        let mut control = None;
        let mut selected = None;
        for verify in [false, true] {
            let file = open_file_for_block(Path::new(&path), true, true)?;
            let disk = block_on(BlockDevice::new(
                file,
                true,
                client.initiator().clone(),
                None,
                None,
                false,
                verify,
            ))?;
            if disk.sector_size() != 512 || disk.sector_count() <= 128 {
                return Err("probe requires a 512-byte-sector device larger than 64 KiB".into());
            }
            if selected.is_none() {
                selected = Some(block_on(nonzero_sectors(&disk, &memory))?);
            }
            let selected = selected.as_deref().ok_or("missing selected test sectors")?;
            let result = block_on(exercise(&disk, &memory, selected, iterations))?;
            println!(
                "RESULT verify={verify} reads={iterations} nonzero={} digest={:#x}",
                result.1, result.0
            );
            if result.1 != iterations {
                return Err("probe did not read nonzero data on every iteration".into());
            }
            if let Some(control) = control {
                if result != control {
                    return Err("verified and control reads produced different data".into());
                }
            } else {
                control = Some(result);
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("pcat_probe requires Linux".into())
}
