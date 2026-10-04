// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

//! Fuzz harness for VhdxFile open, read, write, flush, and close/abort/drop.

use arbitrary::Arbitrary;
use pal_async::DefaultPool;
use parking_lot::Mutex;
use std::borrow::Borrow;
use vhdx::AsyncFile;
use vhdx::VhdxFile;
use xtask_fuzz::fuzz_target;

/// Cap on backing file growth so input-controlled offsets cannot cause harness OOMs.
const MAX_FILE_SIZE: usize = 64 << 20;

fn too_large() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::OutOfMemory,
        "fuzz file size limit exceeded",
    )
}

struct FuzzFile {
    data: Mutex<Vec<u8>>,
}

impl FuzzFile {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: Mutex::new(data),
        }
    }
}

impl AsyncFile for FuzzFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0u8; len]
    }

    async fn read_into(&self, offset: u64, mut buf: Vec<u8>) -> Result<Vec<u8>, std::io::Error> {
        let data = self.data.lock();
        let off = offset as usize;
        let end = off.saturating_add(buf.len());
        if end > data.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read extends past EOF",
            ));
        }
        buf.copy_from_slice(&data[off..end]);
        Ok(buf)
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> Result<(), std::io::Error> {
        let buf = buf.borrow();
        let mut data = self.data.lock();
        let off = offset as usize;
        let end = off.saturating_add(buf.len());
        if end > MAX_FILE_SIZE {
            return Err(too_large());
        }
        if end > data.len() {
            data.resize(end, 0);
        }
        data[off..end].copy_from_slice(buf.as_ref());
        Ok(())
    }

    async fn flush(&self) -> Result<(), std::io::Error> {
        Ok(())
    }

    async fn file_size(&self) -> Result<u64, std::io::Error> {
        Ok(self.data.lock().len() as u64)
    }

    async fn set_file_size(&self, size: u64) -> Result<(), std::io::Error> {
        let size = usize::try_from(size).map_err(|_| too_large())?;
        if size > MAX_FILE_SIZE {
            return Err(too_large());
        }
        self.data.lock().resize(size, 0);
        Ok(())
    }
}

#[derive(Arbitrary, Debug)]
enum Action {
    Read { offset: u64, len: u32 },
    Write { offset: u64, data: Vec<u8> },
    Flush,
}

#[derive(Arbitrary, Debug)]
enum FinalAction {
    Close,
    Abort,
    Drop,
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    actions: Vec<Action>,
    final_action: FinalAction,
    // Last field: the derive hands all remaining bytes to the file image,
    // keeping file contents independent from the action sequence.
    file_data: Vec<u8>,
}

fn do_fuzz(input: FuzzInput) {
    DefaultPool::run_with(async |driver| {
        let file = FuzzFile::new(input.file_data);

        // Writable open spawns a background log task, so a real executor is required.
        let Ok(vhdx) = VhdxFile::open(file)
            .allow_replay(true)
            .writable(&driver)
            .await
        else {
            return;
        };

        for action in input.actions {
            match action {
                Action::Read { offset, len } => {
                    let mut ranges = Vec::new();
                    let _ = vhdx.resolve_read(offset, len, &mut ranges).await;
                }
                Action::Write { offset, data } => {
                    let mut ranges = Vec::new();
                    if let Ok(guard) = vhdx
                        .resolve_write(offset, data.len() as u32, &mut ranges)
                        .await
                    {
                        let _ = guard.complete().await;
                    }
                }
                Action::Flush => {
                    let _ = vhdx.flush().await;
                }
            }
        }

        match input.final_action {
            FinalAction::Close => {
                let _ = vhdx.close().await;
            }
            FinalAction::Abort => vhdx.abort().await,
            FinalAction::Drop => drop(vhdx),
        }
    });
}

fuzz_target!(|input: FuzzInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input);
});
