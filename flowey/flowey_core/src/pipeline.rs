// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core types and traits used to create and work with flowey pipelines.

mod artifact;

pub use artifact::Artifact;

use self::internal::*;
use crate::node::FlowArch;
use crate::node::FlowNodeBase;
use crate::node::FlowPlatform;
use crate::node::FlowPlatformLinuxDistro;
use crate::node::GhUserSecretVar;
use crate::node::IntoRequest;
use crate::node::NodeHandle;
use crate::node::ReadVar;
use crate::node::WriteVar;
use crate::node::steps::ado::AdoResourcesRepositoryId;
use crate::node::user_facing::AdoRuntimeVar;
use crate::node::user_facing::GhPermission;
use crate::node::user_facing::GhPermissionValue;
use crate::patch::PatchResolver;
use crate::patch::ResolvedPatches;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Pipeline types which are considered "user facing", and included in the
/// `flowey` prelude.
pub mod user_facing {
    pub use super::AdoCiTriggers;
    pub use super::AdoPrTriggers;
    pub use super::AdoResourcesRepository;
    pub use super::AdoResourcesRepositoryRef;
    pub use super::AdoResourcesRepositoryType;
    pub use super::AdoScheduleTriggers;
    pub use super::GhCiTriggers;
    pub use super::GhPrTriggers;
    pub use super::GhRunner;
    pub use super::GhRunnerOsLabel;
    pub use super::GhScheduleTriggers;
    pub use super::HostExt;
    pub use super::IntoPipeline;
    pub use super::ParameterKind;
    pub use super::Pipeline;
    pub use super::PipelineBackendHint;
    pub use super::PipelineJob;
    pub use super::PipelineJobCtx;
    pub use super::PipelineJobHandle;
    pub use super::PublishArtifact;
    pub use super::PublishTypedArtifact;
    pub use super::UseArtifact;
    pub use super::UseParameter;
    pub use super::UseTypedArtifact;
    pub use crate::node::FlowArch;
    pub use crate::node::FlowPlatform;
}

fn linux_distro() -> FlowPlatformLinuxDistro {
    // Check for Nix environment first
    // USING_NIX=1 is set in CI, IN_NIX_SHELL is set when inside nix-shell
    if std::env::var("USING_NIX")
        .map(|v| v == "1")
        .unwrap_or(false)
        || std::env::var("IN_NIX_SHELL").is_ok()
    {
        return FlowPlatformLinuxDistro::Nix;
    }

    // A `nix develop` shell doesn't set `IN_NIX_SHELL`, but the PATH should include a nix store path
    if std::env::var("PATH").is_ok_and(|path| path.contains("/nix/store")) {
        return FlowPlatformLinuxDistro::Nix;
    }

    if let Ok(etc_os_release) = fs_err::read_to_string("/etc/os-release") {
        if etc_os_release.contains("ID=ubuntu") {
            FlowPlatformLinuxDistro::Ubuntu
        } else if etc_os_release.contains("ID=fedora") {
            FlowPlatformLinuxDistro::Fedora
        } else if etc_os_release.contains("ID=arch") {
            FlowPlatformLinuxDistro::Arch
        } else {
            FlowPlatformLinuxDistro::Unknown
        }
    } else {
        FlowPlatformLinuxDistro::Unknown
    }
}

pub trait HostExt: Sized {
    /// Return the value for the current host machine.
    ///
    /// Will panic on non-local backends.
    fn host(backend_hint: PipelineBackendHint) -> Self;
}

impl HostExt for FlowPlatform {
    /// Return the platform of the current host machine.
    ///
    /// Will panic on non-local backends.
    fn host(backend_hint: PipelineBackendHint) -> Self {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            panic!("can only use `FlowPlatform::host` when defining a local-only pipeline");
        }

        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux(linux_distro())
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            panic!("no valid host-os")
        }
    }
}

impl HostExt for FlowArch {
    /// Return the arch of the current host machine.
    ///
    /// Will panic on non-local backends.
    fn host(backend_hint: PipelineBackendHint) -> Self {
        if !matches!(backend_hint, PipelineBackendHint::Local) {
            panic!("can only use `FlowArch::host` when defining a local-only pipeline");
        }

        // xtask-fmt allow-target-arch oneoff-flowey
        if cfg!(target_arch = "x86_64") {
            Self::X86_64
        // xtask-fmt allow-target-arch oneoff-flowey
        } else if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else {
            panic!("no valid host-arch")
        }
    }
}

/// Trigger ADO pipelines via Continuous Integration
#[derive(Default, Debug)]
pub struct AdoScheduleTriggers {
    /// Friendly name for the scheduled run
    pub display_name: String,
    /// Run the pipeline whenever there is a commit on these specified branches
    /// (supports glob syntax)
    pub branches: Vec<String>,
    /// Specify any branches which should be filtered out from the list of
    /// `branches` (supports glob syntax)
    pub exclude_branches: Vec<String>,
    /// Run the pipeline in a schedule, as specified by a cron string
    pub cron: String,
}

/// Trigger ADO pipelines per PR
#[derive(Debug)]
pub struct AdoPrTriggers {
    /// Run the pipeline whenever there is a PR to these specified branches
    /// (supports glob syntax)
    pub branches: Vec<String>,
    /// Specify any branches which should be filtered out from the list of
    /// `branches` (supports glob syntax)
    pub exclude_branches: Vec<String>,
    /// Run the pipeline even if the PR is a draft PR. Defaults to `false`.
    pub run_on_draft: bool,
    /// Automatically cancel the pipeline run if a new commit lands in the
    /// branch. Defaults to `true`.
    pub auto_cancel: bool,
}

/// Trigger ADO pipelines per PR
#[derive(Debug, Default)]
pub struct AdoCiTriggers {
    /// Run the pipeline whenever there is a change to these specified branches
    /// (supports glob syntax)
    pub branches: Vec<String>,
    /// Specify any branches which should be filtered out from the list of
    /// `branches` (supports glob syntax)
    pub exclude_branches: Vec<String>,
    /// Run the pipeline whenever a matching tag is created (supports glob
    /// syntax)
    pub tags: Vec<String>,
    /// Specify any tags which should be filtered out from the list of `tags`
    /// (supports glob syntax)
    pub exclude_tags: Vec<String>,
    /// Whether to batch changes per branch.
    pub batch: bool,
}

impl Default for AdoPrTriggers {
    fn default() -> Self {
        Self {
            branches: Vec::new(),
            exclude_branches: Vec::new(),
            run_on_draft: false,
            auto_cancel: true,
        }
    }
}

/// ADO repository resource.
#[derive(Debug)]
pub struct AdoResourcesRepository {
    /// Type of repo that is being connected to.
    pub repo_type: AdoResourcesRepositoryType,
    /// Repository name. Format depends on `repo_type`.
    pub name: String,
    /// git ref to checkout.
    pub git_ref: AdoResourcesRepositoryRef,
    /// (optional) ID of the service endpoint connecting to this repository.
    pub endpoint: Option<String>,
}

/// ADO repository resource type
#[derive(Debug)]
pub enum AdoResourcesRepositoryType {
    /// Azure Repos Git repository
    AzureReposGit,
    /// Github repository
    GitHub,
}

/// ADO repository ref
#[derive(Debug)]
pub enum AdoResourcesRepositoryRef<P = UseParameter<String>> {
    /// Hard-coded ref (e.g: refs/heads/main)
    Fixed(String),
    /// Connected to pipeline-level parameter
    Parameter(P),
}

/// Trigger Github Actions pipelines via Continuous Integration
///
/// NOTE: Github Actions doesn't support specifying the branch when triggered by `schedule`.
/// To run on a specific branch, modify the branch checked out in the pipeline.
#[derive(Default, Debug)]
pub struct GhScheduleTriggers {
    /// Run the pipeline in a schedule, as specified by a cron string
    pub cron: String,
}

/// Trigger Github Actions pipelines per PR
#[derive(Debug)]
pub struct GhPrTriggers {
    /// Run the pipeline whenever there is a PR to these specified branches
    /// (supports glob syntax)
    pub branches: Vec<String>,
    /// Specify any branches which should be filtered out from the list of
    /// `branches` (supports glob syntax)
    pub exclude_branches: Vec<String>,
    /// Automatically cancel the pipeline run if a new commit lands in the
    /// branch. Defaults to `true`.
    pub auto_cancel: bool,
    /// Run the pipeline whenever the PR trigger matches the specified types
    pub types: Vec<String>,
}

/// Trigger Github Actions pipelines per PR
#[derive(Debug, Default)]
pub struct GhCiTriggers {
    /// Run the pipeline whenever there is a change to these specified branches
    /// (supports glob syntax)
    pub branches: Vec<String>,
    /// Specify any branches which should be filtered out from the list of
    /// `branches` (supports glob syntax)
    pub exclude_branches: Vec<String>,
    /// Run the pipeline whenever a matching tag is created (supports glob
    /// syntax)
    pub tags: Vec<String>,
    /// Specify any tags which should be filtered out from the list of `tags`
    /// (supports glob syntax)
    pub exclude_tags: Vec<String>,
}

impl GhPrTriggers {
    /// Triggers the pipeline on the default PR events plus when a draft is marked as ready for review.
    pub fn new_draftable() -> Self {
        Self {
            branches: Vec::new(),
            exclude_branches: Vec::new(),
            types: vec![
                "opened".into(),
                "synchronize".into(),
                "reopened".into(),
                "ready_for_review".into(),
            ],
            auto_cancel: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GhRunnerOsLabel {
    UbuntuLatest,
    Ubuntu2404,
    Ubuntu2204,
    WindowsLatest,
    Windows2025,
    Windows2022,
    Ubuntu2404Arm,
    Ubuntu2204Arm,
    Windows11Arm,
    Custom(String),
}

/// GitHub runner type
#[derive(Debug, Clone, PartialEq)]
pub enum GhRunner {
    // See <https://docs.github.com/en/actions/using-workflows/workflow-syntax-for-github-actions#choosing-github-hosted-runners>
    // for more details.
    GhHosted(GhRunnerOsLabel),
    // Self hosted runners are selected by matching runner labels to <labels>.
    // 'self-hosted' is a common label for self hosted runners, but is not required.
    // Labels are case-insensitive and can take the form of arbitrary strings.
    // See <https://docs.github.com/en/actions/hosting-your-own-runners> for more details.
    SelfHosted(Vec<String>),
    // This uses a runner belonging to <group> that matches all labels in <labels>.
    // See <https://docs.github.com/en/actions/using-workflows/workflow-syntax-for-github-actions#choosing-github-hosted-runners>
    // for more details.
    RunnerGroup { group: String, labels: Vec<String> },
}

impl GhRunner {
    /// Whether this is a self-hosted runner with the provided label
    pub fn is_self_hosted_with_label(&self, label: &str) -> bool {
        matches!(self, GhRunner::SelfHosted(labels) if labels.iter().any(|s| s.as_str() == label))
    }
}

/// Parameter type (unstable / stable).
#[derive(Debug, Clone)]
pub enum ParameterKind {
    // The parameter is considered an unstable API, and should not be
    // taken as a dependency.
    Unstable,
    // The parameter is considered a stable API, and can be used by
    // external pipelines to control behavior of the pipeline.
    Stable,
}

#[derive(Clone, Debug)]
#[must_use]
pub struct UseParameter<T> {
    idx: usize,
    _kind: std::marker::PhantomData<T>,
}

/// Opaque handle to an artifact which must be published by a single job.
#[must_use]
pub struct PublishArtifact {
    idx: usize,
}

/// Opaque handle to an artifact which can be used by one or more jobs.
#[derive(Clone)]
#[must_use]
pub struct UseArtifact {
    idx: usize,
}

/// Opaque handle to an artifact of type `T` which must be published by a single job.
#[must_use]
pub struct PublishTypedArtifact<T>(PublishArtifact, std::marker::PhantomData<fn() -> T>);

/// Opaque handle to an artifact of type `T` which can be used by one or more
/// jobs.
#[must_use]
pub struct UseTypedArtifact<T>(UseArtifact, std::marker::PhantomData<fn(T)>);

impl<T> Clone for UseTypedArtifact<T> {
    fn clone(&self) -> Self {
        UseTypedArtifact(self.0.clone(), std::marker::PhantomData)
    }
}

#[derive(Default)]
pub struct Pipeline {
    jobs: Vec<PipelineJobMetadata>,
    artifacts: Vec<ArtifactMeta>,
    parameters: Vec<ParameterMeta>,
    extra_deps: BTreeSet<(usize, usize)>,
    // builder internal
    artifact_names: BTreeSet<String>,
    dummy_done_idx: usize,
    artifact_map_idx: usize,
    global_patchfns: Vec<crate::patch::PatchFn>,
    inject_all_jobs_with: Option<Box<dyn for<'a> Fn(PipelineJob<'a>) -> PipelineJob<'a>>>,
    // backend specific
    ado_name: Option<String>,
    ado_job_id_overrides: BTreeMap<usize, String>,
    ado_schedule_triggers: Vec<AdoScheduleTriggers>,
    ado_ci_triggers: Option<AdoCiTriggers>,
    ado_pr_triggers: Option<AdoPrTriggers>,
    ado_resources_repository: Vec<InternalAdoResourcesRepository>,
    ado_bootstrap_template: String,
    ado_variables: BTreeMap<String, String>,
    ado_post_process_yaml_cb: Option<Box<dyn FnOnce(serde_yaml::Value) -> serde_yaml::Value>>,
    gh_name: Option<String>,
    gh_schedule_triggers: Vec<GhScheduleTriggers>,
    gh_ci_triggers: Option<GhCiTriggers>,
    gh_pr_triggers: Option<GhPrTriggers>,
    gh_bootstrap_template: String,
}

impl Pipeline {
    pub fn new() -> Pipeline {
        Pipeline::default()
    }

    /// Inject all pipeline jobs with some common logic. (e.g: to resolve common
    /// configuration requirements shared by all jobs).
    ///
    /// Can only be invoked once per pipeline.
    #[track_caller]
    pub fn inject_all_jobs_with(
        &mut self,
        cb: impl for<'a> Fn(PipelineJob<'a>) -> PipelineJob<'a> + 'static,
    ) -> &mut Self {
        if self.inject_all_jobs_with.is_some() {
            panic!("can only call inject_all_jobs_with once!")
        }
        self.inject_all_jobs_with = Some(Box::new(cb));
        self
    }

    /// (ADO only) Provide a YAML template used to bootstrap flowey at the start
    /// of an ADO pipeline.
    ///
    /// The template has access to the following vars, which will be statically
    /// interpolated into the template's text:
    ///
    /// - `{{FLOWEY_OUTDIR}}`
    ///     - Directory to copy artifacts into.
    ///     - NOTE: this var will include `\` on Windows, and `/` on linux!
    /// - `{{FLOWEY_BIN_EXTENSION}}`
    ///     - Extension of the expected flowey bin (either "", or ".exe")
    /// - `{{FLOWEY_CRATE}}`
    ///     - Name of the project-specific flowey crate to be built
    /// - `{{FLOWEY_TARGET}}`
    ///     - The target-triple flowey is being built for
    /// - `{{FLOWEY_PIPELINE_PATH}}`
    ///     - Repo-root relative path to the pipeline (as provided when
    ///       generating the pipeline via the flowey CLI)
    ///
    /// The template's sole responsibility is to copy 3 files into the
    /// `{{FLOWEY_OUTDIR}}`:
    ///
    /// 1. The bootstrapped flowey binary, with the file name
    ///    `flowey{{FLOWEY_BIN_EXTENSION}}`
    /// 2. Two files called `pipeline.yaml` and `pipeline.json`, which are
    ///    copied of the pipeline YAML and pipeline JSON currently being run.
    ///    `{{FLOWEY_PIPELINE_PATH}}` is provided as a way to disambiguate in
    ///    cases where the same template is being for multiple pipelines (e.g: a
    ///    debug vs. release pipeline).
    pub fn ado_set_flowey_bootstrap_template(&mut self, template: String) -> &mut Self {
        self.ado_bootstrap_template = template;
        self
    }

    /// (ADO only) Provide a callback function which will be used to
    /// post-process any YAML flowey generates for the pipeline.
    ///
    /// Until flowey defines a stable API for maintaining out-of-tree backends,
    /// this method can be used to integrate the output from the generic ADO
    /// backend with any organization-specific templates that one may be
    /// required to use (e.g: for compliance reasons).
    pub fn ado_post_process_yaml(
        &mut self,
        cb: impl FnOnce(serde_yaml::Value) -> serde_yaml::Value + 'static,
    ) -> &mut Self {
        self.ado_post_process_yaml_cb = Some(Box::new(cb));
        self
    }

    /// (ADO only) Add a new scheduled CI trigger. Can be called multiple times
    /// to set up multiple schedules runs.
    pub fn ado_add_schedule_trigger(&mut self, triggers: AdoScheduleTriggers) -> &mut Self {
        self.ado_schedule_triggers.push(triggers);
        self
    }

    /// (ADO only) Set a PR trigger. Calling this method multiple times will
    /// overwrite any previously set triggers.
    pub fn ado_set_pr_triggers(&mut self, triggers: AdoPrTriggers) -> &mut Self {
        self.ado_pr_triggers = Some(triggers);
        self
    }

    /// (ADO only) Set a CI trigger. Calling this method multiple times will
    /// overwrite any previously set triggers.
    pub fn ado_set_ci_triggers(&mut self, triggers: AdoCiTriggers) -> &mut Self {
        self.ado_ci_triggers = Some(triggers);
        self
    }

    /// (ADO only) Declare a new repository resource, returning a type-safe
    /// handle which downstream ADO steps are able to consume via
    /// [`AdoStepServices::resolve_repository_id`](crate::node::user_facing::AdoStepServices::resolve_repository_id).
    pub fn ado_add_resources_repository(
        &mut self,
        repo: AdoResourcesRepository,
    ) -> AdoResourcesRepositoryId {
        let AdoResourcesRepository {
            repo_type,
            name,
            git_ref,
            endpoint,
        } = repo;

        let repo_id = format!("repo{}", self.ado_resources_repository.len());

        self.ado_resources_repository
            .push(InternalAdoResourcesRepository {
                repo_id: repo_id.clone(),
                repo_type,
                name,
                git_ref: match git_ref {
                    AdoResourcesRepositoryRef::Fixed(s) => AdoResourcesRepositoryRef::Fixed(s),
                    AdoResourcesRepositoryRef::Parameter(p) => {
                        AdoResourcesRepositoryRef::Parameter(p.idx)
                    }
                },
                endpoint,
            });
        AdoResourcesRepositoryId { repo_id }
    }

    /// (GitHub Actions only) Set the pipeline-level name.
    ///
    /// <https://docs.github.com/en/actions/writing-workflows/workflow-syntax-for-github-actions#name>
    pub fn gh_set_name(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.gh_name = Some(name.as_ref().into());
        self
    }

    /// Provide a YAML template used to bootstrap flowey at the start of an GitHub
    /// pipeline.
    ///
    /// The template has access to the following vars, which will be statically
    /// interpolated into the template's text:
    ///
    /// - `{{FLOWEY_OUTDIR}}`
    ///     - Directory to copy artifacts into.
    ///     - NOTE: this var will include `\` on Windows, and `/` on linux!
    /// - `{{FLOWEY_BIN_EXTENSION}}`
    ///     - Extension of the expected flowey bin (either "", or ".exe")
    /// - `{{FLOWEY_CRATE}}`
    ///     - Name of the project-specific flowey crate to be built
    /// - `{{FLOWEY_TARGET}}`
    ///     - The target-triple flowey is being built for
    /// - `{{FLOWEY_PIPELINE_PATH}}`
    ///     - Repo-root relative path to the pipeline (as provided when
    ///       generating the pipeline via the flowey CLI)
    ///
    /// The template's sole responsibility is to copy 3 files into the
    /// `{{FLOWEY_OUTDIR}}`:
    ///
    /// 1. The bootstrapped flowey binary, with the file name
    ///    `flowey{{FLOWEY_BIN_EXTENSION}}`
    /// 2. Two files called `pipeline.yaml` and `pipeline.json`, which are
    ///    copied of the pipeline YAML and pipeline JSON currently being run.
    ///    `{{FLOWEY_PIPELINE_PATH}}` is provided as a way to disambiguate in
    ///    cases where the same template is being for multiple pipelines (e.g: a
    ///    debug vs. release pipeline).
    pub fn gh_set_flowey_bootstrap_template(&mut self, template: String) -> &mut Self {
        self.gh_bootstrap_template = template;
        self
    }

    /// (GitHub Actions only) Add a new scheduled CI trigger. Can be called multiple times
    /// to set up multiple schedules runs.
    pub fn gh_add_schedule_trigger(&mut self, triggers: GhScheduleTriggers) -> &mut Self {
        self.gh_schedule_triggers.push(triggers);
        self
    }

    /// (GitHub Actions only) Set a PR trigger. Calling this method multiple times will
    /// overwrite any previously set triggers.
    pub fn gh_set_pr_triggers(&mut self, triggers: GhPrTriggers) -> &mut Self {
        self.gh_pr_triggers = Some(triggers);
        self
    }

    /// (GitHub Actions only) Set a CI trigger. Calling this method multiple times will
    /// overwrite any previously set triggers.
    pub fn gh_set_ci_triggers(&mut self, triggers: GhCiTriggers) -> &mut Self {
        self.gh_ci_triggers = Some(triggers);
        self
    }

    /// (GitHub Actions only) Use a pre-defined GitHub Actions secret variable.
    ///
    /// For more information on defining secrets for use in GitHub Actions, see
    /// <https://docs.github.com/en/actions/security-guides/using-secrets-in-github-actions>
    pub fn gh_use_secret(&mut self, secret_name: impl AsRef<str>) -> GhUserSecretVar {
        GhUserSecretVar(secret_name.as_ref().to_string())
    }

    pub fn new_job(
        &mut self,
        platform: FlowPlatform,
        arch: FlowArch,
        label: impl AsRef<str>,
    ) -> PipelineJob<'_> {
        let idx = self.jobs.len();
        self.jobs.push(PipelineJobMetadata {
            root_nodes: BTreeMap::new(),
            patches: ResolvedPatches::build(),
            label: label.as_ref().into(),
            platform,
            arch,
            cond_param_idx: None,
            timeout_minutes: None,
            ado_pool: None,
            ado_variables: BTreeMap::new(),
            gh_override_if: None,
            gh_global_env: BTreeMap::new(),
            gh_pool: None,
            gh_permissions: BTreeMap::new(),
        });

        PipelineJob {
            pipeline: self,
            job_idx: idx,
        }
    }

    /// Declare a dependency between two jobs that does is not a result of an
    /// artifact.
    pub fn non_artifact_dep(
        &mut self,
        job: &PipelineJobHandle,
        depends_on_job: &PipelineJobHandle,
    ) -> &mut Self {
        self.extra_deps
            .insert((depends_on_job.job_idx, job.job_idx));
        self
    }

    #[track_caller]
    pub fn new_artifact(&mut self, name: impl AsRef<str>) -> (PublishArtifact, UseArtifact) {
        let name = name.as_ref();
        let owned_name = name.to_string();

        let not_exists = self.artifact_names.insert(owned_name.clone());
        if !not_exists {
            panic!("duplicate artifact name: {}", name)
        }

        let idx = self.artifacts.len();
        self.artifacts.push(ArtifactMeta {
            name: owned_name,
            published_by_job: None,
            used_by_jobs: BTreeSet::new(),
        });

        (PublishArtifact { idx }, UseArtifact { idx })
    }

    /// Returns a pair of opaque handles to a new artifact for use across jobs
    /// in the pipeline.
    #[track_caller]
    pub fn new_typed_artifact<T: Artifact>(
        &mut self,
        name: impl AsRef<str>,
    ) -> (PublishTypedArtifact<T>, UseTypedArtifact<T>) {
        let (publish, use_artifact) = self.new_artifact(name);
        (
            PublishTypedArtifact(publish, std::marker::PhantomData),
            UseTypedArtifact(use_artifact, std::marker::PhantomData),
        )
    }

    /// (ADO only) Set the pipeline-level name.
    ///
    /// <https://learn.microsoft.com/en-us/azure/devops/pipelines/process/run-number?view=azure-devops&tabs=yaml>
    pub fn ado_add_name(&mut self, name: String) -> &mut Self {
        self.ado_name = Some(name);
        self
    }

    /// (ADO only) Declare a pipeline-level, named, read-only ADO variable.
    ///
    /// `name` and `value` are both arbitrary strings.
    ///
    /// Returns an instance of [`AdoRuntimeVar`], which, if need be, can be
    /// converted into a [`ReadVar<String>`] using
    /// [`NodeCtx::get_ado_variable`].
    ///
    /// NOTE: Unless required by some particular third-party task, it's strongly
    /// recommended to _avoid_ using this method, and to simply use
    /// [`ReadVar::from_static`] to get a obtain a static variable.
    ///
    /// [`NodeCtx::get_ado_variable`]: crate::node::NodeCtx::get_ado_variable
    pub fn ado_new_named_variable(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> AdoRuntimeVar {
        let name = name.as_ref();
        let value = value.as_ref();

        self.ado_variables.insert(name.into(), value.into());

        // safe, since we'll ensure that the global exists in the ADO backend
        AdoRuntimeVar::dangerous_from_global(name, false)
    }

    /// (ADO only) Declare multiple pipeline-level, named, read-only ADO
    /// variables at once.
    ///
    /// This is a convenience method to streamline invoking
    /// [`Self::ado_new_named_variable`] multiple times.
    ///
    /// NOTE: Unless required by some particular third-party task, it's strongly
    /// recommended to _avoid_ using this method, and to simply use
    /// [`ReadVar::from_static`] to get a obtain a static variable.
    ///
    /// DEVNOTE: In the future, this API may be updated to return a handle that
    /// will allow resolving the resulting `AdoRuntimeVar`, but for
    /// implementation expediency, this API does not currently do this. If you
    /// need to read the value of this variable at runtime, you may need to
    /// invoke [`AdoRuntimeVar::dangerous_from_global`] manually.
    ///
    /// [`NodeCtx::get_ado_variable`]: crate::node::NodeCtx::get_ado_variable
    pub fn ado_new_named_variables<K, V>(
        &mut self,
        vars: impl IntoIterator<Item = (K, V)>,
    ) -> &mut Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.ado_variables.extend(
            vars.into_iter()
                .map(|(k, v)| (k.as_ref().into(), v.as_ref().into())),
        );
        self
    }

    /// Declare a pipeline-level runtime parameter with type `bool`.
    ///
    /// To obtain a [`ReadVar<bool>`] that can be used within a node, use the
    /// [`PipelineJobCtx::use_parameter`] method.
    ///
    /// `name` is the name of the parameter.
    ///
    /// `description` is an arbitrary string, which will be be shown to users.
    ///
    /// `kind` is the type of parameter and if it should be treated as a stable
    /// external API to callers of the pipeline.
    ///
    /// `default` is the default value for the parameter. If none is provided,
    /// the parameter _must_ be specified in order for the pipeline to run.
    ///
    /// `possible_values` can be used to limit the set of valid values the
    /// parameter accepts.
    pub fn new_parameter_bool(
        &mut self,
        name: impl AsRef<str>,
        description: impl AsRef<str>,
        kind: ParameterKind,
        default: Option<bool>,
    ) -> UseParameter<bool> {
        let idx = self.parameters.len();
        let name = new_parameter_name(name, kind.clone());
        self.parameters.push(ParameterMeta {
            parameter: Parameter::Bool {
                name,
                description: description.as_ref().into(),
                kind,
                default,
            },
            used_by_jobs: BTreeSet::new(),
        });

        UseParameter {
            idx,
            _kind: std::marker::PhantomData,
        }
    }

    /// Declare a pipeline-level runtime parameter with type `i64`.
    ///
    /// To obtain a [`ReadVar<i64>`] that can be used within a node, use the
    /// [`PipelineJobCtx::use_parameter`] method.
    ///
    /// `name` is the name of the parameter.
    ///
    /// `description` is an arbitrary string, which will be be shown to users.
    ///
    /// `kind` is the type of parameter and if it should be treated as a stable
    /// external API to callers of the pipeline.
    ///
    /// `default` is the default value for the parameter. If none is provided,
    /// the parameter _must_ be specified in order for the pipeline to run.
    ///
    /// `possible_values` can be used to limit the set of valid values the
    /// parameter accepts.
    pub fn new_parameter_num(
        &mut self,
        name: impl AsRef<str>,
        description: impl AsRef<str>,
        kind: ParameterKind,
        default: Option<i64>,
        possible_values: Option<Vec<i64>>,
    ) -> UseParameter<i64> {
        let idx = self.parameters.len();
        let name = new_parameter_name(name, kind.clone());
        self.parameters.push(ParameterMeta {
            parameter: Parameter::Num {
                name,
                description: description.as_ref().into(),
                kind,
                default,
                possible_values,
            },
            used_by_jobs: BTreeSet::new(),
        });

        UseParameter {
            idx,
            _kind: std::marker::PhantomData,
        }
    }

    /// Declare a pipeline-level runtime parameter with type `String`.
    ///
    /// To obtain a [`ReadVar<String>`] that can be used within a node, use the
    /// [`PipelineJobCtx::use_parameter`] method.
    ///
    /// `name` is the name of the parameter.
    ///
    /// `description` is an arbitrary string, which will be be shown to users.
    ///
    /// `kind` is the type of parameter and if it should be treated as a stable
    /// external API to callers of the pipeline.
    ///
    /// `default` is the default value for the parameter. If none is provided,
    /// the parameter _must_ be specified in order for the pipeline to run.
    ///
    /// `possible_values` allows restricting inputs to a set of possible values.
    /// Depending on the backend, these options may be presented as a set of
    /// radio buttons, a dropdown menu, or something in that vein. If `None`,
    /// then any string is allowed.
    pub fn new_parameter_string(
        &mut self,
        name: impl AsRef<str>,
        description: impl AsRef<str>,
        kind: ParameterKind,
        default: Option<impl AsRef<str>>,
        possible_values: Option<Vec<String>>,
    ) -> UseParameter<String> {
        let idx = self.parameters.len();
        let name = new_parameter_name(name, kind.clone());
        self.parameters.push(ParameterMeta {
            parameter: Parameter::String {
                name,
                description: description.as_ref().into(),
                kind,
                default: default.map(|x| x.as_ref().into()),
                possible_values,
            },
            used_by_jobs: BTreeSet::new(),
        });

        UseParameter {
            idx,
            _kind: std::marker::PhantomData,
        }
    }
}

pub struct PipelineJobCtx<'a> {
    pipeline: &'a mut Pipeline,
    job_idx: usize,
}

impl PipelineJobCtx<'_> {
    /// Create a new `WriteVar<SideEffect>` anchored to the pipeline job.
    pub fn new_done_handle(&mut self) -> WriteVar<crate::node::SideEffect> {
        self.pipeline.dummy_done_idx += 1;
        crate::node::thin_air_write_runtime_var(format!("start{}", self.pipeline.dummy_done_idx))
    }

    /// Claim that this job will use this artifact, obtaining a path to a folder
    /// with the artifact's contents.
    pub fn use_artifact(&mut self, artifact: &UseArtifact) -> ReadVar<PathBuf> {
        self.pipeline.artifacts[artifact.idx]
            .used_by_jobs
            .insert(self.job_idx);

        crate::node::thin_air_read_runtime_var(consistent_artifact_runtime_var_name(
            &self.pipeline.artifacts[artifact.idx].name,
            true,
        ))
    }

    /// Claim that this job will publish this artifact, obtaining a path to a
    /// fresh, empty folder which will be published as the specific artifact at
    /// the end of the job.
    pub fn publish_artifact(&mut self, artifact: PublishArtifact) -> ReadVar<PathBuf> {
        let existing = self.pipeline.artifacts[artifact.idx]
            .published_by_job
            .replace(self.job_idx);
        assert!(existing.is_none()); // PublishArtifact isn't cloneable

        crate::node::thin_air_read_runtime_var(consistent_artifact_runtime_var_name(
            &self.pipeline.artifacts[artifact.idx].name,
            false,
        ))
    }

    fn helper_request<R: IntoRequest>(&mut self, req: R)
    where
        R::Node: 'static,
    {
        self.pipeline.jobs[self.job_idx]
            .root_nodes
            .entry(NodeHandle::from_type::<R::Node>())
            .or_default()
            .push(serde_json::to_vec(&req.into_request()).unwrap().into());
    }

    fn new_artifact_map_vars<T: Artifact>(&mut self) -> (ReadVar<T>, WriteVar<T>) {
        let artifact_map_idx = self.pipeline.artifact_map_idx;
        self.pipeline.artifact_map_idx += 1;

        let backing_var = format!("artifact_map{}", artifact_map_idx);
        let read_var = crate::node::thin_air_read_runtime_var(backing_var.clone());
        let write_var = crate::node::thin_air_write_runtime_var(backing_var);
        (read_var, write_var)
    }

    /// Claim that this job will use this artifact, obtaining the resolved
    /// contents of the artifact.
    pub fn use_typed_artifact<T: Artifact>(
        &mut self,
        artifact: &UseTypedArtifact<T>,
    ) -> ReadVar<T> {
        let artifact_path = self.use_artifact(&artifact.0);
        let (read, write) = self.new_artifact_map_vars::<T>();
        self.helper_request(artifact::resolve::Request::new(artifact_path, write));
        read
    }

    /// Claim that this job will publish this artifact, obtaining a variable to
    /// write the artifact's contents to. The artifact will be published at
    /// the end of the job.
    pub fn publish_typed_artifact<T: Artifact>(
        &mut self,
        artifact: PublishTypedArtifact<T>,
    ) -> WriteVar<T> {
        let artifact_path = self.publish_artifact(artifact.0);
        let (read, write) = self.new_artifact_map_vars::<T>();
        let done = self.new_done_handle();
        self.helper_request(artifact::publish::Request::new(read, artifact_path, done));
        write
    }

    /// Obtain a `ReadVar<T>` corresponding to a pipeline parameter which is
    /// specified at runtime.
    pub fn use_parameter<T>(&mut self, param: UseParameter<T>) -> ReadVar<T>
    where
        T: Serialize + DeserializeOwned,
    {
        self.pipeline.parameters[param.idx]
            .used_by_jobs
            .insert(self.job_idx);

        crate::node::thin_air_read_runtime_var(
            self.pipeline.parameters[param.idx]
                .parameter
                .name()
                .to_string(),
        )
    }

    /// Shortcut which allows defining a bool pipeline parameter within a Job.
    ///
    /// To share a single parameter between multiple jobs, don't use this method
    /// - use [`Pipeline::new_parameter_bool`] + [`Self::use_parameter`] instead.
    pub fn new_parameter_bool(
        &mut self,
        name: impl AsRef<str>,
        description: impl AsRef<str>,
        kind: ParameterKind,
        default: Option<bool>,
    ) -> ReadVar<bool> {
        let param = self
            .pipeline
            .new_parameter_bool(name, description, kind, default);
        self.use_parameter(param)
    }

    /// Shortcut which allows defining a number pipeline parameter within a Job.
    ///
    /// To share a single parameter between multiple jobs, don't use this method
    /// - use [`Pipeline::new_parameter_num`] + [`Self::use_parameter`] instead.
    pub fn new_parameter_num(
        &mut self,
        name: impl AsRef<str>,
        description: impl AsRef<str>,
        kind: ParameterKind,
        default: Option<i64>,
        possible_values: Option<Vec<i64>>,
    ) -> ReadVar<i64> {
        let param =
            self.pipeline
                .new_parameter_num(name, description, kind, default, possible_values);
        self.use_parameter(param)
    }

    /// Shortcut which allows defining a string pipeline parameter within a Job.
    ///
    /// To share a single parameter between multiple jobs, don't use this method
    /// - use [`Pipeline::new_parameter_string`] + [`Self::use_parameter`] instead.
    pub fn new_parameter_string(
        &mut self,
        name: impl AsRef<str>,
        description: impl AsRef<str>,
        kind: ParameterKind,
        default: Option<String>,
        possible_values: Option<Vec<String>>,
    ) -> ReadVar<String> {
        let param =
            self.pipeline
                .new_parameter_string(name, description, kind, default, possible_values);
        self.use_parameter(param)
    }
}

#[must_use]
pub struct PipelineJob<'a> {
    pipeline: &'a mut Pipeline,
    job_idx: usize,
}

impl PipelineJob<'_> {
    /// (ADO only) specify which agent pool this job will be run on.
    pub fn ado_set_pool(self, pool: impl AsRef<str>) -> Self {
        self.ado_set_pool_with_demands(pool, Vec::new())
    }

    /// (ADO only) specify which agent pool this job will be run on, with
    /// additional special runner demands.
    pub fn ado_set_pool_with_demands(self, pool: impl AsRef<str>, demands: Vec<String>) -> Self {
        self.pipeline.jobs[self.job_idx].ado_pool = Some(AdoPool {
            name: pool.as_ref().into(),
            demands,
        });
        self
    }

    /// (ADO only) Declare a job-level, named, read-only ADO variable.
    ///
    /// `name` and `value` are both arbitrary strings, which may include ADO
    /// template expressions.
    ///
    /// NOTE: Unless required by some particular third-party task, it's strongly
    /// recommended to _avoid_ using this method, and to simply use
    /// [`ReadVar::from_static`] to get a obtain a static variable.
    ///
    /// DEVNOTE: In the future, this API may be updated to return a handle that
    /// will allow resolving the resulting `AdoRuntimeVar`, but for
    /// implementation expediency, this API does not currently do this. If you
    /// need to read the value of this variable at runtime, you may need to
    /// invoke [`AdoRuntimeVar::dangerous_from_global`] manually.
    ///
    /// [`NodeCtx::get_ado_variable`]: crate::node::NodeCtx::get_ado_variable
    pub fn ado_new_named_variable(self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        let name = name.as_ref();
        let value = value.as_ref();
        self.pipeline.jobs[self.job_idx]
            .ado_variables
            .insert(name.into(), value.into());
        self
    }

    /// (ADO only) Declare multiple job-level, named, read-only ADO variables at
    /// once.
    ///
    /// This is a convenience method to streamline invoking
    /// [`Self::ado_new_named_variable`] multiple times.
    ///
    /// NOTE: Unless required by some particular third-party task, it's strongly
    /// recommended to _avoid_ using this method, and to simply use
    /// [`ReadVar::from_static`] to get a obtain a static variable.
    ///
    /// DEVNOTE: In the future, this API may be updated to return a handle that
    /// will allow resolving the resulting `AdoRuntimeVar`, but for
    /// implementation expediency, this API does not currently do this. If you
    /// need to read the value of this variable at runtime, you may need to
    /// invoke [`AdoRuntimeVar::dangerous_from_global`] manually.
    ///
    /// [`NodeCtx::get_ado_variable`]: crate::node::NodeCtx::get_ado_variable
    pub fn ado_new_named_variables<K, V>(self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.pipeline.jobs[self.job_idx].ado_variables.extend(
            vars.into_iter()
                .map(|(k, v)| (k.as_ref().into(), v.as_ref().into())),
        );
        self
    }

    /// Overrides the id of the job.
    ///
    /// Flowey typically generates a reasonable job ID but some use cases that depend
    /// on the ID may find it useful to override it to something custom.
    pub fn ado_override_job_id(self, name: impl AsRef<str>) -> Self {
        self.pipeline
            .ado_job_id_overrides
            .insert(self.job_idx, name.as_ref().into());
        self
    }

    /// (GitHub Actions only) specify which Github runner this job will be run on.
    pub fn gh_set_pool(self, pool: GhRunner) -> Self {
        self.pipeline.jobs[self.job_idx].gh_pool = Some(pool);
        self
    }

    /// (GitHub Actions only) Manually override the `if:` condition for this
    /// particular job.
    ///
    /// **This is dangerous**, as an improperly set `if` condition may break
    /// downstream flowey jobs which assume flowey is in control of the job's
    /// scheduling logic.
    ///
    /// See
    /// <https://docs.github.com/en/actions/writing-workflows/workflow-syntax-for-github-actions#jobsjob_idif>
    /// for more info.
    pub fn gh_dangerous_override_if(self, condition: impl AsRef<str>) -> Self {
        self.pipeline.jobs[self.job_idx].gh_override_if = Some(condition.as_ref().into());
        self
    }

    /// (GitHub Actions only) Declare a global job-level environment variable,
    /// visible to all downstream steps.
    ///
    /// `name` and `value` are both arbitrary strings, which may include GitHub
    /// Actions template expressions.
    ///
    /// **This is dangerous**, as it is easy to misuse this API in order to
    /// write a node which takes an implicit dependency on there being a global
    /// variable set on its behalf by the top-level pipeline code, making it
    /// difficult to "locally reason" about the behavior of a node simply by
    /// reading its code.
    ///
    /// Whenever possible, nodes should "late bind" environment variables:
    /// accepting a compile-time / runtime flowey parameter, and then setting it
    /// prior to executing a child command that requires it.
    ///
    /// Only use this API in exceptional cases, such as obtaining an environment
    /// variable whose value is determined by a job-level GitHub Actions
    /// expression evaluation.
    pub fn gh_dangerous_global_env_var(
        self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Self {
        let name = name.as_ref();
        let value = value.as_ref();
        self.pipeline.jobs[self.job_idx]
            .gh_global_env
            .insert(name.into(), value.into());
        self
    }

    /// (GitHub Actions only) Grant permissions required by nodes in the job.
    ///
    /// For a given node handle, grant the specified permissions.
    /// The list provided must match the permissions specified within the node
    /// using `requires_permission`.
    ///
    /// NOTE: While this method is called at a node-level for auditability, the emitted
    /// yaml grants permissions at the job-level.
    ///
    /// This can lead to weird situations where node 1 might not specify a permission
    /// required according to Github Actions, but due to job-level granting of the permission
    /// by another node 2, the pipeline executes even though it wouldn't if node 2 was removed.
    ///
    /// For available permission scopes and their descriptions, see
    /// <https://docs.github.com/en/actions/using-workflows/workflow-syntax-for-github-actions#permissions>.
    pub fn gh_grant_permissions<N: FlowNodeBase + 'static>(
        self,
        permissions: impl IntoIterator<Item = (GhPermission, GhPermissionValue)>,
    ) -> Self {
        let node_handle = NodeHandle::from_type::<N>();
        for (permission, value) in permissions {
            self.pipeline.jobs[self.job_idx]
                .gh_permissions
                .entry(node_handle)
                .or_default()
                .insert(permission, value);
        }
        self
    }

    pub fn apply_patchfn(self, patchfn: crate::patch::PatchFn) -> Self {
        self.pipeline.jobs[self.job_idx]
            .patches
            .apply_patchfn(patchfn);
        self
    }

    /// Set a timeout for the job, in minutes.
    ///
    /// Not calling this will result in the platform's default timeout being used,
    /// which is typically 60 minutes, but may vary.
    pub fn with_timeout_in_minutes(self, timeout: u32) -> Self {
        self.pipeline.jobs[self.job_idx].timeout_minutes = Some(timeout);
        self
    }

    /// (ADO+Local Only) Only run the job if the specified condition is true.
    pub fn with_condition(self, cond: UseParameter<bool>) -> Self {
        self.pipeline.jobs[self.job_idx].cond_param_idx = Some(cond.idx);
        self.pipeline.parameters[cond.idx]
            .used_by_jobs
            .insert(self.job_idx);
        self
    }

    /// Add a flow node which will be run as part of the job.
    pub fn dep_on<R: IntoRequest + 'static>(
        self,
        f: impl FnOnce(&mut PipelineJobCtx<'_>) -> R,
    ) -> Self {
        // JobToNodeCtx will ensure artifact deps are taken care of
        let req = f(&mut PipelineJobCtx {
            pipeline: self.pipeline,
            job_idx: self.job_idx,
        });

        self.pipeline.jobs[self.job_idx]
            .root_nodes
            .entry(NodeHandle::from_type::<R::Node>())
            .or_default()
            .push(serde_json::to_vec(&req.into_request()).unwrap().into());

        self
    }

    /// Finish describing the pipeline job.
    pub fn finish(self) -> PipelineJobHandle {
        PipelineJobHandle {
            job_idx: self.job_idx,
        }
    }

    /// Return the job's platform.
    pub fn get_platform(&self) -> FlowPlatform {
        self.pipeline.jobs[self.job_idx].platform
    }

    /// Return the job's architecture.
    pub fn get_arch(&self) -> FlowArch {
        self.pipeline.jobs[self.job_idx].arch
    }
}

#[derive(Clone)]
pub struct PipelineJobHandle {
    job_idx: usize,
}

impl PipelineJobHandle {
    pub fn is_handle_for(&self, job: &PipelineJob<'_>) -> bool {
        self.job_idx == job.job_idx
    }
}

#[derive(Clone, Copy)]
pub enum PipelineBackendHint {
    /// Pipeline is being run on the user's dev machine (via bash / direct run)
    Local,
    /// Pipeline is run on ADO
    Ado,
    /// Pipeline is run on GitHub Actions
    Github,
}

/// Trait for types that can be converted into a [`Pipeline`].
///
/// This is the primary entry point for defining flowey pipelines. Implement this trait
/// to create a pipeline definition that can be executed locally or converted to CI YAML.
///
/// # Example
///
/// ```rust,no_run
/// use flowey_core::pipeline::{IntoPipeline, Pipeline, PipelineBackendHint};
/// use flowey_core::node::{FlowPlatform, FlowPlatformLinuxDistro, FlowArch};
///
/// struct MyPipeline;
///
/// impl IntoPipeline for MyPipeline {
///     fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
///         let mut pipeline = Pipeline::new();
///
///         // Define a job that runs on Linux x86_64
///         let _job = pipeline
///             .new_job(
///                 FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
///                 FlowArch::X86_64,
///                 "build"
///             )
///             .finish();
///
///         Ok(pipeline)
///     }
/// }
/// ```
///
/// # Complex Example with Parameters and Artifacts
///
/// ```rust,ignore
/// use flowey_core::pipeline::{IntoPipeline, Pipeline, PipelineBackendHint, ParameterKind};
/// use flowey_core::node::{FlowPlatform, FlowPlatformLinuxDistro, FlowArch};
///
/// struct BuildPipeline;
///
/// impl IntoPipeline for BuildPipeline {
///     fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline> {
///         let mut pipeline = Pipeline::new();
///
///         // Define a runtime parameter
///         let enable_tests = pipeline.new_parameter_bool(
///             "enable_tests",
///             "Whether to run tests",
///             ParameterKind::Stable,
///             Some(true) // default value
///         );
///
///         // Create an artifact for passing data between jobs
///         let (publish_build, use_build) = pipeline.new_artifact("build-output");
///
///         // Job 1: Build
///         let build_job = pipeline
///             .new_job(
///                 FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
///                 FlowArch::X86_64,
///                 "build"
///             )
///             .with_timeout_in_minutes(30)
///             .dep_on(|ctx| flowey_lib_hvlite::_jobs::example_node::Request {
///                 output_dir: ctx.publish_artifact(publish_build),
///             })
///             .finish();
///
///         // Job 2: Test (conditionally run based on parameter)
///         let _test_job = pipeline
///             .new_job(
///                 FlowPlatform::Linux(FlowPlatformLinuxDistro::Ubuntu),
///                 FlowArch::X86_64,
///                 "test"
///             )
///             .with_condition(enable_tests)
///             .dep_on(|ctx| flowey_lib_hvlite::_jobs::example_node2::Request {
///                 input_dir: ctx.use_artifact(&use_build),
///             })
///             .finish();
///
///         Ok(pipeline)
///     }
/// }
/// ```
pub trait IntoPipeline {
    fn into_pipeline(self, backend_hint: PipelineBackendHint) -> anyhow::Result<Pipeline>;
}

fn new_parameter_name(name: impl AsRef<str>, kind: ParameterKind) -> String {
    match kind {
        ParameterKind::Unstable => format!("__unstable_{}", name.as_ref()),
        ParameterKind::Stable => name.as_ref().into(),
    }
}

/// Structs which should only be used by top-level flowey emitters. If you're a
/// pipeline author, these are not types you need to care about!
pub mod internal {
    use super::*;
    use std::collections::BTreeMap;

    pub fn consistent_artifact_runtime_var_name(artifact: impl AsRef<str>, is_use: bool) -> String {
        format!(
            "artifact_{}_{}",
            if is_use { "use_from" } else { "publish_from" },
            artifact.as_ref()
        )
    }

    #[derive(Debug)]
    pub struct InternalAdoResourcesRepository {
        /// flowey-generated unique repo identifier
        pub repo_id: String,
        /// Type of repo that is being connected to.
        pub repo_type: AdoResourcesRepositoryType,
        /// Repository name. Format depends on `repo_type`.
        pub name: String,
        /// git ref to checkout.
        pub git_ref: AdoResourcesRepositoryRef<usize>,
        /// (optional) ID of the service endpoint connecting to this repository.
        pub endpoint: Option<String>,
    }

    pub struct PipelineJobMetadata {
        pub root_nodes: BTreeMap<NodeHandle, Vec<Box<[u8]>>>,
        pub patches: PatchResolver,
        pub label: String,
        pub platform: FlowPlatform,
        pub arch: FlowArch,
        pub cond_param_idx: Option<usize>,
        pub timeout_minutes: Option<u32>,
        // backend specific
        pub ado_pool: Option<AdoPool>,
        pub ado_variables: BTreeMap<String, String>,
        pub gh_override_if: Option<String>,
        pub gh_pool: Option<GhRunner>,
        pub gh_global_env: BTreeMap<String, String>,
        pub gh_permissions: BTreeMap<NodeHandle, BTreeMap<GhPermission, GhPermissionValue>>,
    }

    // TODO: support a more structured format for demands
    // See https://learn.microsoft.com/en-us/azure/devops/pipelines/yaml-schema/pool-demands
    #[derive(Debug, Clone)]
    pub struct AdoPool {
        pub name: String,
        pub demands: Vec<String>,
    }

    #[derive(Debug)]
    pub struct ArtifactMeta {
        pub name: String,
        pub published_by_job: Option<usize>,
        pub used_by_jobs: BTreeSet<usize>,
    }

    #[derive(Debug)]
    pub struct ParameterMeta {
        pub parameter: Parameter,
        pub used_by_jobs: BTreeSet<usize>,
    }

    /// Mirror of [`Pipeline`], except with all field marked as `pub`.
    pub struct PipelineFinalized {
        pub jobs: Vec<PipelineJobMetadata>,
        pub artifacts: Vec<ArtifactMeta>,
        pub parameters: Vec<ParameterMeta>,
        pub extra_deps: BTreeSet<(usize, usize)>,
        // backend specific
        pub ado_name: Option<String>,
        pub ado_schedule_triggers: Vec<AdoScheduleTriggers>,
        pub ado_ci_triggers: Option<AdoCiTriggers>,
        pub ado_pr_triggers: Option<AdoPrTriggers>,
        pub ado_bootstrap_template: String,
        pub ado_resources_repository: Vec<InternalAdoResourcesRepository>,
        pub ado_post_process_yaml_cb:
            Option<Box<dyn FnOnce(serde_yaml::Value) -> serde_yaml::Value>>,
        pub ado_variables: BTreeMap<String, String>,
        pub ado_job_id_overrides: BTreeMap<usize, String>,
        pub gh_name: Option<String>,
        pub gh_schedule_triggers: Vec<GhScheduleTriggers>,
        pub gh_ci_triggers: Option<GhCiTriggers>,
        pub gh_pr_triggers: Option<GhPrTriggers>,
        pub gh_bootstrap_template: String,
    }

    impl PipelineFinalized {
        pub fn from_pipeline(mut pipeline: Pipeline) -> Self {
            if let Some(cb) = pipeline.inject_all_jobs_with.take() {
                for job_idx in 0..pipeline.jobs.len() {
                    let _ = cb(PipelineJob {
                        pipeline: &mut pipeline,
                        job_idx,
                    });
                }
            }

            let Pipeline {
                mut jobs,
                artifacts,
                parameters,
                extra_deps,
                ado_name,
                ado_bootstrap_template,
                ado_schedule_triggers,
                ado_ci_triggers,
                ado_pr_triggers,
                ado_resources_repository,
                ado_post_process_yaml_cb,
                ado_variables,
                ado_job_id_overrides,
                gh_name,
                gh_schedule_triggers,
                gh_ci_triggers,
                gh_pr_triggers,
                gh_bootstrap_template,
                // not relevant to consumer code
                dummy_done_idx: _,
                artifact_map_idx: _,
                artifact_names: _,
                global_patchfns,
                inject_all_jobs_with: _, // processed above
            } = pipeline;

            for patchfn in global_patchfns {
                for job in &mut jobs {
                    job.patches.apply_patchfn(patchfn)
                }
            }

            Self {
                jobs,
                artifacts,
                parameters,
                extra_deps,
                ado_name,
                ado_schedule_triggers,
                ado_ci_triggers,
                ado_pr_triggers,
                ado_bootstrap_template,
                ado_resources_repository,
                ado_post_process_yaml_cb,
                ado_variables,
                ado_job_id_overrides,
                gh_name,
                gh_schedule_triggers,
                gh_ci_triggers,
                gh_pr_triggers,
                gh_bootstrap_template,
            }
        }
    }

    #[derive(Debug, Clone)]
    pub enum Parameter {
        Bool {
            name: String,
            description: String,
            kind: ParameterKind,
            default: Option<bool>,
        },
        String {
            name: String,
            description: String,
            default: Option<String>,
            kind: ParameterKind,
            possible_values: Option<Vec<String>>,
        },
        Num {
            name: String,
            description: String,
            default: Option<i64>,
            kind: ParameterKind,
            possible_values: Option<Vec<i64>>,
        },
    }

    impl Parameter {
        pub fn name(&self) -> &str {
            match self {
                Parameter::Bool { name, .. } => name,
                Parameter::String { name, .. } => name,
                Parameter::Num { name, .. } => name,
            }
        }
    }
}
