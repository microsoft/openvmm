// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A simple VMM for loading and running test microkernels (TMKs) but does not
//! support general-purpose VMs.
//!
//! This is used to test the underlying VMM infrastructure without the complexity
//! of the full OpenVMM stack.

// UNSAFETY: needed to map guest memory.
#![expect(unsafe_code)]

mod host_vmm;
mod load;
mod paravisor_vmm;
mod run;

use clap::Parser;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use run::CommonState;
use std::path::PathBuf;
use tracing_subscriber::fmt::format::FmtSpan;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .fmt_fields(tracing_helpers::formatter::FieldFormatter)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
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
    hv: HypervisorOpt,
    /// The path to the TMK binary.
    #[clap(long)]
    tmk: PathBuf,
}

#[derive(clap::ValueEnum, Clone)]
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

    let mut state = CommonState::new(driver, opts).await?;

    match state.opts.hv {
        #[cfg(target_os = "linux")]
        HypervisorOpt::Kvm => state.run_host_vmm(virt_kvm::Kvm).await,
        #[cfg(all(target_os = "linux", guest_arch = "x86_64"))]
        HypervisorOpt::Mshv => state.run_host_vmm(virt_mshv::LinuxMshv).await,
        #[cfg(target_os = "linux")]
        HypervisorOpt::MshvVtl => state.run_paravisor_vmm(virt::IsolationType::None).await,
        #[cfg(windows)]
        HypervisorOpt::Whp => state.run_host_vmm(virt_whp::Whp).await,
        #[cfg(target_os = "macos")]
        HypervisorOpt::Hvf => state.run_host_vmm(virt_hvf::HvfHypervisor).await,
    }
}
