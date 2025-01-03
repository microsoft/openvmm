// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a github release artifact

use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// First component of a github repo path
        ///
        /// e.g: the "foo" in "github.com/foo/bar"
        pub repo_owner: String,
        /// Second component of a github repo path
        ///
        /// e.g: the "bar" in "github.com/foo/bar"
        pub repo_name: String,
        /// Specific artifact to download.
        pub file_name: String,
        /// Path to downloaded artifact.
        pub path: WriteVar<PathBuf>,
        /// The Github actions run id to download artifacts from
        pub run_id: String
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cache::Node>();
        ctx.import::<crate::use_gh_cli::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut download_reqs: BTreeMap<
            (String, String, String),
            BTreeMap<String, Vec<WriteVar<PathBuf>>>,
        > = BTreeMap::new();
        for req in requests {
            let Request {
                repo_owner,
                repo_name,
                file_name,
                path,
                run_id,
            } = req;

            // if any package needs auth, we might as well download every
            // package using the GH cli.
            download_reqs
                .entry((repo_owner, repo_name, run_id))
                .or_default()
                .entry(file_name)
                .or_default()
                .push(path)
        }

        if download_reqs.is_empty() {
            return Ok(());
        }

        let gh_token = ctx.get_gh_context_var(GhContextVar::GITHUB__TOKEN);
        ctx.req(crate::use_gh_cli::Request::WithAuth(
            crate::use_gh_cli::GhCliAuth::AuthToken(gh_token),
        ));
        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        match ctx.persistent_dir() {
            Some(dir) => Self::with_local_cache(ctx, dir, download_reqs, gh_cli),
            None => Self::with_ci_cache(ctx, download_reqs, gh_cli),
        }

        Ok(())
    }
}

impl Node {
    // Have a single folder which caches downloaded artifacts
    fn with_local_cache(
        ctx: &mut NodeCtx<'_>,
        persistent_dir: ReadVar<PathBuf>,
        download_reqs: BTreeMap<(String, String, String), BTreeMap<String, Vec<WriteVar<PathBuf>>>>,
        gh_cli: ReadVar<PathBuf>,
    ) {
        ctx.emit_rust_step("download artifacts from github actions run", |ctx| {
            let gh_cli = gh_cli.claim(ctx);
            let persistent_dir = persistent_dir.claim(ctx);
            let download_reqs = download_reqs.claim(ctx);
            move |rt| {
                let persistent_dir = rt.read(persistent_dir);

                // first - check what reqs are already present in the local cache
                let mut remaining_download_reqs: BTreeMap<
                    (String, String, String),
                    BTreeMap<String, Vec<ClaimedWriteVar<PathBuf>>>,
                > = BTreeMap::new();
                for ((repo_owner, repo_name, run_id), files) in download_reqs {
                    for (file, vars) in files {
                        let cached_file = persistent_dir
                            .join(format!("{repo_owner}/{repo_name}/{run_id}/{file}"));

                        if cached_file.exists() {
                            for var in vars {
                                rt.write(var, &cached_file)
                            }
                        } else {
                            let existing = remaining_download_reqs
                                .entry((repo_owner.clone(), repo_name.clone(), run_id.clone()))
                                .or_default()
                                .insert(file, vars);
                            assert!(existing.is_none());
                        }
                    }
                }

                if remaining_download_reqs.is_empty() {
                    log::info!("100% local cache hit!");
                    return Ok(());
                }

                download_all_reqs(rt, &remaining_download_reqs, &persistent_dir, gh_cli)?;

                for ((repo_owner, repo_name, run_id), files) in remaining_download_reqs {
                    for (file, vars) in files {
                        let file = persistent_dir
                            .join(format!("{repo_owner}/{repo_name}/{run_id}/{file}"));
                        assert!(file.exists());
                        for var in vars {
                            rt.write(var, &file)
                        }
                    }
                }

                Ok(())
            }
        });
    }

    // Instead of having a cache directory per-repo (and spamming the
    // workflow with a whole bunch of cache task requests), have a single
    // cache directory for each flow's request-set.
    fn with_ci_cache(
        ctx: &mut NodeCtx<'_>,
        download_reqs: BTreeMap<(String, String, String), BTreeMap<String, Vec<WriteVar<PathBuf>>>>,
        gh_cli: ReadVar<PathBuf>,
    ) {
        let cache_dir = ctx.emit_rust_stepv("create gh-artifact-download cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let request_set_hash = {
            let hasher = &mut rustc_hash::FxHasher::default();
            for ((repo_owner, repo_name, run_id), files) in &download_reqs {
                std::hash::Hash::hash(repo_owner, hasher);
                std::hash::Hash::hash(repo_name, hasher);
                std::hash::Hash::hash(run_id, hasher);
                for file in files.keys() {
                    std::hash::Hash::hash(&file, hasher);
                }
            }
            let hash = std::hash::Hasher::finish(hasher);
            format!("{:08x?}", hash)
        };

        let cache_key = ReadVar::from_static(format!("gh-artifact-download-{request_set_hash}"));
        let hitvar = ctx.reqv(|v| {
            crate::cache::Request {
                label: "gh-artifact-download".into(),
                dir: cache_dir.clone(),
                key: cache_key,
                restore_keys: None, // OK if not exact - better than nothing
                hitvar: crate::cache::CacheResult::HitVar(v),
            }
        });

        ctx.emit_rust_step("download artifacts from github", |ctx| {
            let cache_dir = cache_dir.claim(ctx);
            let hitvar = hitvar.claim(ctx);
            let gh_cli = gh_cli.claim(ctx);
            let download_reqs = download_reqs.claim(ctx);
            move |rt| {
                let cache_dir = rt.read(cache_dir);
                let hitvar = rt.read(hitvar);

                if !matches!(hitvar, crate::cache::CacheHit::Hit) {
                    download_all_reqs(rt, &download_reqs, &cache_dir, gh_cli)?;
                }

                for ((repo_owner, repo_name, run_id), files) in download_reqs {
                    for (file, vars) in files {
                        let file =
                            cache_dir.join(format!("{repo_owner}/{repo_name}/{run_id}/{file}"));
                        assert!(file.exists());
                        for var in vars {
                            rt.write(var, &file)
                        }
                    }
                }

                Ok(())
            }
        });
    }
}

fn download_all_reqs(
    rt: &mut RustRuntimeServices<'_>,
    download_reqs: &BTreeMap<
        (String, String, String),
        BTreeMap<String, Vec<WriteVar<PathBuf, VarClaimed>>>,
    >,
    cache_dir: &Path,
    gh_cli: ReadVar<PathBuf, VarClaimed>,
) -> anyhow::Result<()> {
    let sh = xshell::Shell::new()?;
    let gh_cli = rt.read(gh_cli);

    for ((repo_owner, repo_name, run_id), files) in download_reqs {
        let repo = format!("{repo_owner}/{repo_name}");

        let out_dir = cache_dir.join(format!("{repo_owner}/{repo_name}/{run_id}"));
        fs_err::create_dir_all(&out_dir)?;
        sh.change_dir(&out_dir);

        // FUTURE: while the gh cli takes care of doing simultaneous downloads in
        // the context of a single (repo, tag), we might want to have flowey spawn
        // multiple processes to saturate the network connection in cases where
        // multiple (repo, tag) pairs are being pulled at the same time.
        let patterns = files.keys().flat_map(|k| ["--pattern".into(), k.clone()]);
        xshell::cmd!(sh, "{gh_cli} run download {run_id} -R {repo} {patterns...}").run()?;
    }

    Ok(())
}
