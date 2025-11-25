// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone executable that hosts the IGVM agent Windows RPC façade.

// UNSAFETY: Windows FFI
#![cfg_attr(windows, expect(unsafe_code))]

#[cfg(target_os = "windows")]
mod rpc;

use cfg_if::cfg_if;
use std::process::ExitCode;

fn main() -> ExitCode {
    cfg_if! {
        if #[cfg(target_os = "windows")] {
            use tracing_subscriber::fmt;
            use tracing_subscriber::EnvFilter;

            let filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("test_igvm_agent_rpc_server=info"));

            let _ = fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stdout)
                .try_init();

            tracing::info!("launching IGVM agent RPC server binary");

            if let Err(err) = rpc::run_server() {
                tracing::error!(%err, "failed to run IGVM agent RPC server");
                return ExitCode::FAILURE;
            }

            tracing::info!("IGVM agent RPC server exited successfully");

            ExitCode::SUCCESS
        }
        else {
            eprintln!("IGVM agent RPC server is only supported on Windows hosts.");
            ExitCode::FAILURE
        }
    }
}
