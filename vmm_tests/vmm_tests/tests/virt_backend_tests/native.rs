// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native backend registration and capability filtering.

use super::fixture::Fixture;
use anyhow::Context as _;
use pal_async::DefaultDriver;
use petri::requirements::OpenVmmHypervisor;
use petri::requirements::TestCaseRequirements;
use petri::requirements::TestRequirement;
use std::sync::OnceLock;
use virt::Hypervisor;
use virt::Partition as _;

pub(crate) struct Capabilities {
    pub(crate) time_control: bool,
    pub(crate) reset: bool,
    pub(crate) deadline: bool,
}

// Probe exactly the unenlightened, single-VP configuration exercised by the
// tests. In particular, MSHV Windows-guest reset restrictions do not apply.
fn probe<H: Hypervisor>(mut hv: H) -> anyhow::Result<Capabilities>
where
    H::Partition: virt::Partition,
{
    let mut pool = pal_async::DefaultPool::new();
    let driver = pool.driver();
    pool.run_until(async {
        let fixture = Fixture::new(driver).await?;
        let (partition, _binder) = fixture.build(&mut hv)?;
        Ok(Capabilities {
            time_control: partition.supports_time_control().is_some(),
            reset: partition.supports_reset().is_some(),
            deadline: partition.caps().tsc_deadline,
        })
    })
}

macro_rules! backend {
    ($module:ident, $backend:ident, $hv:ty, $constructor:expr) => {
        pub(crate) mod $module {
            use super::*;

            static CAPS: OnceLock<Result<Capabilities, String>> = OnceLock::new();

            fn new_hypervisor() -> anyhow::Result<$hv> {
                $constructor
            }

            pub(crate) fn capabilities() -> &'static Result<Capabilities, String> {
                CAPS.get_or_init(|| {
                    new_hypervisor()
                        .and_then(probe)
                        .map_err(|error| format!("{error:#}"))
                })
            }

            pub(crate) fn test(
                name: &'static str,
                supported: fn() -> bool,
                run: impl 'static + Send + AsyncFn(DefaultDriver, $hv) -> anyhow::Result<()>,
            ) -> petri::TestCase {
                petri::SimpleTest::new_async(
                    name,
                    |_| Some(()),
                    async move |_, driver, ()| {
                        // Probe errors are test failures, not evidence of an
                        // unsupported host. Preserve them in Petri's logs.
                        CAPS.get()
                            .context("backend capability requirements were not evaluated")?
                            .as_ref()
                            .map_err(|error| anyhow::anyhow!("{error}"))
                            .context(concat!(stringify!($module), " capability probe failed"))?;
                        anyhow::ensure!(supported(), "backend does not support test requirements");
                        let hv = new_hypervisor()
                            .context(concat!("failed to construct ", stringify!($module)))?;
                        run(driver, hv).await
                    },
                )
                .requirements(TestCaseRequirements::new(
                    TestRequirement::OpenVmmHypervisor(OpenVmmHypervisor::$backend)
                        .and(TestRequirement::HostCapability(supported)),
                ))
                .into()
            }
        }
    };
}

#[cfg(target_os = "linux")]
backend!(kvm, Kvm, virt_kvm::Kvm, Ok(virt_kvm::Kvm::new()?));
#[cfg(target_os = "linux")]
backend!(
    mshv,
    Mshv,
    virt_mshv::LinuxMshv,
    Ok(virt_mshv::LinuxMshv::new()?)
);
#[cfg(windows)]
backend!(
    whp,
    Whp,
    virt_whp::Whp,
    Ok(virt_whp::Whp {
        user_mode_apic: false,
        offload_enlightenments: false,
    })
);

/// Registers a contract function for each native backend at its definition site.
macro_rules! backend_test {
    ($test:ident, requires: [$($requirement:ident),* $(,)?]) => {
        #[cfg(target_os = "linux")]
        $crate::native::backend_test!(@backend kvm, $test, [$($requirement),*]);
        #[cfg(target_os = "linux")]
        $crate::native::backend_test!(@backend mshv, $test, [$($requirement),*]);
        #[cfg(windows)]
        $crate::native::backend_test!(@backend whp, $test, [$($requirement),*]);
    };
    (@backend $backend:ident, $test:ident, [$($requirement:ident),*]) => {
        petri::multitest!(vec![
            $crate::native::$backend::test(
                concat!(stringify!($backend), "::", stringify!($test)),
                || $crate::native::$backend::capabilities()
                    .as_ref()
                    .map_or(true, |caps| {
                        let _ = caps;
                        true $(&& caps.$requirement)*
                    }),
                async |driver, mut hv| {
                    use virt::BindProcessor as _;
                    let fixture = $crate::fixture::Fixture::new(driver).await?;
                    let (partition, mut binder) = fixture.build(&mut hv)?;
                    let mut processor = binder.bind()?;
                    $test(&partition, &mut processor)
                },
            ),
        ]);
    };
}

pub(crate) use backend_test;
