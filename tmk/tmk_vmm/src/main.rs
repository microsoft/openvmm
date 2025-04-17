// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple VMM for loading and running test microkernels (TMKs) but does not
//! support general-purpose VMs.
//!
//! This is used to test the underlying VMM infrastructure without the complexity
//! of the full OpenVMM stack.

mod host_vmm;
mod load;
mod paravisor_vmm;
mod run;

use anyhow::Context;
use clap::Parser;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use run::CommonState;
use std::path::PathBuf;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .pretty()
                .map_event_format(|e| e.with_source_location(false))
                .fmt_fields(tracing_helpers::formatter::FieldFormatter)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE),
        )
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .with_env_var("TMK_LOG")
                .from_env_lossy(),
        )
        .init();

    DefaultPool::run_with(do_main)
}

/// A simple VMM for loading and running test microkernels (TMKs).
///
/// This is used to test the underlying VMM infrastructure without the complexity
/// of the full OpenVMM stack.
///
/// This can run either on a host or inside a paravisor environment.
#[derive(Parser)]
struct Options {
    /// The hypervisor interface to use to run the TMK.
    #[clap(long)]
    hv: Option<HypervisorOpt>,
    /// Disable offloads to the hypervisor. This disables WHP APIC emulation,
    /// for example.
    #[clap(long)]
    disable_offloads: bool,
    /// The path to the TMK binary.
    #[clap(long)]
    tmk: PathBuf,
    /// List tests available in the TMK.
    #[clap(long)]
    list: bool,
    /// Tests to run. Default is to run all tests.
    #[clap(conflicts_with("list"))]
    tests: Vec<String>,
}

#[derive(clap::ValueEnum, Copy, Clone)]
enum HypervisorOpt {
    /// Use KVM to run the TMK.
    #[cfg(target_os = "linux")]
    Kvm,
    /// Use mshv to run the TMK.
    #[cfg(all(target_os = "linux", guest_arch = "x86_64"))]
    Mshv,
    /// Use mshv-vtl to run the TMK; only supported inside a paravisor
    /// environment.
    #[cfg(target_os = "linux")]
    MshvVtl,
    /// Use WHP to run the TMK.
    #[cfg(target_os = "windows")]
    Whp,
    /// Use Hypervisor.Framework to run the TMK.
    #[cfg(target_os = "macos")]
    Hvf,
}

async fn do_main(driver: DefaultDriver) -> anyhow::Result<()> {
    let opts = Options::parse();

    if opts.list {
        let tmk = fs_err::File::open(&opts.tmk).context("failed to open TMK")?;
        let tests = load::enumerate_tests(&tmk)?;
        for test in tests {
            println!("{}", test.name);
        }
        Ok(())
    } else {
        let hv = match opts.hv {
            Some(hv) => hv,
            None => choose_hypervisor()?,
        };
        let mut state = CommonState::new(driver, opts).await?;

        state
            .for_each_test(async |state, test| match hv {
                #[cfg(target_os = "linux")]
                HypervisorOpt::Kvm => state.run_host_vmm(virt_kvm::Kvm, test).await,
                #[cfg(all(target_os = "linux", guest_arch = "x86_64"))]
                HypervisorOpt::Mshv => state.run_host_vmm(virt_mshv::LinuxMshv, test).await,
                #[cfg(target_os = "linux")]
                HypervisorOpt::MshvVtl => {
                    state
                        .run_paravisor_vmm(virt::IsolationType::None, test)
                        .await
                }
                #[cfg(windows)]
                HypervisorOpt::Whp => state.run_host_vmm(virt_whp::Whp, test).await,
                #[cfg(target_os = "macos")]
                HypervisorOpt::Hvf => state.run_host_vmm(virt_hvf::HvfHypervisor, test).await,
            })
            .await
    }
}

fn choose_hypervisor() -> anyhow::Result<HypervisorOpt> {
    #[cfg(target_os = "linux")]
    {
        if virt::Hypervisor::is_available(&virt_mshv::LinuxMshv)? {
            return Ok(HypervisorOpt::Mshv);
        }
        if virt::Hypervisor::is_available(&virt_kvm::Kvm)? {
            return Ok(HypervisorOpt::Kvm);
        }
    }
    #[cfg(windows)]
    {
        if virt::Hypervisor::is_available(&virt_whp::Whp)? {
            return Ok(HypervisorOpt::Whp);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if virt::Hypervisor::is_available(&virt_hvf::HvfHypervisor)? {
            return Ok(HypervisorOpt::Hvf);
        }
    }

    anyhow::bail!("no hypervisor available");
}
