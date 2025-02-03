// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for defining tests.

use petri::TestArtifactRequirements;
use petri::TestArtifacts;

#[linkme::distributed_slice]
pub(crate) static TESTS: [fn() -> (&'static str, Vec<Box<dyn RunTest>>)] = [..];

/// Defines a single test from a value that implements [`RunTest`].
macro_rules! test {
    ($test:expr) => {
        $crate::multitest!(vec![Box::new($test)]);
    };
}
pub(crate) use test;

/// Defines a set of tests from a [`Vec<Box<dyn RunTest>>`].
macro_rules! multitest {
    ($tests:expr) => {
        const _: () = {
            // UNSAFETY: linkme uses manual link sections, which are unsafe.
            #[expect(unsafe_code)]
            #[linkme::distributed_slice($crate::test::TESTS)]
            static TEST: fn() -> (&'static str, Vec<Box<dyn $crate::test::RunTest>>) =
                || (module_path!(), $tests);
        };
    };
}
pub(crate) use multitest;

/// A single test.
pub struct Test {
    module: &'static str,
    test: Box<dyn RunTest>,
}

impl Test {
    /// Returns all the tests defined in this crate.
    pub(crate) fn all() -> impl Iterator<Item = Self> {
        TESTS.iter().flat_map(|f| {
            let (module, tests) = f();
            tests.into_iter().map(move |test| Self { module, test })
        })
    }

    /// Returns the name of the test.
    pub fn name(&self) -> String {
        let crate_name = module_path!().split("::").next().unwrap();
        // Prefix the module where the test was defined, but strip the crate
        // name for consistency with libtest.
        format!(
            "{}::{}",
            self.module
                .strip_prefix(crate_name)
                .unwrap()
                .strip_prefix("::")
                .unwrap(),
            self.test.leaf_name()
        )
    }

    /// Returns the artifact requirements for the test.
    pub fn requirements(&self) -> TestArtifactRequirements {
        // All tests require the log directory.
        self.test
            .requirements()
            .require(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY)
    }

    /// Returns a libtest-mimic trial to run the test.
    pub fn trial(
        self,
        resolve: fn(&str, TestArtifactRequirements) -> anyhow::Result<TestArtifacts>,
    ) -> libtest_mimic::Trial {
        libtest_mimic::Trial::test(self.name(), move || {
            let name = self.name();
            let artifacts = resolve(&name, self.requirements())
                .map_err(|err| format!("failed to resolve artifacts: {:#}", err))?;
            self.test.run(&name, &artifacts)
        })
    }
}

/// A test that can be run.
///
/// Register it to be run with [`test!`] or [`multitest!`].
pub(crate) trait RunTest: Send {
    /// The leaf name of the test.
    ///
    /// To produce the full test name, this will be prefixed with the module
    /// name where the test is defined.
    fn leaf_name(&self) -> &str;
    /// Returns the artifacts required by the test.
    fn requirements(&self) -> TestArtifactRequirements;
    /// Runs the test, which has been assigned `name`, with the given
    /// `artifacts`.
    fn run(&self, name: &str, artifacts: &TestArtifacts) -> Result<(), libtest_mimic::Failed>;
}

/// A test defined by a fixed set of requirements and a run function.
pub(crate) struct SimpleTest<F> {
    leaf_name: &'static str,
    requirements: TestArtifactRequirements,
    run: F,
}

impl<F, E> SimpleTest<F>
where
    F: 'static + Send + Fn(&str, &TestArtifacts) -> Result<(), E>,
    E: Into<anyhow::Error>,
{
    /// Returns a new test with the given `leaf_name`, `requirements`, and `run`
    /// functions.
    pub fn new(leaf_name: &'static str, requirements: TestArtifactRequirements, run: F) -> Self {
        SimpleTest {
            leaf_name,
            requirements,
            run,
        }
    }
}

impl<F, E> RunTest for SimpleTest<F>
where
    F: 'static + Send + Fn(&str, &TestArtifacts) -> Result<(), E>,
    E: Into<anyhow::Error>,
{
    fn leaf_name(&self) -> &str {
        self.leaf_name
    }

    fn requirements(&self) -> TestArtifactRequirements {
        self.requirements.clone()
    }

    fn run(&self, name: &str, artifacts: &TestArtifacts) -> Result<(), libtest_mimic::Failed> {
        (self.run)(name, artifacts).map_err(|err| format!("{:#}", err.into()))?;
        Ok(())
    }
}
