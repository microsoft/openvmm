// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest crash dump helpers.

use anyhow::Context;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use get_resources::crash::GuestCrashDeviceHandle;
use mesh::channel;
use mesh::rpc::FailableRpc;
use mesh::OneshotReceiver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use unicycle::FuturesUnordered;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// Spawns a crash dump handling task and returns a resource to instantiate a
/// guest crash device.
pub fn spawn_dump_handler(
    spawner: impl Spawn,
    dump_path: PathBuf,
    max_file_size: Option<u64>,
) -> (Resource<VmbusDeviceHandleKind>, Task<()>) {
    const DEFAULT_MAX_DUMP_SIZE: u64 = 256 * 1024 * 1024;

    let (send, recv) = channel::<FailableRpc<_, _>>();
    let task = spawner.spawn("crash_dumps", async move {
        handle_dump_requests(&dump_path, recv).await
    });
    let config = GuestCrashDeviceHandle {
        request_dump: send,
        max_dump_size: max_file_size.unwrap_or(DEFAULT_MAX_DUMP_SIZE),
    };
    (config.into_resource(), task)
}

/// Handles dump requests from the crash dump device by opening files in the
/// provided path.
pub async fn handle_dump_requests(
    dump_path: &Path,
    mut recv: mesh::Receiver<
        mesh::rpc::Rpc<OneshotReceiver<()>, Result<File, mesh::error::RemoteError>>,
    >,
) {
    let mut tasks = FuturesUnordered::new();
    while let Some(rpc) = ((&mut recv).map(Some), (&mut tasks).map(|()| None))
        .merge()
        .next()
        .await
    {
        let Some(rpc) = rpc else { continue };
        rpc.handle_failable_sync(|done| {
            let tempfile = tempfile::Builder::new()
                .prefix("underhill.")
                .suffix(".core")
                .tempfile_in(dump_path)
                .context("failed to create file")?;

            let file = tempfile
                .as_file()
                .try_clone()
                .context("failed to clone file")?;

            tracing::info!(path = %tempfile.path().display(), "writing VTL2 crash dump");
            tasks.push(wait_for_dump(done, tempfile));
            anyhow::Ok(file)
        })
    }
}

async fn wait_for_dump(done: OneshotReceiver<()>, tempfile: tempfile::NamedTempFile) {
    if let Ok(()) = done.await {
        match tempfile.keep() {
            Ok((_, path)) => {
                tracing::info!(
                    path = %path.display(),
                    "wrote VTL2 crash dump"
                );
            }
            Err(err) => {
                tracing::error!(
                    path = %err.file.path().display(),
                    "failed to persist VTL2 dump file"
                );
            }
        }
    } else {
        tracing::info!(
            path = %tempfile.path().display(),
            "VTL2 crash dump did not complete, removing"
        );
        drop(tempfile);
    }
}
