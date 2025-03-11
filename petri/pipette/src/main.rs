// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is the petri pipette agent, which runs on the guest and executes
//! commands and other requests from the host.

mod agent;
mod execute;
mod shutdown;
mod trace;
#[cfg(windows)]
mod winsvc;

// This is here to satisfy rust-analyzer on macos. Pipette does not yet support
// macos.
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("unsupported on macos")
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn main() -> anyhow::Result<()> {
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--service") {
        return winsvc::start_service();
    }

    pal_async::DefaultPool::run_with(async |driver| {
        let agent = agent::Agent::new(driver).await?;
        agent.run().await
    })
}
