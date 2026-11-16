// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test entrypoint for running OpenTMK guest tests.
//!
//! Each test boots a UEFI VM with a per-test [`opentmk_protocol::TestConfig`]
//! patched into `opentmk.efi`, streams the guest's `COM1` output through
//! [`petri::PetriVm::com1`], and drives [`OpenTmkRunner`] (with optional
//! pre-boot, per-event, and post-run hooks) before tearing down the VM.

#![forbid(unsafe_code)]
// Shared runner helpers are currently exercised by Windows Hyper-V tests.
#![cfg_attr(not(windows), expect(dead_code))]

use anyhow::Context as _;
use futures::StreamExt as _;
use opentmk_protocol::OpenTmkEvent;
use opentmk_protocol::OpenTmkRun;
use opentmk_protocol::TestConfig;
use petri::PetriLogFile;
use petri::PetriVm;
use petri::PetriVmArtifacts;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ProcessorTopology;
use petri_artifacts_common::tags::MachineArch;
use std::sync::Arc;
use std::time::Duration;
use tracing::Level;

/// Default VP count the guest-side OpenTMK tests assert on.
const OPENTMK_VP_COUNT: u32 = 4;

/// Default deadline for an OpenTMK guest to finish streaming results over COM1.
const OPENTMK_TIMEOUT: Duration = Duration::from_secs(240);

/// Control flow returned by an [`OpenTmkRunner::on_event`] hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookFlow {
    /// Continue scanning COM1 (stops automatically once `TEST_END` is seen).
    Continue,
    /// Stop scanning COM1 immediately and proceed to `on_complete` and teardown.
    Stop,
}

/// Async event hook invoked for each parsed [`OpenTmkEvent`] streamed over COM1.
trait EventHook<T: PetriVmmBackend> {
    async fn on_event(
        &mut self,
        vm: &mut PetriVm<T>,
        event: &OpenTmkEvent,
        run: &mut OpenTmkRun,
    ) -> anyhow::Result<HookFlow>;
}

struct NoopEventHook;

impl<T: PetriVmmBackend> EventHook<T> for NoopEventHook {
    async fn on_event(
        &mut self,
        _vm: &mut PetriVm<T>,
        _event: &OpenTmkEvent,
        _run: &mut OpenTmkRun,
    ) -> anyhow::Result<HookFlow> {
        Ok(HookFlow::Continue)
    }
}

struct FnEventHook<F>(F);

impl<T, F> EventHook<T> for FnEventHook<F>
where
    T: PetriVmmBackend,
    F: AsyncFnMut(&mut PetriVm<T>, &OpenTmkEvent, &mut OpenTmkRun) -> anyhow::Result<HookFlow>,
{
    async fn on_event(
        &mut self,
        vm: &mut PetriVm<T>,
        event: &OpenTmkEvent,
        run: &mut OpenTmkRun,
    ) -> anyhow::Result<HookFlow> {
        (self.0)(vm, event, run).await
    }
}

/// Async completion hook invoked after COM1 scanning finishes and before VM teardown.
trait CompleteHook<T: PetriVmmBackend> {
    async fn on_complete(self, vm: &mut PetriVm<T>, run: &OpenTmkRun) -> anyhow::Result<()>;
}

struct DefaultCompleteHook;

impl<T: PetriVmmBackend> CompleteHook<T> for DefaultCompleteHook {
    async fn on_complete(self, _vm: &mut PetriVm<T>, run: &OpenTmkRun) -> anyhow::Result<()> {
        run.evaluate().map_err(Into::into)
    }
}

struct FnCompleteHook<F>(F);

impl<T, F> CompleteHook<T> for FnCompleteHook<F>
where
    T: PetriVmmBackend,
    F: AsyncFnOnce(&mut PetriVm<T>, &OpenTmkRun) -> anyhow::Result<()>,
{
    async fn on_complete(self, vm: &mut PetriVm<T>, run: &OpenTmkRun) -> anyhow::Result<()> {
        (self.0)(vm, run).await
    }
}

/// Test-side driver for booting an OpenTMK guest, following `vm.com1()`, and
/// running custom host hooks.
struct OpenTmkRunner<'a, T: PetriVmmBackend, E = NoopEventHook, C = DefaultCompleteHook> {
    params: petri::PetriTestParams<'a>,
    vm_artifacts: PetriVmArtifacts<T>,
    timeout: Duration,
    configure: Option<Box<dyn FnOnce(PetriVmBuilder<T>) -> PetriVmBuilder<T> + 'a>>,
    on_event: E,
    on_complete: C,
}

impl<'a, T: PetriVmmBackend> OpenTmkRunner<'a, T, NoopEventHook, DefaultCompleteHook> {
    /// Creates a runner with default timeout, 4 VPs, no event hook, and default
    /// pass/fail evaluation.
    fn new(params: petri::PetriTestParams<'a>, vm_artifacts: PetriVmArtifacts<T>) -> Self {
        Self {
            params,
            vm_artifacts,
            timeout: OPENTMK_TIMEOUT,
            configure: None,
            on_event: NoopEventHook,
            on_complete: DefaultCompleteHook,
        }
    }
}

impl<'a, T: PetriVmmBackend, E, C> OpenTmkRunner<'a, T, E, C> {
    /// Overrides the maximum duration to wait for OpenTMK COM1 output.
    #[expect(dead_code, reason = "hook API for custom OpenTMK tests")]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Customizes the [`PetriVmBuilder`] prior to booting the VM (e.g. enabling
    /// a vTPM, adjusting topology, or setting OpenHCL command-line flags).
    #[expect(dead_code, reason = "hook API for custom OpenTMK tests")]
    fn configure(mut self, f: impl FnOnce(PetriVmBuilder<T>) -> PetriVmBuilder<T> + 'a) -> Self {
        self.configure = Some(Box::new(f));
        self
    }

    /// Registers an async hook invoked for every [`OpenTmkEvent`] parsed from
    /// `COM1` while the VM is running.
    #[expect(dead_code, reason = "hook API for custom OpenTMK tests")]
    fn on_event<F>(self, f: F) -> OpenTmkRunner<'a, T, FnEventHook<F>, C>
    where
        F: AsyncFnMut(&mut PetriVm<T>, &OpenTmkEvent, &mut OpenTmkRun) -> anyhow::Result<HookFlow>,
    {
        OpenTmkRunner {
            params: self.params,
            vm_artifacts: self.vm_artifacts,
            timeout: self.timeout,
            configure: self.configure,
            on_event: FnEventHook(f),
            on_complete: self.on_complete,
        }
    }

    /// Registers an async completion hook invoked after COM1 scanning ends and
    /// before VM teardown, replacing default `OpenTmkRun::evaluate` validation.
    #[expect(dead_code, reason = "hook API for custom OpenTMK tests")]
    fn on_complete<F>(self, f: F) -> OpenTmkRunner<'a, T, E, FnCompleteHook<F>>
    where
        F: AsyncFnOnce(&mut PetriVm<T>, &OpenTmkRun) -> anyhow::Result<()>,
    {
        OpenTmkRunner {
            params: self.params,
            vm_artifacts: self.vm_artifacts,
            timeout: self.timeout,
            configure: self.configure,
            on_event: self.on_event,
            on_complete: FnCompleteHook(f),
        }
    }

    /// Boots the OpenTMK guest, streams `COM1` events through any registered
    /// hooks, runs completion evaluation, and tears down the VM.
    fn run(self) -> anyhow::Result<()>
    where
        E: EventHook<T>,
        C: CompleteHook<T>,
    {
        pal_async::DefaultPool::run_with(async |driver| {
            let opentmk_log = self.params.log_source.log_file("opentmk")?;
            let mut builder = PetriVmBuilder::new(self.params, self.vm_artifacts, &driver)?
                .with_expect_no_boot_event()
                .with_processor_topology(ProcessorTopology {
                    vp_count: OPENTMK_VP_COUNT,
                    ..Default::default()
                });
            if let Some(configure) = self.configure {
                builder = configure(builder);
            }

            let mut vm = builder.run_without_agent().await?;
            let mut com1 = vm.com1()?;
            let mut run = OpenTmkRun::default();
            let mut on_event = self.on_event;
            let on_complete = self.on_complete;
            let timeout = self.timeout;

            // Capture the scan/evaluation result so teardown always runs even on
            // failure: OpenTMK spins after TEST_END rather than powering off.
            let result: anyhow::Result<()> = async {
                let timed = mesh::CancelContext::new()
                    .with_timeout(timeout)
                    .until_cancelled(async {
                        while let Some(line) = com1.next().await {
                            let line = line.trim_end();
                            if line.is_empty() {
                                continue;
                            }
                            let event = run.apply_line(line);
                            log_opentmk_event(&opentmk_log, &event);

                            if on_event.on_event(&mut vm, &event, &mut run).await? == HookFlow::Stop
                            {
                                break;
                            }
                            if run.ended {
                                break;
                            }
                        }
                        anyhow::Ok(())
                    })
                    .await;

                match timed {
                    Ok(inner) => inner?,
                    Err(_) => {
                        opentmk_log.write_entry_fmt(
                            None,
                            Level::ERROR,
                            format_args!("OpenTMK COM1 scan timed out after {timeout:?}"),
                        );
                    }
                }

                on_complete.on_complete(&mut vm, &run).await
            }
            .await;

            let teardown_result = vm.teardown().await;
            result.and(teardown_result)
        })
    }
}

/// Maps an OpenTMK `log::Level` string to a tracing [`Level`].
fn opentmk_level_to_tracing(level: &str) -> Level {
    match level {
        "ERROR" => Level::ERROR,
        "WARN" => Level::WARN,
        "INFO" => Level::INFO,
        "DEBUG" => Level::DEBUG,
        "TRACE" => Level::TRACE,
        _ => Level::INFO,
    }
}

/// Formats and writes structured [`OpenTmkEvent`] entries to `opentmk.log`.
///
/// [`OpenTmkEvent::Raw`] lines (such as UEFI firmware output) are omitted here
/// because Petri already records raw `COM1` output in `guest.log` / `uefi.log`.
fn log_opentmk_event(log_file: &PetriLogFile, event: &OpenTmkEvent) {
    let format_loc = |msg: &str, line: &str| {
        if line.is_empty() {
            msg.to_owned()
        } else {
            format!("{msg} ({line})")
        }
    };

    match event {
        OpenTmkEvent::TestStart { line } => {
            log_file.write_entry_fmt(
                None,
                Level::INFO,
                format_args!("{}", format_loc(opentmk_protocol::TEST_START, line)),
            );
        }
        OpenTmkEvent::TestEnd { line } => {
            log_file.write_entry_fmt(
                None,
                Level::INFO,
                format_args!("{}", format_loc(opentmk_protocol::TEST_END, line)),
            );
        }
        OpenTmkEvent::Log {
            level,
            message,
            line,
        } => {
            log_file.write_entry_fmt(
                None,
                opentmk_level_to_tracing(level),
                format_args!("{}", format_loc(message, line)),
            );
        }
        OpenTmkEvent::Panic { message, line } => {
            log_file.write_entry_fmt(
                None,
                Level::ERROR,
                format_args!("{}", format_loc(message, line)),
            );
        }
        OpenTmkEvent::Assert {
            expression,
            description,
            passed,
            line,
        } => {
            let level = if *passed { Level::INFO } else { Level::ERROR };
            let status = if *passed { "pass" } else { "FAIL" };
            let body = if description.is_empty() {
                format!("assert [{status}] {expression}")
            } else {
                format!("assert [{status}] {expression}: {description}")
            };
            log_file.write_entry_fmt(None, level, format_args!("{}", format_loc(&body, line)));
        }
        OpenTmkEvent::Raw(_) => {}
    }
}

/// Serializes the embedded [`TestConfig`] payload for `test`.
fn opentmk_config_json(test: &str) -> Vec<u8> {
    serde_json::to_vec(&TestConfig {
        version: 1,
        backend: "hyperv".to_owned(),
        test: test.to_owned(),
        params: serde_json::Value::Null,
    })
    .expect("opentmk config serialization cannot fail")
}

/// Constructs a lazy [`petri::UefiGuest::CustomDisk`] that patches `config_json`
/// into the architecture-matched `opentmk.efi` artifact and packages it into a
/// bootable FAT32 VHD.
fn opentmk_uefi_guest(
    resolver: &petri::ArtifactResolver<'_>,
    arch: MachineArch,
    config_json: Vec<u8>,
) -> petri::UefiGuest {
    use petri_artifacts_vmm_test::artifacts::opentmk::*;
    let efi = match arch {
        MachineArch::X86_64 => resolver.require(OPENTMK_EFI_X64).erase(),
        MachineArch::Aarch64 => resolver.require(OPENTMK_EFI_AARCH64).erase(),
    };
    petri::UefiGuest::CustomDisk(petri::CustomUefiDisk::new(move || {
        let efi_bytes = fs_err::read(efi.get()).context("failed to read opentmk.efi")?;
        let disk_arch = match arch {
            MachineArch::X86_64 => opentmk_disk::Arch::X86_64,
            MachineArch::Aarch64 => opentmk_disk::Arch::Aarch64,
        };
        let image =
            opentmk_disk::build_opentmk_vhd_with_config(&efi_bytes, disk_arch, &config_json)
                .context("failed to build opentmk boot VHD")?;
        Ok(petri::Disk::Temporary(Arc::new(image.into_temp_path())))
    }))
}

/// Build the host requirements for an OpenTMK test with the given `isolation`.
///
/// Isolated variants declare an isolation requirement so they are only selected
/// on hosts that support that isolation type, and are excluded elsewhere.
fn opentmk_requirements(
    isolation: Option<petri::IsolationType>,
) -> petri::requirements::TestCaseRequirements {
    use petri::requirements::ExecutionEnvironment;
    use petri::requirements::IsolationType as ReqIsolationType;
    use petri::requirements::TestRequirement;

    let requirement = match isolation {
        None => TestRequirement::Any,
        // Hyper-V VBS does not work nested, so also require a baremetal host
        // (matching the `#[vmm_test]` macro behavior for hyper-v + vbs).
        Some(petri::IsolationType::Vbs) => TestRequirement::Isolation(ReqIsolationType::Vbs).and(
            TestRequirement::ExecutionEnvironment(ExecutionEnvironment::Baremetal),
        ),
        Some(petri::IsolationType::Snp) => TestRequirement::Isolation(ReqIsolationType::Snp),
        Some(petri::IsolationType::Tdx) => TestRequirement::Isolation(ReqIsolationType::Tdx),
    };

    petri::requirements::TestCaseRequirements::new(requirement)
}

/// Resolve the artifacts to boot OpenTMK under OpenHCL, optionally as a
/// confidential VM. Returns `None` if the host can't run it (non-x86_64).
fn resolve_opentmk_openhcl<T: PetriVmmBackend>(
    resolver: &petri::ArtifactResolver<'_>,
    test: &str,
    isolation: Option<petri::IsolationType>,
) -> Option<PetriVmArtifacts<T>> {
    let arch = MachineArch::host();
    if arch != MachineArch::X86_64 {
        return None;
    }
    let guest = opentmk_uefi_guest(resolver, arch, opentmk_config_json(test));
    PetriVmArtifacts::new(
        resolver,
        petri::Firmware::openhcl_uefi(resolver, arch, guest, isolation),
        arch,
        false,
    )
}

#[cfg(windows)]
mod hyperv {
    use crate::OpenTmkRunner;
    use crate::resolve_opentmk_openhcl;
    use petri::IsolationType;
    use petri::hyperv::HyperVPetriBackend;

    /// Maps an isolation token to its `Option<petri::IsolationType>` value.
    macro_rules! opentmk_isolation {
        (none) => {
            None
        };
        (vbs) => {
            Some(IsolationType::Vbs)
        };
        (snp) => {
            Some(IsolationType::Snp)
        };
        (tdx) => {
            Some(IsolationType::Tdx)
        };
    }

    /// Defines one OpenHCL OpenTMK test per isolation token in the `[..]` list,
    /// all running the guest-internal scenario `$test`, with an optional runner
    /// customization closure (`|runner| runner.configure(...)...`).
    macro_rules! opentmk_test {
        ($base:literal, $test:literal, [$($iso:tt),+ $(,)?]) => {
            opentmk_test!($base, $test, [$($iso),+], |runner| runner);
        };
        ($base:literal, $test:literal, [$($iso:tt),+ $(,)?], |$runner:ident| $customize:expr) => {
            ::petri::multitest!(vec![
                $( opentmk_test!(@case $base, $test, $iso, |$runner| $customize) ),+
            ]);
        };
        (@case $base:literal, $test:literal, none, |$runner:ident| $customize:expr) => {
            opentmk_test!(@build concat!($base), $test, none, |$runner| $customize)
        };
        (@case $base:literal, $test:literal, vbs, |$runner:ident| $customize:expr) => {
            opentmk_test!(@build concat!($base, "_vbs"), $test, vbs, |$runner| $customize)
        };
        (@case $base:literal, $test:literal, snp, |$runner:ident| $customize:expr) => {
            opentmk_test!(@build concat!($base, "_snp"), $test, snp, |$runner| $customize)
        };
        (@case $base:literal, $test:literal, tdx, |$runner:ident| $customize:expr) => {
            opentmk_test!(@build concat!($base, "_tdx"), $test, tdx, |$runner| $customize)
        };
        (@build $name:expr, $test:literal, $iso:tt, |$runner:ident| $customize:expr) => {
            ::petri::SimpleTest::new_sync(
                $name,
                |resolver| {
                    resolve_opentmk_openhcl::<HyperVPetriBackend>(
                        resolver,
                        $test,
                        opentmk_isolation!($iso),
                    )
                },
                |params, artifacts| {
                    let $runner = OpenTmkRunner::new(params, artifacts);
                    ($customize).run()
                },
            )
            .requirements(crate::opentmk_requirements(opentmk_isolation!($iso)))
            .into()
        };
    }

    opentmk_test!(
        "opentmk_hyperv_openhcl_uefi_x64",
        "hv_processor",
        [none, vbs, snp, tdx]
    );

    opentmk_test!(
        "opentmk_hyperv_openhcl_memory_protect_read",
        "hv_memory_protect_read",
        [none, vbs, snp, tdx]
    );

    opentmk_test!(
        "opentmk_hyperv_openhcl_memory_protect_write",
        "hv_memory_protect_write",
        [none, vbs, snp, tdx]
    );

    opentmk_test!(
        "opentmk_hyperv_openhcl_register_intercept",
        "hv_register_intercept",
        [none, vbs, snp, tdx]
    );
}

#[cfg(windows)]
fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}

#[cfg(not(windows))]
fn main() {}
