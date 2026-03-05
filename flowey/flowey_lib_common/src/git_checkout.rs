// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checkout git repos

use flowey::node::prelude::*;
use std::collections::BTreeMap;

/// Describes the source of a particular repo.
#[derive(Serialize, Deserialize)]
pub enum RepoSource<C = VarNotClaimed> {
    /// (ADO Only) Checkout a repo described by the given ADO resource.
    ///
    /// [`AdoResourcesRepositoryId`] is only obtainable by declaring the
    /// resource at the pipeline level. See the docs for this type for more
    /// information.
    AdoResource(AdoResourcesRepositoryId),
    /// (GitHub Only) Checkout a repo described by the given repository "{owner}/{name}" (e.g. "microsoft/openvmm") .
    GithubRepo { owner: String, name: String },
    /// (GitHub Only) Checkout the repo containing the pipeline.
    GithubSelf,
    /// Use a pre-existing clone of the repo.
    ExistingClone(ReadVar<PathBuf, C>),
    /// (Local Only): Clone the repo from the given URL in the given path.
    LocalOnlyNewClone {
        url: String,
        path: PathBuf,
        ignore_existing_clone: bool,
    },
}

impl<C> Clone for RepoSource<C> {
    fn clone(&self) -> Self {
        match self {
            Self::AdoResource(arg0) => Self::AdoResource(arg0.clone()),
            Self::GithubRepo { owner, name } => Self::GithubRepo {
                owner: owner.clone(),
                name: name.clone(),
            },
            Self::GithubSelf => Self::GithubSelf,
            Self::ExistingClone(arg0) => Self::ExistingClone(arg0.clone()),
            Self::LocalOnlyNewClone {
                url,
                path,
                ignore_existing_clone,
            } => Self::LocalOnlyNewClone {
                url: url.clone(),
                path: path.clone(),
                ignore_existing_clone: *ignore_existing_clone,
            },
        }
    }
}

// FUTURE: really should be a proc macro
impl ClaimVar for RepoSource {
    type Claimed = RepoSource<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        match self {
            RepoSource::AdoResource(x) => RepoSource::AdoResource(x),
            RepoSource::GithubRepo { owner, name } => RepoSource::GithubRepo { owner, name },
            RepoSource::GithubSelf => RepoSource::GithubSelf,
            RepoSource::ExistingClone(v) => RepoSource::ExistingClone(v.claim(ctx)),
            RepoSource::LocalOnlyNewClone {
                url,
                path,
                ignore_existing_clone,
            } => RepoSource::LocalOnlyNewClone {
                url,
                path,
                ignore_existing_clone,
            },
        }
    }
}

flowey_request! {
    pub enum Request {
        /// Checkout a repo, returning a path to the repo.
        ///
        /// Checking out the same repo multiple times will result in unique clones
        /// on each invocation.
        ///
        /// Notice: unlike the checkout steps you might be familiar with in ADO or
        /// GH Actions, the details of how / where the repo is checked out are
        /// _decoupled_ from the having nodes get a handle to a checked out repo's
        /// path.
        ///
        /// This is because the specifics of how / where the repo is checked out
        /// vary depending on the flow's deployment context, and are therefore
        /// provided separately via the [`Request::RegisterRepo`] request (typically
        /// via a top-level job node).
        CheckoutRepo {
            /// ad-hoc string used to correlate this `CheckoutRepo` request with its
            /// corresponding `RegisterRepo` request.
            repo_id: ReadVar<String>,
            /// Path to the cloned repo
            repo_path: WriteVar<PathBuf>,
            /// In CI: whether the cloned repo should persist credentials
            /// post-clone.
            persist_credentials: bool,
            // FUTURE: include additional knobs, like whether or not to clone
            // submodules, checkout depth, etc...
        },
        /// Specify the details of how to check out a particular repo_id.
        RegisterRepo {
            /// ad-hoc string used to correlate this `RegisterRepo` request with its
            /// corresponding `CheckoutRepo` request.
            repo_id: String,
            /// How the repo should be cloned
            repo_src: RepoSource,
            /// In CI: whether checkout requests for this repo should be allowed to
            /// persist credentials post-clone.
            ///
            /// NOTE: in order to avoid accidentally giving credentials to flows
            /// that didn't explicitly request them ,flowey requires that a repo
            /// cloned with persistent credentials to be registered under a
            /// _separate_ repo_id than the repo without persistent credentials.
            allow_persist_credentials: bool,
            /// The fetch depth of the checkout. If None, the entire history is
            /// checked out.
            // FIXME: this should really be on `CheckoutRepo`, but that will require
            // a bit of refactoring to the node logic below... to unblock the
            // current fire, I'm just going to leave it here for now.
            depth: Option<usize>,
            pre_run_deps: Vec<ReadVar<SideEffect>>,
        },
        /// When running locally: whether or not all repos should be cloned
        /// locally ahead of time, vs. re-cloning them.
        LocalOnlyRequireExistingClones(bool),
    }
}

new_flow_node!(struct Node);

// TODO: this entire module should be proc macro generated...
pub mod process_reqs {
    use super::*;

    pub struct RequestCheckoutRepo {
        pub repo_id: ReadVar<String>,
        pub repo_path: WriteVar<PathBuf>,
        pub persist_credentials: bool,
    }

    pub struct RequestRegisterRepo {
        pub repo_id: String,
        pub repo_src: RepoSource,
        pub allow_persist_credentials: bool,
        pub depth: Option<usize>,
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
    }

    pub struct ResolvedRequestsAdo {
        pub checkout_repo: Vec<RequestCheckoutRepo>,
        pub register_repo: Vec<RequestRegisterRepo>,
    }

    impl ResolvedRequestsAdo {
        pub fn from_reqs(requests: Vec<Request>) -> anyhow::Result<Self> {
            let ResolvedRequests::Ado(v) = process_reqs(requests, false)? else {
                panic!()
            };
            Ok(v)
        }
    }

    pub struct ResolvedRequestsLocal {
        pub checkout_repo: Vec<RequestCheckoutRepo>,
        pub register_repo: Vec<RequestRegisterRepo>,
        pub require_local_clones: bool,
    }

    impl ResolvedRequestsLocal {
        pub fn from_reqs(requests: Vec<Request>) -> anyhow::Result<Self> {
            let ResolvedRequests::Local(v) = process_reqs(requests, true)? else {
                panic!()
            };
            Ok(v)
        }
    }

    enum ResolvedRequests {
        Ado(ResolvedRequestsAdo),
        Local(ResolvedRequestsLocal),
    }

    fn process_reqs(requests: Vec<Request>, is_local: bool) -> anyhow::Result<ResolvedRequests> {
        let mut checkout_repo = Vec::new();
        let mut register_repo = Vec::new();
        let mut require_local_clones = None;

        for req in requests {
            match req {
                Request::CheckoutRepo {
                    repo_id,
                    repo_path,
                    persist_credentials,
                } => checkout_repo.push(RequestCheckoutRepo {
                    repo_id,
                    repo_path,
                    persist_credentials,
                }),
                Request::RegisterRepo {
                    repo_id,
                    repo_src,
                    allow_persist_credentials,
                    depth,
                    pre_run_deps,
                } => register_repo.push(RequestRegisterRepo {
                    repo_id,
                    repo_src,
                    allow_persist_credentials,
                    depth,
                    pre_run_deps,
                }),
                Request::LocalOnlyRequireExistingClones(v) => same_across_all_reqs(
                    "LocalOnlyRequireExistingClones",
                    &mut require_local_clones,
                    v,
                )?,
            }
        }

        if !is_local {
            if require_local_clones.is_some() {
                anyhow::bail!(
                    "can only set `LocalOnlyRequireExistingClones` when using the Local backend"
                )
            }
        }

        Ok(if is_local {
            ResolvedRequests::Local(ResolvedRequestsLocal {
                checkout_repo,
                register_repo,
                require_local_clones: require_local_clones.ok_or(anyhow::anyhow!(
                    "Missing required request: LocalOnlyRequireExistingClones",
                ))?,
            })
        } else {
            ResolvedRequests::Ado(ResolvedRequestsAdo {
                checkout_repo,
                register_repo,
            })
        })
    }
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(dep: &mut ImportCtx<'_>) {
        dep.import::<crate::install_git::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        match ctx.backend() {
            FlowBackend::Local => Self::emit_local(requests, ctx),
            FlowBackend::Ado => Self::emit_ado(requests, ctx),
            FlowBackend::Github => Self::emit_gh(requests, ctx),
        }
    }
}

impl Node {
    fn emit_ado(requests: Vec<Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let process_reqs::ResolvedRequestsAdo {
            checkout_repo,
            register_repo,
        } = process_reqs::ResolvedRequestsAdo::from_reqs(requests)?;

        if checkout_repo.is_empty() {
            return Ok(());
        }

        let mut did_checkouts = Vec::new();
        let mut registered_repos = BTreeMap::<(String, bool), (usize, RepoSource)>::new();
        for (
            idx,
            process_reqs::RequestRegisterRepo {
                repo_id,
                repo_src,
                allow_persist_credentials,
                depth,
                pre_run_deps,
            },
        ) in register_repo.into_iter().enumerate()
        {
            let existing = registered_repos.insert(
                (repo_id.clone(), allow_persist_credentials),
                (idx, repo_src.clone()),
            );
            if existing.is_some() {
                anyhow::bail!("got a duplicate RegisterRepo request for {repo_id}")
            }

            let (persist_credentials_str, write_persist_credentials_str) = ctx.new_var();
            let (active, write_active) = ctx.new_var();

            ctx.emit_rust_step(format!("check if {repo_id} needs to be cloned"), |ctx| {
                pre_run_deps.claim(ctx);
                let write_active = write_active.claim(ctx);
                let write_persist_credentials_str = write_persist_credentials_str.claim(ctx);
                let repo_ids = checkout_repo
                    .iter()
                    .map(|process_reqs::RequestCheckoutRepo { repo_id, persist_credentials, .. }| {
                       ( repo_id.clone().claim(ctx), *persist_credentials)
                    })
                    .collect::<Vec<_>>();
                let repo_id = repo_id.clone();
                move |rt| {
                    for (requested_checkout_repo_id, persist_credentials) in repo_ids {
                        if rt.read(requested_checkout_repo_id) == repo_id {
                            if persist_credentials {
                                if allow_persist_credentials != persist_credentials {
                                    anyhow::bail!("pipeline implementation bug: attempted to checkout repo with `persist_credentials`, whose registration didn't include `allow_persist_credentials: true`")
                                }
                            }

                            rt.write(write_persist_credentials_str, &persist_credentials.to_string());
                            rt.write(write_active, &true);
                            return Ok(());
                        }
                    }

                    rt.write(write_active, &false);
                    Ok(())
                }
            });

            let (did_checkout, claim_did_checkout) = ctx.new_var();
            if let RepoSource::AdoResource(checkout_str) = repo_src {
                ctx.emit_ado_step_with_condition(
                    format!("checkout repo {repo_id}"),
                    active.clone(),
                    |ctx| {
                        claim_did_checkout.claim(ctx);
                        let persist_credentials_str = persist_credentials_str.claim(ctx);
                        move |rt| {
                            let checkout_str = rt.resolve_repository_id(checkout_str);
                            let persist_credentials =
                                rt.get_var(persist_credentials_str).as_raw_var_name();
                            let depth = match depth {
                                Some(x) => x.to_string(),
                                None => "0".into(),
                            };

                            // FUTURE: make fetchTags, fetchDepth configurable
                            // (along with many other things)
                            //
                            // TODO OSS: for expediency - always clone with
                            // recursive submodules. This should be
                            // configurable...
                            format!(
                                r#"
                                - checkout: {checkout_str}
                                  path: repo{idx}
                                  fetchTags: false
                                  fetchDepth: {depth}
                                  persistCredentials: $({persist_credentials})
                                  submodules: recursive
                            "#
                            )
                        }
                    },
                );
            } else {
                ctx.emit_side_effect_step(
                    [
                        active.into_side_effect(),
                        persist_credentials_str.into_side_effect(),
                    ],
                    [claim_did_checkout],
                )
            }

            did_checkouts.push(did_checkout);
        }

        ctx.emit_rust_step("report cloned repo directories", move |ctx| {
            did_checkouts.claim(ctx);
            let mut registered_repos = registered_repos.into_iter().map(|(k, (a, b))| (k, (a, b.claim(ctx)))).collect::<BTreeMap<_, _>>();
            let checkout_repo = checkout_repo
                .into_iter()
                .map(|process_reqs::RequestCheckoutRepo { repo_id, repo_path, persist_credentials }| {
                    (repo_id.claim(ctx), repo_path.claim(ctx), persist_credentials)
                })
                .collect::<Vec<_>>();

            move |rt| {
                let mut checkout_reqs = BTreeMap::<(String, bool), Vec<ClaimedWriteVar<PathBuf>>>::new();
                for (repo_id, repo_path, persist_credentials) in checkout_repo {
                    checkout_reqs
                        .entry((rt.read(repo_id), persist_credentials))
                        .or_default()
                        .push(repo_path);
                }


                for ((repo_id, persist_credentials), repo_paths) in checkout_reqs {
                    let (idx, repo_src) = registered_repos
                        .remove(&(repo_id.clone(), persist_credentials))
                        .with_context(|| format!("pipeline implementation bug: did not specify a RegisterRepo request for repo {repo_id}"))?;

                    let path = match repo_src {
                        RepoSource::AdoResource(_) => {
                            // HACK: this should be using something like AGENT_WORKDIR
                            if cfg!(windows) {
                                Path::new(r#"D:\a\_work\1\"#)
                            } else {
                                Path::new("/mnt/vss/_work/1/")
                            }
                            .join(format!("repo{idx}"))
                        },
                        RepoSource::GithubRepo{ .. } | RepoSource::GithubSelf => anyhow::bail!("repo source for ADO backend must be an `AdoResource` or `ExistingClone`"),
                        RepoSource::ExistingClone(path) => {
                            let path = rt.read(path);
                            path.absolute().context(format!("Failed to make {} absolute", path.display()))?
                        },
                        RepoSource::LocalOnlyNewClone { .. } => unreachable!(),
                    };

                    log::info!("reporting repo is cloned at {}", path.display());
                    for var in repo_paths {
                        rt.write(var, &path);
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }

    fn emit_gh(requests: Vec<Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let process_reqs::ResolvedRequestsAdo {
            checkout_repo,
            register_repo,
        } = process_reqs::ResolvedRequestsAdo::from_reqs(requests)?;

        if checkout_repo.is_empty() {
            return Ok(());
        }

        let mut did_checkouts = Vec::new();
        let mut registered_repos = BTreeMap::<(String, bool), (usize, RepoSource)>::new();
        for (
            idx,
            process_reqs::RequestRegisterRepo {
                repo_id,
                repo_src,
                allow_persist_credentials,
                depth,
                pre_run_deps,
            },
        ) in register_repo.into_iter().enumerate()
        {
            let existing = registered_repos.insert(
                (repo_id.clone(), allow_persist_credentials),
                (idx, repo_src.clone()),
            );
            if existing.is_some() {
                anyhow::bail!("got a duplicate RegisterRepo request for {repo_id}")
            }

            let (persist_credentials_str, write_persist_credentials_str) = ctx.new_var();
            let (active, write_active) = ctx.new_var();
            ctx.emit_rust_step(format!("check if {repo_id} needs to be cloned"), |ctx| {
                pre_run_deps.claim(ctx);
                let write_active = write_active.claim(ctx);
                let write_persist_credentials_str = write_persist_credentials_str.claim(ctx);
                let repo_ids = checkout_repo
                    .iter()
                    .map(|process_reqs::RequestCheckoutRepo { repo_id, persist_credentials, .. }| {
                       (repo_id.clone().claim(ctx), *persist_credentials)
                    })
                    .collect::<Vec<_>>();
                let repo_id = repo_id.clone();
                move |rt| {
                    for (requested_checkout_repo_id, persist_credentials) in repo_ids {
                        if rt.read(requested_checkout_repo_id) == repo_id {
                            if persist_credentials {
                                if allow_persist_credentials != persist_credentials {
                                    anyhow::bail!("pipeline implementation bug: attempted to checkout repo with `persist_credentials`, whose registration didn't include `allow_persist_credentials: true`")
                                }
                            }

                            rt.write(write_persist_credentials_str, &persist_credentials.to_string());
                            rt.write(write_active, &true);
                            return Ok(());
                        }
                    }

                    rt.write(write_active, &false);
                    Ok(())
                }
            });

            if matches!(
                repo_src,
                RepoSource::GithubSelf | RepoSource::GithubRepo { .. }
            ) {
                let mut step = ctx
                    .emit_gh_step(format!("checkout repo {repo_id}"), "actions/checkout@v4")
                    .condition(active.clone())
                    .with("path", format!("repo{idx}"))
                    .with("fetch-depth", depth.unwrap_or(0).to_string())
                    .with("persist-credentials", persist_credentials_str)
                    .requires_permission(GhPermission::Contents, GhPermissionValue::Read);
                if let RepoSource::GithubRepo { owner, name } = repo_src {
                    step = step.with("repository", format!("{owner}/{name}"))
                }
                did_checkouts.push(step.finish(ctx));
            } else if !matches!(repo_src, RepoSource::ExistingClone(_)) {
                anyhow::bail!(
                    "repo source must be a `GithubRepo`, `GithubSelf`, or `ExistingClone` for GitHub backend"
                );
            }
        }

        let parent_path = ctx.get_gh_context_var().global().workspace();
        ctx.emit_rust_step("report cloned repo directories", move |ctx| {
            did_checkouts.claim(ctx);
            let mut registered_repos = registered_repos.into_iter().map(|(k, (a, b))| (k, (a, b.claim(ctx)))).collect::<BTreeMap<_, _>>();
            let checkout_repo = checkout_repo
                .into_iter()
                .map(|process_reqs::RequestCheckoutRepo { repo_id, repo_path, persist_credentials }| {
                    (repo_id.claim(ctx), repo_path.claim(ctx), persist_credentials)
                })
                .collect::<Vec<_>>();
            let parent_path = parent_path.claim(ctx);

            move |rt| {
                let mut checkout_reqs = BTreeMap::<(String, bool), Vec<ClaimedWriteVar<PathBuf>>>::new();
                for (repo_id, repo_path, persist_credentials) in checkout_repo {
                    checkout_reqs
                        .entry((rt.read(repo_id), persist_credentials))
                        .or_default()
                        .push(repo_path);
                }

                let parent_path = rt.read(parent_path);
                for ((repo_id, persist_credentials), repo_paths) in checkout_reqs {
                    let (idx, repo_src) = registered_repos
                        .remove(&(repo_id.clone(), persist_credentials))
                        .with_context(|| format!("pipeline implementation bug: did not specify a RegisterRepo request for repo {repo_id}"))?;

                    let path = match repo_src {
                        RepoSource::AdoResource(_) => unreachable!(),
                        RepoSource::GithubRepo{ .. } => {
                            PathBuf::from(parent_path.clone()).join(format!("repo{idx}"))
                        },
                        RepoSource::GithubSelf => {
                            PathBuf::from(parent_path.clone()).join(format!("repo{idx}"))
                        },
                        RepoSource::ExistingClone(path) => {
                            let path = rt.read(path);
                            path.absolute().context(format!("Failed to make {} absolute", path.display()))?
                        },
                        RepoSource::LocalOnlyNewClone { .. } => unreachable!(),
                    };

                    log::info!("reporting repo is cloned at {}", path.display());

                    for var in repo_paths {
                        rt.write(var, &path);
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }

    fn emit_local(requests: Vec<Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let process_reqs::ResolvedRequestsLocal {
            checkout_repo,
            register_repo,
            require_local_clones,
        } = process_reqs::ResolvedRequestsLocal::from_reqs(requests)?;

        if checkout_repo.is_empty() {
            return Ok(());
        }

        let git_ensure_installed = ctx.reqv(crate::install_git::Request::EnsureInstalled);

        ctx.emit_rust_step("report repo directory", move |ctx| {
            git_ensure_installed.claim(ctx);
            let register_repo = register_repo
                .into_iter()
                .map(|process_reqs::RequestRegisterRepo { repo_id, repo_src, allow_persist_credentials: _, depth, pre_run_deps }|
                    (repo_id, repo_src.claim(ctx), depth, pre_run_deps.claim(ctx)
                )).collect::<Vec<_>>();
            let checkout_repo = checkout_repo
                .into_iter()
                .map(|process_reqs::RequestCheckoutRepo { repo_id, repo_path, persist_credentials }| {
                    (repo_id.claim(ctx), repo_path.claim(ctx), persist_credentials)
                })
                .collect::<Vec<_>>();

            move |rt| {
               for (checkout_repo_id, repo_path, _persist_credentials) in checkout_repo {
                    let checkout_repo_id = rt.read(checkout_repo_id);

                    log::info!("reporting checkout info for {checkout_repo_id}");

                    let mut found_path = None;
                    for (repo_id, repo_src, depth, _) in &register_repo {
                        if &checkout_repo_id != repo_id {
                            continue;
                        }

                        match repo_src {
                            RepoSource::ExistingClone(path) => {
                                let path = rt.read(path.clone());
                                let path = path.absolute().context(format!("Failed to make {} absolute", path.display()))?;
                                found_path = Some(path);
                                break;
                            }
                            RepoSource::LocalOnlyNewClone { .. } if require_local_clones => {
                                anyhow::bail!("`LocalOnlyRequireExistingClones` is active, all repos must be registered using `RepoKind::ExistingClone`");
                            }
                            RepoSource::LocalOnlyNewClone { url, path, ignore_existing_clone } => {
                                if rt.sh.path_exists(path) {
                                    rt.sh.change_dir(path);
                                    if flowey::shell_cmd!(rt, "git status").run().is_ok()
                                        && *ignore_existing_clone
                                    {
                                        rt.write(repo_path, path);
                                        return Ok(());
                                    }
                                }
                                if let Some(depth_arg) = depth {
                                    let depth_arg_string = depth_arg.to_string();
                                    flowey::shell_cmd!(rt, "git clone --depth {depth_arg_string} {url} {path}").run()?;
                                } else {
                                    flowey::shell_cmd!(rt, "git clone {url} {path}").run()?;
                                }
                                found_path = Some(path.clone());
                                break;
                            }
                            RepoSource::AdoResource( .. ) => {
                                anyhow::bail!("ADO resources are not supported on local backend");
                            }
                            RepoSource::GithubRepo{ .. } | RepoSource::GithubSelf => {
                                anyhow::bail!("Github repos for GH Actions are not supported on local backend");
                            }
                        }
                    }

                    if let Some(path) = found_path {
                        rt.write(repo_path, &path);
                    } else {
                        anyhow::bail!("missing registration for id {checkout_repo_id}")
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}
