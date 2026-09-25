// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The restore readiness endpoint: a single-use, process-local sink to which
//! the VM worker writes its restore readiness event when the restored VM first
//! starts.

use crate::Options;
use anyhow::Context;
use std::fs::File;
use std::io;
use std::path::Path;

/// Connects the `--restore-ready-path` endpoint, if one is configured.
pub(crate) fn restore_ready_sink(opt: &Options) -> anyhow::Result<Option<File>> {
    opt.restore_ready_path
        .as_deref()
        .map(connect_restore_ready_sink)
        .transpose()
        .context("failed to connect restore readiness endpoint")
}

/// Connects a single-use restore-readiness event sink.
pub(crate) fn connect_restore_ready_sink(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;

        let socket = unix_socket::UnixStream::connect(path)?;
        Ok(File::from(OwnedFd::from(socket)))
    }

    #[cfg(windows)]
    {
        const NAMED_PIPE_PREFIX: &str = "//./pipe/";

        let normalized = path.to_string_lossy().replace('\\', "/");
        if !normalized.starts_with(NAMED_PIPE_PREFIX) || normalized.len() == NAMED_PIPE_PREFIX.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "restore readiness path must name a Windows //./pipe/... endpoint",
            ));
        }
        std::fs::OpenOptions::new().write(true).open(path)
    }
}
