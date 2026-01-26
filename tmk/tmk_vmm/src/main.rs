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
use anyhow::Result;
use clap::Parser;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use run::CommonState;
use std::path::PathBuf;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() -> Result<()> {
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
    /// Use mshv-vtl to run the TMK inside a CCA realm.
    #[cfg(all(target_os = "linux", guest_arch = "aarch64"))]
    Cca,
}

impl Options {
    fn finalize(mut self) -> Result<Self> {
        let hv = match self.hv {
            Some(hv) => hv,
            None => choose_hypervisor()?,
        };

        self.hv = Some(hv);

        Ok(self)
    }
}

async fn do_main(driver: DefaultDriver) -> Result<()> {
    let opts = Options::parse().finalize()?;

    if opts.list {
        let tmk = fs_err::File::open(&opts.tmk).context("failed to open TMK")?;
        let tests = load::enumerate_tests(&tmk)?;
        for test in tests {
            println!("{}", test.name);
        }
        Ok(())
    } else {
        let hv = opts.hv.expect("hv must have an finalized value");
        let mut state = CommonState::new(driver, opts).await?;

        state
            .for_each_test(async |state, test| match hv {
                #[cfg(target_os = "linux")]
                HypervisorOpt::Kvm => state.run_host_vmm(virt_kvm::Kvm::new()?, test).await,
                #[cfg(all(target_os = "linux", guest_arch = "x86_64"))]
                HypervisorOpt::Mshv => state.run_host_vmm(virt_mshv::LinuxMshv::new()?, test).await,
                #[cfg(target_os = "linux")]
                HypervisorOpt::MshvVtl => {
                    state
                        .run_paravisor_vmm(virt::IsolationType::None, test)
                        .await
                }
                #[cfg(all(target_os = "linux", guest_arch = "aarch64"))]
                HypervisorOpt::Cca => {
                    state
                        .run_paravisor_vmm(virt::IsolationType::Cca, test)
                        .await
                }
                #[cfg(windows)]
                HypervisorOpt::Whp => {
                    state
                        .run_host_vmm(
                            virt_whp::Whp {
                                user_mode_apic: state.state.opts.disable_offloads,
                                offload_enlightenments: !state.state.opts.disable_offloads,
                            },
                            test,
                        )
                        .await
                }
                #[cfg(target_os = "macos")]
                HypervisorOpt::Hvf => state.run_host_vmm(virt_hvf::HvfHypervisor, test).await,
            })
            .await
    }
}

fn choose_hypervisor() -> Result<HypervisorOpt> {
    #[cfg(all(target_os = "linux", guest_arch = "x86_64"))]
    {
        if virt_mshv::is_available()? {
            return Ok(HypervisorOpt::Mshv);
        }
    }
    #[cfg(target_os = "linux")]
    {
        if virt_kvm::is_available()? {
            return Ok(HypervisorOpt::Kvm);
        }
    }
    #[cfg(windows)]
    {
        if virt_whp::is_available()? {
            return Ok(HypervisorOpt::Whp);
        }
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(HypervisorOpt::Hvf);
    }

    #[expect(clippy::allow_attributes)]
    #[allow(unreachable_code, reason = "unreachable on some targets")]
    {
        anyhow::bail!("no hypervisor available");
    }
}
