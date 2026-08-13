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
        /// Whether this repo requires authentication.
        ///
        /// If true, downloads will be routed through the `gh` CLI client, which
        /// will require auth to be set up. See
        /// [`use_gh_cli`](crate::use_gh_cli).
        pub needs_auth: bool,
        /// Tag associated with the release artifact.
        pub tag: String,
        /// Specific filename to download.
        pub file_name: String,
        /// Path to downloaded artifact.
        pub path: WriteVar<PathBuf>,
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
        let mut use_gh_cli = false;

        for req in requests {
            let Request {
                repo_owner,
                repo_name,
                needs_auth,
                tag,
                file_name,
                path,
            } = req;

            // if any package needs auth, we might as well download every
            // package using the GH cli.
            use_gh_cli |= needs_auth;

            download_reqs
                .entry((repo_owner, repo_name, tag))
                .or_default()
                .entry(file_name)
                .or_default()
                .push(path)
        }

        if download_reqs.is_empty() {
            return Ok(());
        }

        let gh_cli = use_gh_cli.then(|| ctx.reqv(crate::use_gh_cli::Request::Get));

        match ctx.memo_store() {
            Some(store) => Self::with_local_cache(ctx, store, download_reqs, gh_cli),
            None => Self::with_ci_cache(ctx, download_reqs, gh_cli),
        }

        Ok(())
    }
}

impl Node {
    // Each (repo, tag, file) gets its own entry in flowey's shared memoization
    // store, which gives us atomic installs (an interrupted download can't
    // leave a truncated file behind that later runs mistake for a cache hit)
    // and central GC.
    //
    // Entries are created via the batch API, so that a cold miss can still
    // fetch several assets in a single `gh release download` invocation.
    fn with_local_cache(
        ctx: &mut NodeCtx<'_>,
        memo_store: ReadVar<MemoStore>,
        download_reqs: BTreeMap<(String, String, String), BTreeMap<String, Vec<WriteVar<PathBuf>>>>,
        gh_cli: Option<ReadVar<PathBuf>>,
    ) {
        ctx.emit_rust_step("download artifacts from github releases", |ctx| {
            let gh_cli = gh_cli.claim(ctx);
            let memo_store = memo_store.claim(ctx);
            let download_reqs = download_reqs.claim(ctx);
            move |rt| {
                let store = rt.read(memo_store);

                // one memo entry per file, keyed by the identity of the remote
                // asset. there are no local inputs to fingerprint here - the
                // store is being used for atomicity and GC, not content
                // addressing.
                let mut misses = Vec::new();
                let mut resolved = Vec::new();
                for ((repo_owner, repo_name, tag), files) in download_reqs {
                    for (file, vars) in files {
                        let key = MemoKey::new("flowey_lib_common::download_gh_release", 1)
                            .with_str("repo", format!("{repo_owner}/{repo_name}"))
                            .with_str("tag", &tag)
                            .with_str("file", &file);

                        let id = (repo_owner.clone(), repo_name.clone(), tag.clone(), file);
                        match store.lookup(&key)? {
                            Some(entry) => {
                                log::info!("memo hit: {} ({id:?})", key.hash());
                                resolved.push((entry.dir.join(&id.3), vars));
                            }
                            None => {
                                log::info!("memo miss: {} ({id:?})", key.hash());
                                misses.push((id, key, vars));
                            }
                        }
                    }
                }

                if !misses.is_empty() {
                    // fetch everything that missed into one scratch dir, so the
                    // gh cli can batch its `--pattern` args into as few
                    // invocations as possible...
                    let bulk = store.stage()?;
                    let mut regrouped: BTreeMap<(String, String, String), Vec<String>> =
                        BTreeMap::new();
                    for ((repo_owner, repo_name, tag, file), _, _) in &misses {
                        regrouped
                            .entry((repo_owner.clone(), repo_name.clone(), tag.clone()))
                            .or_default()
                            .push(file.clone());
                    }
                    download_all_reqs(rt, &regrouped, bulk.dir(), gh_cli)?;

                    // ...then split it up, so that a later run needing a
                    // different subset still gets per-file hits.
                    for (id, key, vars) in misses {
                        let (repo_owner, repo_name, tag, file) = &id;
                        let staging = store.stage()?;
                        fs_err::rename(
                            bulk.dir()
                                .join(format!("{repo_owner}/{repo_name}/{tag}/{file}")),
                            staging.dir().join(file),
                        )?;
                        let entry = store.commit(&key, staging)?;
                        resolved.push((entry.dir.join(file), vars));
                    }
                }

                for (path, vars) in resolved {
                    for var in vars {
                        rt.write(var, &path)
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
        gh_cli: Option<ReadVar<PathBuf>>,
    ) {
        let cache_dir = ctx.emit_rust_stepv("create gh-release-download cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        // Build a human-readable cache key from repo names and tags so
        // that cache entries are identifiable in the CI cache UI.
        // The hash ensures uniqueness; the descriptive prefix aids debugging.
        let cache_key = {
            use std::fmt::Write as _;

            let hasher = &mut rustc_hash::FxHasher::default();
            let mut key = String::from("gh-release-download-");
            for ((repo_owner, repo_name, tag), files) in &download_reqs {
                std::hash::Hash::hash(repo_owner, hasher);
                std::hash::Hash::hash(repo_name, hasher);
                std::hash::Hash::hash(tag, hasher);
                for file in files.keys() {
                    std::hash::Hash::hash(&file, hasher);
                }
                write!(key, "{repo_name}-{tag}_").unwrap();
            }
            let hash = std::hash::Hasher::finish(hasher);

            // Actions cache keys are limited to 512 characters total, but
            // subsequent machinery adds some more stuff to the key. Truncate
            // generously.
            key.truncate(256);
            write!(key, "{:016x}", hash).unwrap();
            ReadVar::from_static(key)
        };
        let hitvar = ctx.reqv(|v| {
            crate::cache::Request {
                label: "gh-release-download".into(),
                dir: cache_dir.clone(),
                key: cache_key,
                restore_keys: None, // OK if not exact - better than nothing
                hitvar: v,
            }
        });

        ctx.emit_rust_step("download artifacts from github releases", |ctx| {
            let cache_dir = cache_dir.claim(ctx);
            let hitvar = hitvar.claim(ctx);
            let gh_cli = gh_cli.claim(ctx);
            let download_reqs = download_reqs.claim(ctx);
            move |rt| {
                let cache_dir = rt.read(cache_dir);
                let hitvar = rt.read(hitvar);

                if !matches!(hitvar, crate::cache::CacheHit::Hit) {
                    let to_download = download_reqs
                        .iter()
                        .map(|(repo_tag, files)| {
                            (repo_tag.clone(), files.keys().cloned().collect())
                        })
                        .collect();
                    download_all_reqs(rt, &to_download, &cache_dir, gh_cli)?;
                }

                for ((repo_owner, repo_name, tag), files) in download_reqs {
                    for (file, vars) in files {
                        let file = cache_dir.join(format!("{repo_owner}/{repo_name}/{tag}/{file}"));
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
    download_reqs: &BTreeMap<(String, String, String), Vec<String>>,
    cache_dir: &Path,
    gh_cli: Option<ReadVar<PathBuf, VarClaimed>>,
) -> anyhow::Result<()> {
    let gh_cli = rt.read(gh_cli);

    for ((repo_owner, repo_name, tag), files) in download_reqs {
        let repo = format!("{repo_owner}/{repo_name}");

        let out_dir = cache_dir.join(format!("{repo_owner}/{repo_name}/{tag}"));
        fs_err::create_dir_all(&out_dir)?;
        rt.sh.change_dir(&out_dir);

        if let Some(gh_cli) = &gh_cli {
            // FUTURE: while the gh cli takes care of doing simultaneous downloads in
            // the context of a single (repo, tag), we might want to have flowey spawn
            // multiple processes to saturate the network connection in cases where
            // multiple (repo, tag) pairs are being pulled at the same time.
            let patterns = files.iter().flat_map(|k| ["--pattern".into(), k.clone()]);
            flowey::shell_cmd!(
                rt,
                "{gh_cli} release download -R {repo} {tag} {patterns...} --skip-existing"
            )
            .run()?;
        } else {
            // FUTURE: parallelize curl invocations across all download_reqs
            for file in files {
                let mut cmd = flowey::shell_cmd!(
                    rt,
                    "curl --fail -L https://github.com/{repo_owner}/{repo_name}/releases/download/{tag}/{file} -o {file}"
                );

                if matches!(rt.platform(), FlowPlatform::Windows) {
                    cmd = cmd.arg("--ssl-revoke-best-effort");
                }

                cmd.run()?;
            }
        }
    }

    Ok(())
}
