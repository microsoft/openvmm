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
use petri_artifacts_core::ArtifactResolver;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use test_macro_support::TESTS;

/// Defines a single test from a value that implements [`RunTest`].
#[macro_export]
macro_rules! test {
    ($f:ident, $req:expr) => {
        $crate::multitest!(vec![
            $crate::SimpleTest::new(stringify!($f), $req, $f, None,).into()
        ]);
    };
}

/// Defines a set of tests from a [`TestCase`].
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
                artifact_requirements
                    .require(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY);
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
        let logger = try_init_tracing(output_dir).context("failed to initialize tracing")?;
        let mut post_test_hooks = Vec::new();

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
        logger.log_test_result(&name, &r);

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
            self.run(resolve).map_err(|err| format!("{err:#}").into())
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
    fn resolve(&self, resolver: &ArtifactResolver<'_>) -> Option<Self::Artifacts>;
    /// Runs the test, which has been assigned `name`, with the given
    /// `artifacts`.
    fn run(&self, params: PetriTestParams<'_>, artifacts: Self::Artifacts) -> anyhow::Result<()>;
    /// Returns the host requirements of the current test, if any.
    fn host_requirements(&self) -> Option<&TestCaseRequirements>;
}

trait DynRunTest: Send {
    fn leaf_name(&self) -> &str;
    fn artifact_requirements(&self) -> Option<TestArtifactRequirements>;
    fn run(&self, params: PetriTestParams<'_>, artifacts: &TestArtifacts) -> anyhow::Result<()>;
    fn host_requirements(&self) -> Option<&TestCaseRequirements>;
}

impl<T: RunTest> DynRunTest for T {
    fn leaf_name(&self) -> &str {
        self.leaf_name()
    }

    fn artifact_requirements(&self) -> Option<TestArtifactRequirements> {
        let mut requirements = TestArtifactRequirements::new();
        self.resolve(&ArtifactResolver::collector(&mut requirements))?;
        Some(requirements)
    }

    fn run(&self, params: PetriTestParams<'_>, artifacts: &TestArtifacts) -> anyhow::Result<()> {
        let artifacts = self
            .resolve(&ArtifactResolver::resolver(artifacts))
            .context("test should have been skipped")?;
        self.run(params, artifacts)
    }

    fn host_requirements(&self) -> Option<&TestCaseRequirements> {
        self.host_requirements()
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

/// A test defined by an artifact resolver function and a run function.
pub struct SimpleTest<A, F> {
    leaf_name: &'static str,
    resolve: A,
    run: F,
    /// Optional test requirements
    pub host_requirements: Option<TestCaseRequirements>,
}

impl<A, AR, F, E> SimpleTest<A, F>
where
    A: 'static + Send + Fn(&ArtifactResolver<'_>) -> Option<AR>,
    F: 'static + Send + Fn(PetriTestParams<'_>, AR) -> Result<(), E>,
    E: Into<anyhow::Error>,
{
    /// Returns a new test with the given `leaf_name`, `resolve`, `run` functions,
    /// and optional requirements.
    pub fn new(
        leaf_name: &'static str,
        resolve: A,
        run: F,
        host_requirements: Option<TestCaseRequirements>,
    ) -> Self {
        SimpleTest {
            leaf_name,
            resolve,
            run,
            host_requirements,
        }
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

    fn resolve(&self, resolver: &ArtifactResolver<'_>) -> Option<Self::Artifacts> {
        (self.resolve)(resolver)
    }

    fn run(&self, params: PetriTestParams<'_>, artifacts: Self::Artifacts) -> anyhow::Result<()> {
        (self.run)(params, artifacts).map_err(Into::into)
    }

    fn host_requirements(&self) -> Option<&TestCaseRequirements> {
        self.host_requirements.as_ref()
    }
}

#[derive(clap::Parser)]
struct Options {
    /// Lists the required artifacts for all tests.
    #[clap(long)]
    list_required_artifacts: bool,
    #[clap(flatten)]
    inner: libtest_mimic::Arguments,
}

/// Entry point for test binaries.
pub fn test_main(
    resolve: fn(&str, TestArtifactRequirements) -> anyhow::Result<TestArtifacts>,
) -> ! {
    let mut args = <Options as clap::Parser>::parse();
    if args.list_required_artifacts {
        // FUTURE: write this in a machine readable format.
        for test in Test::all() {
            println!("{}:", test.name());
            for artifact in test.artifact_requirements.required_artifacts() {
                println!("required: {artifact:?}");
            }
            for artifact in test.artifact_requirements.optional_artifacts() {
                println!("optional: {artifact:?}");
            }
            println!();
        }
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
            test.trial(resolve).with_ignored_flag(!can_run)
        })
        .collect();

    libtest_mimic::run(&args.inner, trials).exit();
}
