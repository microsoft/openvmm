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
    pub(crate) hv1: bool,
}

// Probe the same enlightenment configuration used by the test, including
// configuration-dependent restrictions such as MSHV Windows-guest reset.
fn probe<H: Hypervisor>(mut hv: H, hv1: bool) -> anyhow::Result<Capabilities>
where
    H::Partition: virt::Partition,
{
    let mut pool = pal_async::DefaultPool::new();
    let driver = pool.driver();
    pool.run_until(async {
        let fixture = Fixture::new(driver).await?;
        let (partition, _binder) = fixture.build(&mut hv, hv1)?;
        Ok(Capabilities {
            time_control: partition.supports_time_control().is_some(),
            reset: partition.supports_reset().is_some(),
            deadline: partition.caps().tsc_deadline,
            hv1: partition.caps().hv1,
        })
    })
}

macro_rules! backend {
    ($module:ident, $backend:ident, $hv:ty, $constructor:expr) => {
        pub(crate) mod $module {
            use super::*;

            static CAPS: [OnceLock<Result<Capabilities, String>>; 2] =
                [OnceLock::new(), OnceLock::new()];

            fn new_hypervisor(hv1: bool) -> anyhow::Result<$hv> {
                ($constructor)(hv1)
            }

            pub(crate) fn capabilities(hv1: bool) -> &'static Result<Capabilities, String> {
                CAPS[usize::from(hv1)].get_or_init(|| {
                    new_hypervisor(hv1)
                        .and_then(|hv| probe(hv, hv1))
                        .map_err(|error| format!("{error:#}"))
                })
            }

            pub(crate) fn test(
                name: &'static str,
                hv1: bool,
                supported: fn() -> bool,
                run: impl 'static + Send + AsyncFn(DefaultDriver, $hv) -> anyhow::Result<()>,
            ) -> petri::TestCase {
                petri::SimpleTest::new_async(
                    name,
                    |_| Some(()),
                    async move |_, driver, ()| {
                        // Probe errors are test failures, not evidence of an
                        // unsupported host. Preserve them in Petri's logs.
                        CAPS[usize::from(hv1)]
                            .get()
                            .context("backend capability requirements were not evaluated")?
                            .as_ref()
                            .map_err(|error| anyhow::anyhow!("{error}"))
                            .context(concat!(stringify!($module), " capability probe failed"))?;
                        anyhow::ensure!(supported(), "backend does not support test requirements");
                        let hv = new_hypervisor(hv1)
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
backend!(kvm, Kvm, virt_kvm::Kvm, |_| Ok(virt_kvm::Kvm::new()?));
#[cfg(target_os = "linux")]
backend!(kvm_tsc_fallback, Kvm, virt_kvm::Kvm, |_| {
    let mut kvm = virt_kvm::Kvm::new()?;
    kvm.force_tsc_fallback(true);
    Ok(kvm)
});
#[cfg(target_os = "linux")]
backend!(mshv, Mshv, virt_mshv::LinuxMshv, |_| Ok(
    virt_mshv::LinuxMshv::new()?
));
#[cfg(windows)]
backend!(whp, Whp, virt_whp::Whp, |hv1| Ok(virt_whp::Whp {
    user_mode_apic: false,
    offload_enlightenments: hv1,
}));

/// Registers a contract function for each native backend at its definition site.
macro_rules! backend_test {
    ($test:ident, requires: [$($requirement:ident),* $(,)?]) => {
        $crate::native::backend_test!($test, hv1: false, requires: [$($requirement),*]);
    };
    ($test:ident, hv1: $hv1:literal, requires: [$($requirement:ident),* $(,)?]) => {
        #[cfg(target_os = "linux")]
        $crate::native::backend_test!(@backend kvm, $test, $hv1, [$($requirement),*]);
        #[cfg(target_os = "linux")]
        $crate::native::backend_test!(@backend kvm_tsc_fallback, $test, $hv1, [$($requirement),*]);
        #[cfg(target_os = "linux")]
        $crate::native::backend_test!(@backend mshv, $test, $hv1, [$($requirement),*]);
        #[cfg(windows)]
        $crate::native::backend_test!(@backend whp, $test, $hv1, [$($requirement),*]);
    };
    (@backend $backend:ident, $test:ident, $hv1:literal, [$($requirement:ident),*]) => {
        petri::multitest!(vec![
            $crate::native::$backend::test(
                concat!(stringify!($backend), "::", stringify!($test)),
                $hv1,
                || $crate::native::$backend::capabilities($hv1)
                    .as_ref()
                    .map_or(true, |caps| {
                        let _ = caps;
                        true $(&& caps.$requirement)*
                    }),
                async |driver, mut hv| {
                    use virt::BindProcessor as _;
                    let fixture = $crate::fixture::Fixture::new(driver).await?;
                    let (partition, mut binder) = fixture.build(&mut hv, $hv1)?;
                    let mut processor = binder.bind()?;
                    $test(&partition, &mut processor)
                },
            ),
        ]);
    };
}

pub(crate) use backend_test;
