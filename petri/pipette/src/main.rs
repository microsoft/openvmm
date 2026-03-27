// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is the petri pipette agent, which runs on the guest and executes
//! commands and other requests from the host.

// UNSAFETY: init.rs requires unsafe for libc calls (fork, mount, reboot, waitpid)
// on Linux; shutdown.rs requires unsafe for the Windows shutdown API.
#![cfg_attr(not(any(windows, target_os = "linux")), forbid(unsafe_code))]

#[cfg(any(target_os = "linux", windows))]
mod agent;
#[cfg(any(target_os = "linux", windows))]
mod crash;
#[cfg(any(target_os = "linux", windows))]
mod execute;
#[cfg(target_os = "linux")]
mod init;
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

    // When running as PID 1 (rdinit=/pipette), perform minimal init duties
    // before starting the agent.
    #[cfg(target_os = "linux")]
    if init::is_pid1() {
        init::init_as_pid1()?;
    }

    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("--service") {
        return winsvc::start_service();
    }

    pal_async::DefaultPool::run_with(async |driver| {
        loop {
            let agent = agent::Agent::new(driver.clone()).await?;
            agent.run().await?;
            eprintln!("Pipette disconnected, reconnecting...");
        }
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("unsupported platform");
}
