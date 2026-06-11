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
#[cfg(target_os = "linux")]
mod mount;
#[cfg(any(target_os = "linux", windows))]
mod shutdown;
#[cfg(any(target_os = "linux", windows))]
mod trace;
#[cfg(windows)]
mod winsvc;

#[cfg(any(target_os = "linux", windows))]
struct Args {
    #[cfg(windows)]
    service: bool,
    transport: agent::Transport,
}

#[cfg(any(target_os = "linux", windows))]
fn parse_args() -> anyhow::Result<Args> {
    use anyhow::Context;

    #[cfg(windows)]
    let mut service = false;
    let mut transport = agent::Transport::Vsock;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            #[cfg(windows)]
            "--service" => service = true,
            "--transport" => {
                let val = args
                    .next()
                    .context("--transport requires a value (tcp or vsock)")?;
                match val.as_str() {
                    "tcp" => transport = agent::Transport::Tcp,
                    "vsock" => transport = agent::Transport::Vsock,
                    other => anyhow::bail!("unknown transport {other:?}, expected tcp or vsock"),
                }
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok(Args {
        #[cfg(windows)]
        service,
        transport,
    })
}

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

    let args = parse_args()?;

    #[cfg(windows)]
    if args.service {
        return winsvc::start_service(args.transport);
    }

    let transport = args.transport;

    pal_async::DefaultPool::run_with(async |driver| {
        loop {
            let agent = agent::Agent::new(driver.clone(), transport).await?;
            agent.run().await?;
            eprintln!("Pipette disconnected, reconnecting...");
        }
    })
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("unsupported platform");
}
