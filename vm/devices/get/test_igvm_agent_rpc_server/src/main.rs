// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone executable that hosts the IGVM agent Windows RPC façade.

#[cfg(target_os = "windows")]
mod rpc;

use cfg_if::cfg_if;
use clap::Parser;
use get_resources::ged::IgvmAttestTestConfig;
use std::process::ExitCode;

/// IGVM Agent RPC Server
#[derive(Parser, Debug)]
#[clap(name = "test_igvm_agent_rpc_server")]
#[clap(about = "Test IGVM Agent RPC Server for attestation testing")]
struct Args {
    /// Test configuration to use for the IGVM agent
    #[clap(long, value_enum)]
    test_config: Option<TestConfig>,
}

/// Test configuration options
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum TestConfig {
    /// Test AK cert retry after failure
    AkCertRequestFailureAndRetry,
    /// Test AK cert persistency across boots
    AkCertPersistentAcrossBoot,
}

impl From<TestConfig> for IgvmAttestTestConfig {
    fn from(config: TestConfig) -> Self {
        match config {
            TestConfig::AkCertRequestFailureAndRetry => {
                IgvmAttestTestConfig::AkCertRequestFailureAndRetry
            }
            TestConfig::AkCertPersistentAcrossBoot => {
                IgvmAttestTestConfig::AkCertPersistentAcrossBoot
            }
        }
    }
}

fn main() -> ExitCode {
    cfg_if! {
        if #[cfg(target_os = "windows")] {
            use tracing_subscriber::fmt;
            use tracing_subscriber::EnvFilter;
            use test_igvm_agent_lib::IgvmAgentTestSetting;

            let args = Args::parse();

            let filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("test_igvm_agent_rpc_server=info"));

            let _ = fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();

            tracing::info!("launching IGVM agent RPC server binary");

            // Install test plan if a configuration was provided
            if let Some(test_config) = args.test_config {
                let igvm_config: IgvmAttestTestConfig = test_config.into();
                let setting = IgvmAgentTestSetting::TestConfig(igvm_config);
                rpc::igvm_agent::install_plan(&setting);
                tracing::info!(?test_config, "installed test configuration");
            } else {
                tracing::info!("no test configuration provided, using default behavior");
            }

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
