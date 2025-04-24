// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cache the contents of a particular directory between runs.
//!
//! The contents of the provided `dir` will be saved at the end of a run, using
//! the user-defined `key` string to tag the contents of the cache.
//!
//! Subsequent runs will then use the `key` to restore the contents of the
//! directory.
//!
//! # A note of file sizes
//!
//! This node is backed by the in-box Cache@2 Task on ADO, and the in-box
//! actions/cache@v3 Action on Github Actions.
//!
//! These actions have limits on the size of data they can cache at any given
//! time, and potentially have issues with particularly large artifacts (e.g:
//! gigabytes in size).
//!
//! In cases where you're intending to cache large files, it is recommended to
//! implement caching functionality directly using [`NodeCtx::persistent_dir`],
//! which is guaranteed to be reliable (when running on a system where such
//! persistent storage is available).
//!
//! # Clearing the cache
//!
//! Clearing the cache is done in different ways depending on the backend:
//!
//! - Local: just delete the cache folder on your machine
//! - Github: use the cache tasks's web UI to manage cache entries
//! - ADO: define a pipeline-level variable called `FloweyCacheGeneration`, and set
//!   it to an new arbitrary value.
//!     - This is because ADO doesn't have a native way to flush the cache
//!       outside of updating the cache key in the YAML file itself.

use flowey::node::prelude::*;
use std::collections::BTreeSet;
use std::io::Seek;
use std::io::Write;

/// Status of a cache directory.
#[derive(Debug, Serialize, Deserialize)]
pub enum CacheHit {
    /// Complete miss - cache is empty.
    Miss,
    /// Direct hit - cache is perfectly restored.
    Hit,
    /// Partial hit - cache was partially restored.
    PartialHit,
}

flowey_request! {
    pub struct Request {
        /// Friendly label for the directory being cached
        pub label: String,
        /// Absolute path to the directory that will be cached between runs
        pub dir: ReadVar<PathBuf>,
        /// The key created when saving a cache and the key used to search for a
        /// cache.
        pub key: ReadVar<String>,
        /// An optional set of alternative restore keys.
        ///
        /// If no cache hit occurs for key, these restore keys are used
        /// sequentially in the order provided to find and restore a cache.
        pub restore_keys: Option<ReadVar<Vec<String>>>,
        /// Variable to write the result of trying to restore the cache.
        pub hitvar: WriteVar<CacheHit>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        // -- end of req processing -- //

        match ctx.backend() {
            FlowBackend::Local => {
                if !ctx.supports_persistent_dir() {
                    ctx.emit_minor_rust_step("Reporting cache misses", |ctx| {
                        let hitvars = requests
                            .into_iter()
                            .map(|v| v.hitvar.claim(ctx))
                            .collect::<Vec<_>>();

                        |rt| {
                            rt.write_all(hitvars, &CacheHit::Miss);
                        }
                    });

                    return Ok(());
                };

                for Request {
                    label,
                    dir,
                    key,
                    restore_keys,
                    hitvar,
                } in requests
                {
                    // work around a bug in how post-job nodes affect stage1 day
                    // culling...
                    let persistent_dir = ctx.persistent_dir().unwrap();

                    // Needed for saving the cache result.
                    let (hitvar_reader, hitvar2) = ctx.new_var();

                    let (resolve_post_job, require_post_job) = ctx.new_post_job_side_effect();

                    ctx.emit_rust_step(format!("Restore cache: {label}"), |ctx| {
                        require_post_job.claim(ctx);
                        let persistent_dir = persistent_dir.clone().claim(ctx);
                        let dir = dir.clone().claim(ctx);
                        let key = key.clone().claim(ctx);
                        let restore_keys = restore_keys.claim(ctx);
                        let hitvar = hitvar.claim(ctx);
                        let hitvar2 = hitvar2.claim(ctx);
                        |rt| {
                            let persistent_dir = rt.read(persistent_dir);
                            let dir = rt.read(dir);
                            let key = rt.read(key);
                            let restore_keys = rt.read(restore_keys);

                            let set_hitvar = move |val| {
                                log::info!("cache status: {val:?}");
                                rt.write(hitvar, &val);
                                rt.write(hitvar2, &val);
                            };

                            // figure out what cache entries are available to us
                            //
                            // (reading this entire file into memory seems fine at
                            // this juncture, given the sort of datasets we're
                            // working with)
                            let available_keys: BTreeSet<String> = if let Ok(s) =
                                fs_err::read_to_string(persistent_dir.join("cache_keys"))
                            {
                                s.split('\n').map(|s| s.trim().to_owned()).collect()
                            } else {
                                BTreeSet::new()
                            };

                            // using the keys the user provided us, check if there's
                            // a match
                            let mut existing_cache_dir = None;
                            for (idx, key) in Some(key)
                                .into_iter()
                                .chain(restore_keys.into_iter().flatten())
                                .enumerate()
                            {
                                if available_keys.contains(&key) {
                                    existing_cache_dir = Some((idx == 0, hash_key_to_dir(&key)));
                                    break;
                                }
                            }

                            let Some((direct_hit, existing_cache_dir)) = existing_cache_dir else {
                                set_hitvar(CacheHit::Miss);
                                return Ok(());
                            };

                            crate::_util::copy_dir_all(
                                persistent_dir.join(existing_cache_dir),
                                dir,
                            )
                            .context("while restoring cache")?;

                            set_hitvar(if direct_hit {
                                CacheHit::Hit
                            } else {
                                CacheHit::PartialHit
                            });

                            Ok(())
                        }
                    });

                    ctx.emit_rust_step(format!("Saving cache: {label}"), |ctx| {
                        resolve_post_job.claim(ctx);
                        let hitvar_reader = hitvar_reader.claim(ctx);
                        let persistent_dir = persistent_dir.clone().claim(ctx);
                        let dir = dir.claim(ctx);
                        let key = key.claim(ctx);
                        move |rt| {
                            let persistent_dir = rt.read(persistent_dir);
                            let dir = rt.read(dir);
                            let key = rt.read(key);
                            let hitvar_reader = rt.read(hitvar_reader);

                            let mut cache_keys_file = fs_err::OpenOptions::new()
                                .append(true)
                                .create(true)
                                .read(true)
                                .open(persistent_dir.join("cache_keys"))?;

                            if matches!(hitvar_reader, CacheHit::Hit) {
                                // no need to update the cache
                                log::info!("was direct hit - no updates needed");
                                return Ok(());
                            }

                            // otherwise, need to update the cache
                            crate::_util::copy_dir_all(
                                dir,
                                persistent_dir.join(hash_key_to_dir(&key)),
                            )?;

                            cache_keys_file.seek(std::io::SeekFrom::End(0))?;
                            writeln!(cache_keys_file, "{}", key)?;

                            log::info!("cache saved");

                            Ok(())
                        }
                    });
                }
            }
            FlowBackend::Ado => {
                for Request {
                    label,
                    dir,
                    key,
                    restore_keys,
                    hitvar,
                } in requests
                {
                    let (resolve_post_job, require_post_job) = ctx.new_post_job_side_effect();

                    let (dir_string, key, restore_keys) = {
                        let (processed_dir, write_processed_dir) = ctx.new_var();
                        let (processed_key, write_processed_key) = ctx.new_var();
                        let (processed_keys, write_processed_keys) = if restore_keys.is_some() {
                            let (a, b) = ctx.new_var();
                            (Some(a), Some(b))
                        } else {
                            (None, None)
                        };

                        ctx.emit_rust_step("Pre-processing cache vars", |ctx| {
                            require_post_job.claim(ctx);
                            let write_processed_dir = write_processed_dir.claim(ctx);
                            let write_processed_key = write_processed_key.claim(ctx);
                            let write_processed_keys = write_processed_keys.claim(ctx);

                            let dir = dir.clone().claim(ctx);
                            let key = key.claim(ctx);
                            let restore_keys = restore_keys.claim(ctx);

                            |rt| {
                                let dir = rt.read(dir);
                                // while we're here, we'll convert dir into a
                                // String, so it can be stuffed into an ADO var
                                rt.write(
                                    write_processed_dir,
                                    &dir.absolute()?.display().to_string(),
                                );

                                // Inject `$(FloweyCacheGeneration)` as part of the
                                // cache key to provide a non-intrusive mechanism to
                                // cycle the ADO cache when it gets into an
                                // inconsistent state.
                                //
                                // Deny cross-os caching by default (matching Github
                                // CI works by default).
                                //
                                // FUTURE: add toggle to request to allow cross-os
                                // caching?
                                let inject_extras = |s| {
                                    format!(r#""$(FloweyCacheGeneration)" | "$(Agent.OS)" | "{s}""#)
                                };

                                let key = rt.read(key);
                                rt.write(write_processed_key, &inject_extras(key));

                                if let Some(write_processed_keys) = write_processed_keys {
                                    let restore_keys = rt.read(restore_keys.unwrap());
                                    rt.write(
                                        write_processed_keys,
                                        &restore_keys
                                            .into_iter()
                                            .map(inject_extras)
                                            .collect::<Vec<_>>()
                                            .join("\\n"),
                                    );
                                }

                                Ok(())
                            }
                        });

                        (processed_dir, processed_key, processed_keys)
                    };

                    let (hitvar_str_reader, hitvar_str_writer) = ctx.new_var();

                    ctx.emit_ado_step(format!("Restore cache: {label}"), |ctx| {
                        let dir_string = dir_string.clone().claim(ctx);
                        let key = key.claim(ctx);
                        let restore_keys = restore_keys.claim(ctx);
                        let hitvar_str_writer = hitvar_str_writer.claim(ctx);
                        |rt| {
                            let dir_string = rt.get_var(dir_string).as_raw_var_name();
                            let key = rt.get_var(key).as_raw_var_name();
                            let restore_keys = if let Some(restore_keys) = restore_keys {
                                format!(
                                    "restore_keys: $({})",
                                    rt.get_var(restore_keys).as_raw_var_name()
                                )
                            } else {
                                String::new()
                            };

                            let hitvar_ado =
                                AdoRuntimeVar::dangerous_from_global("FLOWEY_CACHE_HITVAR", false);
                            // note the _lack_ of $() around the var!
                            let hitvar_input =
                                format!("cacheHitVar: {}", hitvar_ado.as_raw_var_name());
                            rt.set_var(hitvar_str_writer, hitvar_ado);

                            format!(
                                r#"
                                - task: Cache@2
                                  inputs:
                                    key: '$({key})'
                                    path: $({dir_string})
                                    {restore_keys}
                                    {hitvar_input}
                            "#
                            )
                        }
                    });

                    ctx.emit_rust_step("map ADO hitvar to flowey", |ctx| {
                        let label = label.clone();
                        let dir = dir.clone().claim(ctx);

                        let hitvar = hitvar.claim(ctx);
                        let hitvar_str_reader = hitvar_str_reader.claim(ctx);
                        move |rt| {
                            let dir = rt.read(dir);
                            let hitvar_str = rt.read(hitvar_str_reader);
                            let mut var = match hitvar_str.as_str() {
                                "true" => CacheHit::Hit,
                                "false" => CacheHit::Miss,
                                "inexact" => CacheHit::PartialHit,
                                other => anyhow::bail!("unexpected cacheHitVar value: {other}"),
                            };

                            // WORKAROUND: ADO is really cool software, and
                            // sometimes, when it feels like it, i'll get into
                            // an inconsistent state where it reports a cache
                            // hit, but then the cache is actually empty.
                            if matches!(var, CacheHit::Hit | CacheHit::PartialHit) {
                                if dir.read_dir()?.next().is_none() {
                                    log::error!("Detected inconsistent ADO cache entry: {label}");
                                    log::error!("Please define/cycle the `FloweyCacheGeneration` pipeline variable");
                                    var = CacheHit::Miss;
                                }
                            }

                            rt.write(hitvar, &var);
                            Ok(())
                        }
                    });

                    ctx.emit_rust_step(format!("validate cache entry: {label}"), |ctx| {
                        resolve_post_job.claim(ctx);
                        let dir = dir.clone().claim(ctx);
                        move |rt| {
                            let mut dir_contents = rt.read(dir).read_dir()?.peekable();

                            if dir_contents.peek().is_none() {
                                log::error!("Detected empty cache folder for entry: {label}");
                                log::error!("This is a bug - please update the pipeline code");
                                anyhow::bail!("cache error")
                            }

                            for entry in dir_contents {
                                let entry = entry?;
                                log::debug!("uploading: {}", entry.path().display());
                            }

                            Ok(())
                        }
                    });
                }
            }
            FlowBackend::Github => {
                for Request {
                    label,
                    dir,
                    key,
                    restore_keys,
                    hitvar,
                } in requests
                {
                    let (resolve_post_job, require_post_job) = ctx.new_post_job_side_effect();

                    let (dir_string, key, restore_keys) = {
                        let (processed_dir, write_processed_dir) = ctx.new_var();
                        let (processed_key, write_processed_key) = ctx.new_var();
                        let (processed_keys, write_processed_keys) = if restore_keys.is_some() {
                            let (a, b) = ctx.new_var();
                            (Some(a), Some(b))
                        } else {
                            (None, None)
                        };

                        ctx.emit_rust_step("Pre-processing cache vars", |ctx| {
                            require_post_job.claim(ctx);
                            let write_processed_dir = write_processed_dir.claim(ctx);
                            let write_processed_key = write_processed_key.claim(ctx);
                            let write_processed_keys = write_processed_keys.claim(ctx);

                            let dir = dir.clone().claim(ctx);
                            let key = key.claim(ctx);
                            let restore_keys = restore_keys.claim(ctx);

                            |rt| {
                                let dir = rt.read(dir);
                                rt.write(
                                    write_processed_dir,
                                    &dir.absolute()?.display().to_string(),
                                );

                                let key = rt.read(key);
                                let key = format!("{key}-{}-{}", rt.arch(), rt.platform());
                                rt.write(write_processed_key, &key);

                                if let Some(write_processed_keys) = write_processed_keys {
                                    let restore_keys = rt.read(restore_keys.unwrap());
                                    rt.write(
                                        write_processed_keys,
                                        &format!(
                                            r#""[{}]""#,
                                            restore_keys
                                                .into_iter()
                                                .map(|s| format!(
                                                    "'{s}-{}-{}'",
                                                    rt.arch(),
                                                    rt.platform()
                                                ))
                                                .collect::<Vec<_>>()
                                                .join(", ")
                                        ),
                                    );
                                }

                                Ok(())
                            }
                        });

                        (processed_dir, processed_key, processed_keys)
                    };

                    let (hitvar_str_reader, hitvar_str_writer) = ctx.new_var();

                    let mut step = ctx
                        .emit_gh_step(format!("Restore cache: {label}"), "actions/cache@v4")
                        .with("key", key)
                        .with("path", dir_string);
                    if let Some(restore_keys) = restore_keys {
                        step = step.with("restore-keys", restore_keys);
                    }
                    step.output("cache-hit", hitvar_str_writer).finish(ctx);

                    ctx.emit_minor_rust_step("map Github cache-hit to flowey", |ctx| {
                        let hitvar = hitvar.claim(ctx);
                        let hitvar_str_reader = hitvar_str_reader.claim(ctx);
                        // TODO: How do we distinguish between a partial hit and a miss?
                        move |rt| {
                            let hitvar_str = rt.read(hitvar_str_reader);
                            // Github's cache action brilliantly only reports "false" if missing a cache key that exists,
                            // and leaves it blank if its a miss in other cases.
                            let var = match hitvar_str.as_str() {
                                "true" => CacheHit::Hit,
                                _ => CacheHit::Miss,
                            };

                            rt.write(hitvar, &var);
                        }
                    });

                    ctx.emit_rust_step(format!("validate cache entry: {label}"), |ctx| {
                        resolve_post_job.claim(ctx);
                        let dir = dir.clone().claim(ctx);
                        move |rt| {
                            let mut dir_contents = rt.read(dir).read_dir()?.peekable();

                            if dir_contents.peek().is_none() {
                                log::error!("Detected empty cache folder for entry: {label}");
                                log::error!("This is a bug - please update the pipeline code");
                                anyhow::bail!("cache error")
                            }

                            for entry in dir_contents {
                                let entry = entry?;
                                log::debug!("uploading: {}", entry.path().display());
                            }

                            Ok(())
                        }
                    });
                }
            }
        }

        Ok(())
    }
}

// _technically_, if we want to be _super_ sure we're not gonna have a hash
// collision, we should also do a content-hash of the thing we're about to
// cache... but this should be OK for now, given that we don't expect to have a
// massive number of cache entries.
fn hash_key_to_dir(key: &str) -> String {
    let hasher = &mut rustc_hash::FxHasher::default();
    std::hash::Hash::hash(&key, hasher);
    let hash = std::hash::Hasher::finish(hasher);
    format!("{:08x?}", hash)
}
