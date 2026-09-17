// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for defining tests.

#[doc(hidden)]
pub mod test_macro_support {
    // UNSAFETY: Needed for linkme.
    #![expect(unsafe_code)]

    use super::TestCase;
    pub use linkme;

    #[linkme::distributed_slice]
    pub static TESTS: [Option<fn() -> (&'static str, Vec<TestCase>)>];

    // Always have at least one entry to work around linker bugs.
    //
    // See <https://github.com/llvm/llvm-project/issues/65855>.
    #[linkme::distributed_slice(TESTS)]
    static WORKAROUND: Option<fn() -> (&'static str, Vec<TestCase>)> = None;
}

use crate::PetriLogSource;
use crate::TestArtifactRequirements;
use crate::TestArtifacts;
use crate::requirements::HostContext;
use crate::requirements::TestCaseRequirements;
use crate::requirements::can_run_test_with_context;
use crate::tracing::try_init_tracing;
use anyhow::Context as _;
use futures::FutureExt as _;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::RemoteAccess;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use test_macro_support::TESTS;

/// Defines a single test named after the async run function `$f`, using `$req`
/// to resolve its artifacts. See [`SimpleTest::new_async`].
#[macro_export]
macro_rules! test {
    ($f:ident, $req:expr) => {
        $crate::multitest!(vec![
            $crate::SimpleTest::new_async(stringify!($f), $req, $f).into()
        ]);
    };
}

/// Defines a single unstable test named after the async run function `$f`,
/// using `$req` to resolve its artifacts.
///
/// `$reason` documents why the test is unstable and is logged when an unstable
/// failure is ignored.
#[macro_export]
macro_rules! unstable_test {
    ($f:ident, $req:expr, $reason:expr) => {
        $crate::multitest!(vec![
            $crate::SimpleTest::new_async(stringify!($f), $req, $f)
                .unstable($reason)
                .into()
        ]);
    };
}

/// Defines a single test named after the synchronous run function `$f`, using
/// `$req` to resolve its artifacts. See [`SimpleTest::new_sync`].
#[macro_export]
macro_rules! test_sync {
    ($f:ident, $req:expr) => {
        $crate::multitest!(vec![
            $crate::SimpleTest::new_sync(stringify!($f), $req, $f).into()
        ]);
    };
}

/// Defines a single unstable test named after the synchronous run function
/// `$f`, using `$req` to resolve its artifacts.
///
/// `$reason` documents why the test is unstable and is logged when an unstable
/// failure is ignored.
#[macro_export]
macro_rules! unstable_test_sync {
    ($f:ident, $req:expr, $reason:expr) => {
        $crate::multitest!(vec![
            $crate::SimpleTest::new_sync(stringify!($f), $req, $f)
                .unstable($reason)
                .into()
        ]);
    };
}

/// Defines a set of tests from an expression evaluating to a
/// [`Vec<TestCase>`](TestCase).
#[macro_export]
macro_rules! multitest {
    ($tests:expr) => {
        const _: () = {
            use $crate::test_macro_support::linkme;
            #[linkme::distributed_slice($crate::test_macro_support::TESTS)]
            #[linkme(crate = linkme)]
            static TEST: Option<fn() -> (&'static str, Vec<$crate::TestCase>)> =
                Some(|| (module_path!(), $tests));
        };
    };
}

/// A single test case.
pub struct TestCase(Box<dyn DynRunTest>);

impl TestCase {
    /// Creates a new test case from a value that implements [`RunTest`].
    pub fn new(test: impl 'static + RunTest) -> Self {
        Self(Box::new(test))
    }
}

impl<T: 'static + RunTest> From<T> for TestCase {
    fn from(test: T) -> Self {
        Self::new(test)
    }
}

/// A single test, with module name.
struct Test {
    module: &'static str,
    test: TestCase,
    artifact_requirements: TestArtifactRequirements,
}

impl Test {
    /// Returns all the tests defined in this crate.
    fn all() -> impl Iterator<Item = Self> {
        TESTS.iter().flatten().flat_map(|f| {
            let (module, tests) = f();
            tests.into_iter().filter_map(move |test| {
                let mut artifact_requirements = test.0.artifact_requirements()?;
                // All tests require the log directory.
                artifact_requirements.require(
                    petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY,
                    RemoteAccess::LocalOnly,
                    false,
                );
                Some(Self {
                    module,
                    artifact_requirements,
                    test,
                })
            })
        })
    }

    /// Returns the name of the test.
    fn name(&self) -> String {
        // Strip the crate name from the module path, for consistency with libtest.
        match self.module.split_once("::") {
            Some((_crate_name, rest)) => format!("{}::{}", rest, self.test.0.leaf_name()),
            None => self.test.0.leaf_name().to_owned(),
        }
    }

    fn run(
        &self,
        resolve: fn(&str, TestArtifactRequirements) -> anyhow::Result<TestArtifacts>,
    ) -> anyhow::Result<()> {
        let name = self.name();
        let artifacts = resolve(&name, self.artifact_requirements.clone())
            .context("failed to resolve artifacts")?;
        let output_dir = artifacts.get(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY);
        let logger = try_init_tracing(output_dir, tracing::level_filters::LevelFilter::DEBUG)
            .context("failed to initialize tracing")?;
        // Record the test's identity up front, so that a test which is killed
        // or crashes before reporting a result is still identifiable.
        logger.log_test_start(&name);
        let mut post_test_hooks = Vec::new();

        // A process that is faulted or killed writes nothing to its own logs,
        // so the host's error reporting events are the only record that it
        // crashed and the only source of the Watson report ID needed to find
        // the dump.
        #[cfg(windows)]
        post_test_hooks.push(collect_watson_events_hook(logger.clone()));

        // Catch test panics in order to cleanly log the panic result. Without
        // this, `libtest_mimic` will report the panic to stdout and fail the
        // test, but the details won't end up in our per-test JSON log.
        let r = catch_unwind(AssertUnwindSafe(|| {
            self.test.0.run(
                PetriTestParams {
                    test_name: &name,
                    logger: &logger,
                    post_test_hooks: &mut post_test_hooks,
                },
                &artifacts,
            )
        }));
        let r = r.unwrap_or_else(|err| {
            // The error from `catch_unwind` is almost always either a
            // `&str` or a `String`, since that's what `panic!` produces.
            let msg = err
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| err.downcast_ref::<String>().map(|x| x.as_str()));

            let err = if let Some(msg) = msg {
                anyhow::anyhow!("test panicked: {msg}")
            } else {
                anyhow::anyhow!("test panicked (unknown payload type)")
            };
            Err(err)
        });
        logger.log_test_result(&r, self.test.0.unstable().is_some());

        for hook in post_test_hooks {
            tracing::info!(name = hook.name(), "Running post-test hook");
            if let Err(e) = hook.run(r.is_ok()) {
                tracing::error!(
                    error = e.as_ref() as &dyn std::error::Error,
                    "Post-test hook failed"
                );
            } else {
                tracing::info!("Post-test hook completed successfully");
            }
        }

        r
    }

    /// Returns a libtest-mimic trial to run the test.
    fn trial(
        self,
        resolve: fn(&str, TestArtifactRequirements) -> anyhow::Result<TestArtifacts>,
    ) -> libtest_mimic::Trial {
        libtest_mimic::Trial::test(self.name(), move || {
            let unstable = self.test.0.unstable();
            match self.run(resolve) {
                Ok(()) => Ok(()),
                Err(err) => {
                    let Some(reason) = unstable else {
                        return Err(format!("{err:#}").into());
                    };
                    if std::env::var("PETRI_IGNORE_UNSTABLE_FAILURES")
                        .ok()
                        .is_some_and(|v| !v.is_empty() && v != "0")
                    {
                        tracing::warn!(reason, "ignoring unstable test failure: {err:#}");
                        return Ok(());
                    }
                    Err(format!("unstable test failed (reason: {reason}): {err:#}").into())
                }
            }
        })
    }
}

/// A test that can be run.
///
/// Register it to be run with [`test!`] or [`multitest!`].
pub trait RunTest: Send {
    /// The type of artifacts required by the test.
    type Artifacts;

    /// The leaf name of the test.
    ///
    /// To produce the full test name, this will be prefixed with the module
    /// name where the test is defined.
    fn leaf_name(&self) -> &str;
    /// Returns the artifacts required by the test.
    ///
    /// Returns `None` if this test makes no sense for this host environment
    /// (e.g., an x86_64 test on an aarch64 host) and should be left out of the
    /// test list.
    fn resolve(&self, resolver: ArtifactResolver<'_>) -> Option<Self::Artifacts>;
    /// Runs the test, which has been assigned `name`, with the given
    /// `artifacts`.
    fn run(&self, params: PetriTestParams<'_>, artifacts: Self::Artifacts) -> anyhow::Result<()>;
    /// Returns the host requirements of the current test, if any.
    fn host_requirements(&self) -> Option<&TestCaseRequirements>;
    /// If this test is unstable, the reason why; `None` if stable.
    fn unstable(&self) -> Option<&str>;
    /// Whether this test is ignored (skipped by default, like a libtest
    /// `#[ignore]` test).
    fn ignored(&self) -> bool;
}

trait DynRunTest: Send {
    fn leaf_name(&self) -> &str;
    fn artifact_requirements(&self) -> Option<TestArtifactRequirements>;
    fn run(&self, params: PetriTestParams<'_>, artifacts: &TestArtifacts) -> anyhow::Result<()>;
    fn host_requirements(&self) -> Option<&TestCaseRequirements>;
    fn unstable(&self) -> Option<&str>;
    fn ignored(&self) -> bool;
}

impl<T: RunTest> DynRunTest for T {
    fn leaf_name(&self) -> &str {
        self.leaf_name()
    }

    fn artifact_requirements(&self) -> Option<TestArtifactRequirements> {
        let mut requirements = TestArtifactRequirements::new();
        self.resolve(ArtifactResolver::collector(&mut requirements))?;
        Some(requirements)
    }

    fn run(&self, params: PetriTestParams<'_>, artifacts: &TestArtifacts) -> anyhow::Result<()> {
        let artifacts = self
            .resolve(ArtifactResolver::resolver(artifacts))
            .context("test should have been skipped")?;
        self.run(params, artifacts)
    }

    fn host_requirements(&self) -> Option<&TestCaseRequirements> {
        self.host_requirements()
    }

    fn unstable(&self) -> Option<&str> {
        self.unstable()
    }

    fn ignored(&self) -> bool {
        self.ignored()
    }
}

/// Parameters passed to a [`RunTest`] when it is run.
pub struct PetriTestParams<'a> {
    /// The name of the running test.
    pub test_name: &'a str,
    /// The logger for the test.
    pub logger: &'a PetriLogSource,
    /// Any hooks that want to run after the test completes.
    pub post_test_hooks: &'a mut Vec<PetriPostTestHook>,
}

/// A post-test hook to be run after the test completes, regardless of if it
/// succeeds or fails.
pub struct PetriPostTestHook {
    /// The name of the hook.
    name: String,
    /// The hook function.
    hook: Box<dyn FnOnce(bool) -> anyhow::Result<()>>,
}

impl PetriPostTestHook {
    pub fn new(name: String, hook: impl FnOnce(bool) -> anyhow::Result<()> + 'static) -> Self {
        Self {
            name,
            hook: Box::new(hook),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn run(self, test_passed: bool) -> anyhow::Result<()> {
        (self.hook)(test_passed)
    }
}

/// Returns a hook that, if the test failed, writes the Windows Error Reporting
/// and Azure Watson events from the test's execution window to
/// `watson_events.log`.
#[cfg(windows)]
fn collect_watson_events_hook(logger: PetriLogSource) -> PetriPostTestHook {
    let start_time = jiff::Timestamp::now();
    PetriPostTestHook::new("collect watson events".into(), move |test_passed| {
        if test_passed {
            return Ok(());
        }
        let events =
            futures::executor::block_on(crate::vm::hyperv::powershell::watson_events(&start_time));
        if events.is_empty() {
            return Ok(());
        }
        let log_file = logger.log_file("watson_events")?;
        for event in events {
            event.write_to(&log_file);
        }
        Ok(())
    })
}

/// A test defined by an artifact resolver function and a run function.
pub struct SimpleTest<A, F> {
    leaf_name: &'static str,
    resolve: A,
    run: F,
    /// Optional test requirements
    pub host_requirements: Option<TestCaseRequirements>,
    unstable: Option<&'static str>,
    ignored: bool,
    remote_policy: RemoteAccess,
}

impl<A, AR, F, E> SimpleTest<A, F>
where
    A: 'static + Send + Fn(&ArtifactResolver<'_>) -> Option<AR>,
    F: 'static + Send + AsyncFn(PetriTestParams<'_>, DefaultDriver, AR) -> Result<(), E>,
    E: Into<anyhow::Error>,
{
    /// Returns a new test with the given `leaf_name`, `resolve`, and `run`
    /// functions, whose `run` function is async, running it on a task
    /// pool owned by petri.
    ///
    /// The test defaults to stable, not ignored, with no host requirements and
    /// a [`RemoteAccess::LocalOnly`] policy. Use the builder methods
    /// ([`requirements`](Self::requirements), [`unstable`](Self::unstable),
    /// [`ignore`](Self::ignore), [`remote_access`](Self::remote_access)) to
    /// override these.
    pub fn new_async(
        leaf_name: &'static str,
        resolve: A,
        run: F,
    ) -> SimpleTest<A, impl 'static + Send + Fn(PetriTestParams<'_>, AR) -> Result<(), E>> {
        SimpleTest::new_sync(leaf_name, resolve, move |params, artifacts| {
            let mut pool = DefaultPool::named(std::thread::current().name().unwrap_or(leaf_name));
            let driver = pool.driver();
            // The inner catch keeps a panicking test body from unwinding through
            // the pool; the outer one catches panics raised by spawned tasks,
            // such as the VM's timeout watchdog.
            let r = catch_unwind(AssertUnwindSafe(|| {
                pool.run_until(
                    AssertUnwindSafe(run(params, driver.clone(), artifacts)).catch_unwind(),
                )
            }));
            // Let the diagnostic tasks the VM spawns as it is dropped finish
            // before the failure is reported.
            drop(driver);
            pool.run();
            match r.and_then(|r| r) {
                Ok(r) => r,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
    }
}

impl<A, AR, F, E> SimpleTest<A, F>
where
    A: 'static + Send + Fn(&ArtifactResolver<'_>) -> Option<AR>,
    F: 'static + Send + Fn(PetriTestParams<'_>, AR) -> Result<(), E>,
    E: Into<anyhow::Error>,
{
    /// Returns a new test with the given `leaf_name`, `resolve`, and `run`
    /// functions.
    ///
    /// Use [`new_async`](Self::new_async) instead if the test constructs a
    /// [`PetriVm`](crate::PetriVm); its diagnostics on failure rely on petri
    /// owning the task pool.
    ///
    /// The test defaults to stable, not ignored, with no host requirements and
    /// a [`RemoteAccess::LocalOnly`] policy. Use the builder methods
    /// ([`requirements`](Self::requirements), [`unstable`](Self::unstable),
    /// [`ignore`](Self::ignore), [`remote_access`](Self::remote_access)) to
    /// override these.
    pub fn new_sync(leaf_name: &'static str, resolve: A, run: F) -> Self {
        SimpleTest {
            leaf_name,
            resolve,
            run,
            host_requirements: None,
            unstable: None,
            ignored: false,
            remote_policy: RemoteAccess::LocalOnly,
        }
    }
}

impl<A, F> SimpleTest<A, F> {
    /// Sets the host requirements that must be satisfied for this test to run.
    pub fn requirements(mut self, requirements: TestCaseRequirements) -> Self {
        self.host_requirements = Some(requirements);
        self
    }

    /// Marks this test as unstable. When `PETRI_IGNORE_UNSTABLE_FAILURES` is
    /// set (as it is in CI), a failure of this test is logged and ignored
    /// rather than failing the run; otherwise it fails like any other test.
    ///
    /// `reason` documents why the test is unstable and is logged when an
    /// unstable failure is ignored.
    pub fn unstable(mut self, reason: &'static str) -> Self {
        self.unstable = Some(reason);
        self
    }

    /// Marks this test as ignored: it is skipped by default and only runs when
    /// explicitly requested (like a libtest `#[ignore]` test).
    pub fn ignore(mut self) -> Self {
        self.ignored = true;
        self
    }

    /// Sets the remote-access policy used when resolving artifacts.
    pub fn remote_access(mut self, policy: RemoteAccess) -> Self {
        self.remote_policy = policy;
        self
    }
}

impl<A, AR, F, E> RunTest for SimpleTest<A, F>
where
    A: 'static + Send + Fn(&ArtifactResolver<'_>) -> Option<AR>,
    F: 'static + Send + Fn(PetriTestParams<'_>, AR) -> Result<(), E>,
    E: Into<anyhow::Error>,
{
    type Artifacts = AR;

    fn leaf_name(&self) -> &str {
        self.leaf_name
    }

    fn resolve(&self, mut resolver: ArtifactResolver<'_>) -> Option<Self::Artifacts> {
        resolver.set_remote_policy(self.remote_policy);
        (self.resolve)(&resolver)
    }

    fn run(&self, params: PetriTestParams<'_>, artifacts: Self::Artifacts) -> anyhow::Result<()> {
        (self.run)(params, artifacts).map_err(Into::into)
    }

    fn host_requirements(&self) -> Option<&TestCaseRequirements> {
        self.host_requirements.as_ref()
    }

    fn unstable(&self) -> Option<&str> {
        self.unstable
    }

    fn ignored(&self) -> bool {
        self.ignored
    }
}

#[derive(clap::Parser)]
struct Options {
    /// Lists the required artifacts for all tests in JSON format.
    /// Use --tests-from-stdin to query artifacts for specific tests.
    #[clap(long)]
    list_required_artifacts: bool,
    /// When used with --list-required-artifacts, read exact test names from
    /// stdin (one per line) to query artifacts for specific tests only.
    ///
    /// Even though users can use nextest's filter logic to run a subset of
    /// tests, due to nextest's architecture of running one test per binary we
    /// cannot accept a nextest filter here directly. Instead, vmm-tests-run
    /// must first call nextest with the desired filter to determine the exact
    /// test names to pass via stdin, then ask petri what artifacts are required
    /// for those tests.
    #[clap(long, requires = "list_required_artifacts")]
    tests_from_stdin: bool,
    #[clap(flatten)]
    inner: libtest_mimic::Arguments,
}

/// Entry point for test binaries.
pub fn test_main(
    resolve: fn(&str, TestArtifactRequirements) -> anyhow::Result<TestArtifacts>,
) -> ! {
    let mut args = <Options as clap::Parser>::parse();
    if args.list_required_artifacts {
        use std::collections::BTreeSet;

        // Collect all artifacts from tests (all tests, or those specified via stdin)
        let mut required_set = BTreeSet::new();
        let mut optional_set = BTreeSet::new();

        // If reading test names from stdin, collect them into a set for exact matching
        let stdin_tests: Option<BTreeSet<String>> = if args.tests_from_stdin {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let tests: BTreeSet<String> = stdin
                .lock()
                .lines()
                .map_while(Result::ok)
                .filter(|line| !line.is_empty())
                .collect();
            if tests.is_empty() {
                eprintln!("warning: no test names provided on stdin");
            }
            Some(tests)
        } else {
            None
        };

        for test in Test::all() {
            let name = test.name();

            // If reading from stdin, do exact matching; otherwise include all tests
            let matches = match stdin_tests {
                Some(ref stdin_tests) => stdin_tests.contains(&name),
                None => true,
            };

            if matches {
                for artifact in test.artifact_requirements.required_artifacts() {
                    required_set.insert(artifact.global_unique_id());
                }
                for artifact in test.artifact_requirements.optional_artifacts() {
                    optional_set.insert(artifact.global_unique_id());
                }
            }
        }

        // Remove from optional any artifacts that are required
        let optional_set: BTreeSet<_> = optional_set.difference(&required_set).cloned().collect();

        let output = petri_artifacts_core::ArtifactListOutput {
            required: required_set.into_iter().collect(),
            optional: optional_set.into_iter().collect(),
        };

        println!(
            "{}",
            serde_json::to_string(&output).expect("JSON serialization failed")
        );
        std::process::exit(0);
    }

    // Always just use one thread to avoid interleaving logs and to avoid using
    // too many resources. These tests are usually run under nextest, which will
    // run them in parallel in separate processes with appropriate concurrency
    // limits.
    if !matches!(args.inner.test_threads, None | Some(1)) {
        eprintln!("warning: ignoring value passed to --test-threads, using 1");
    }
    args.inner.test_threads = Some(1);

    // Create the host context once to avoid repeated expensive queries
    let host_context = futures::executor::block_on(HostContext::new());

    let trials = Test::all()
        .map(|test| {
            let can_run = can_run_test_with_context(test.test.0.host_requirements(), &host_context);
            let ignored = test.test.0.ignored();
            test.trial(resolve).with_ignored_flag(!can_run || ignored)
        })
        .collect();

    libtest_mimic::run(&args.inner, trials).exit();
}
