// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is the petri pipette agent, which runs on the guest and executes
//! commands and other requests from the host.

#![cfg_attr(not(windows), forbid(unsafe_code))]

#[cfg(any(target_os = "linux", windows))]
mod agent;
#[cfg(any(target_os = "linux", windows))]
mod crash;
#[cfg(any(target_os = "linux", windows))]
mod execute;
#[cfg(any(target_os = "linux", windows))]
mod shutdown;
#[cfg(any(target_os = "linux", windows))]
mod trace;
#[cfg(windows)]
mod winsvc;

#[cfg(any(target_os = "linux", windows))]
fn main() -> anyhow::Result<()> {
    eprintln!("Pipette starting up");

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("Pipette panicked: {}", info);
        hook(info);
    }));

    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--service") {
        return winsvc::start_service();
    }

    pal_async::DefaultPool::run_with(async |driver| {
        let agent = agent::Agent::new(driver).await?;
        agent.run().await
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("unsupported platform");
}
