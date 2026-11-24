// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download NuGet packages using `dotnet restore` with a synthetic `.csproj`.
//!
//! On CI (ADO/GitHub), relies on ambient pipeline credentials (set by
//! `NuGetAuthenticate@1` or equivalent).
//! Locally, uses `az account get-access-token` to obtain an Azure DevOps
//! bearer token, exchanges it for a session token via the Azure DevOps
//! REST API, and passes it to the NuGet credential provider via the
//! `VSS_NUGET_EXTERNAL_FEED_ENDPOINTS` environment variable.
//!
//! The restored packages are cached (via [`crate::cache`], keyed on the
//! requested `(id, version)` set and effective NuGet config contents) so that
//! repeated restores can skip `dotnet restore` and, on Local, the auth dance.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub struct NugetPackage {
    pub id: String,
    pub version: String,
}

fn nuget_restore_cache_key(packages: Vec<NugetPackage>, config: &str) -> anyhow::Result<String> {
    let mut packages: Vec<_> = packages
        .into_iter()
        .map(|NugetPackage { id, version }| (id.to_lowercase(), version))
        .collect();
    packages.sort();
    packages.dedup();

    let hasher = &mut rustc_hash::FxHasher::default();
    for (id, version) in &packages {
        std::hash::Hash::hash(id, hasher);
        std::hash::Hash::hash(version, hasher);
    }
    let config = parse_nuget_config(config)?;
    std::hash::Hash::hash(&config.filtered_config, hasher);
    let hash = std::hash::Hasher::finish(hasher);

    Ok(format!("nuget-install-package-{hash:016x}"))
}

flowey_request! {
    pub enum Request {
        /// A bundle of packages to install in one dotnet restore invocation
        Install {
            /// Path to a nuget.config file
            nuget_config_file: ReadVar<PathBuf>,
            /// A list of nuget packages to install, and outvars denoting where they
            /// were extracted to.
            packages: Vec<(ReadVar<NugetPackage>, WriteVar<PathBuf>)>,
            /// Directory to install the packages into.
            install_dir: ReadVar<PathBuf>,
            /// Side effects that must have run before installing these packages.
            ///
            /// e.g: requiring that a nuget credentials manager has been installed
            pre_install_side_effects: Vec<ReadVar<SideEffect>>,
        },
    }
}

struct InstallRequest {
    nuget_config_file: ReadVar<PathBuf>,
    packages: Vec<(ReadVar<NugetPackage>, WriteVar<PathBuf>)>,
    install_dir: ReadVar<PathBuf>,
    pre_install_side_effects: Vec<ReadVar<SideEffect>>,
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<super::install_dotnet_cli::Node>();
        ctx.import::<crate::cache::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut install = Vec::new();

        for request in requests {
            match request {
                Request::Install {
                    packages,
                    nuget_config_file,
                    install_dir,
                    pre_install_side_effects,
                } => {
                    if packages.is_empty() {
                        continue;
                    }
                    install.push(InstallRequest {
                        packages,
                        nuget_config_file,
                        install_dir,
                        pre_install_side_effects,
                    });
                }
            }
        }

        // -- end of req processing -- //

        if install.is_empty() {
            return Ok(());
        }

        Self::emit_dotnet_restore(ctx, install)
    }
}

impl Node {
    /// Use `dotnet restore` with a synthetic `.csproj` containing
    /// `PackageDownload` items.
    ///
    /// On Local, obtains a session token from `az` CLI and passes it
    /// to the credential provider via `VSS_NUGET_EXTERNAL_FEED_ENDPOINTS`.
    /// On CI, relies on ambient pipeline credentials (set by
    /// `NuGetAuthenticate@1` or equivalent).
    fn emit_dotnet_restore(
        ctx: &mut NodeCtx<'_>,
        install: Vec<InstallRequest>,
    ) -> anyhow::Result<()> {
        let dotnet_bin = ctx.reqv(super::install_dotnet_cli::Request::DotnetBin);

        for InstallRequest {
            packages,
            nuget_config_file,
            install_dir,
            pre_install_side_effects,
        } in install
        {
            // Resolve packages and config at runtime before looking up the cache.
            let package_reads: Vec<_> = packages.iter().map(|(p, _)| p.clone()).collect();
            let cache_key = ctx.emit_rust_stepv("compute nuget restore cache key", |ctx| {
                let package_reads = package_reads.claim(ctx);
                let nuget_config_file = nuget_config_file.clone().claim(ctx);
                move |rt| {
                    let pkgs: Vec<NugetPackage> =
                        package_reads.into_iter().map(|p| rt.read(p)).collect();
                    let config = fs_err::read_to_string(rt.read(nuget_config_file))?;
                    nuget_restore_cache_key(pkgs, &config)
                }
            });

            // A directory that `crate::cache` restores into / saves from.
            // On Local, this is transparently backed by the node's
            // persistent dir; on CI, by the platform's native cache action.
            // Either way, a cache hit means the packages are already sitting
            // here, laid out exactly how `dotnet restore --packages`
            // produces them ({id_lower}/{version_lower}/).
            let restore_packages_dir = ctx
                .emit_rust_stepv("create nuget restore cache dir", |_| {
                    |_| Ok(std::env::current_dir()?.join("packages").absolute()?)
                });

            let cache_hit = ctx.reqv(|v| crate::cache::Request {
                label: "nuget-install-package".into(),
                dir: restore_packages_dir.clone(),
                key: cache_key,
                restore_keys: None, // the package set and config must match exactly
                hitvar: v,
            });

            ctx.emit_rust_step("restore nuget packages", |ctx| {
                let dotnet_bin = dotnet_bin.clone().claim(ctx);
                let install_dir = install_dir.claim(ctx);
                let restore_packages_dir = restore_packages_dir.claim(ctx);
                let cache_hit = cache_hit.claim(ctx);
                pre_install_side_effects.claim(ctx);

                let packages = packages
                    .into_iter()
                    .map(|(a, b)| (a.claim(ctx), b.claim(ctx)))
                    .collect::<Vec<_>>();
                let nuget_config_file = nuget_config_file.claim(ctx);

                move |rt| {
                    let dotnet_bin = rt.read(dotnet_bin);
                    let nuget_config_file = rt.read(nuget_config_file);
                    let install_dir = rt.read(install_dir);
                    let restore_packages_dir = rt.read(restore_packages_dir);
                    let cache_hit = rt.read(cache_hit);

                    let packages = {
                        let mut pkgmap: BTreeMap<_, Vec<_>> = BTreeMap::new();
                        for (package, var) in packages {
                            pkgmap.entry(rt.read(package)).or_default().push(var);
                        }
                        pkgmap
                    };

                    if matches!(cache_hit, crate::cache::CacheHit::Hit) {
                        log::info!(
                            "nuget restore cache hit - skipping `dotnet restore` (and any auth)"
                        );
                    } else {
                        Self::run_dotnet_restore(
                            rt,
                            &dotnet_bin,
                            &nuget_config_file,
                            &restore_packages_dir,
                            packages.keys(),
                        )?;
                    }

                    // Flatten from the dotnet restore layout
                    // ({id_lower}/{version}/) into the expected layout
                    // ({original_case_id}/) in install_dir.
                    //
                    // dotnet restore stores packages with lowercased IDs, but
                    // downstream code expects original-case directory names.
                    //
                    // This always *copies* (never moves) out of
                    // `restore_packages_dir`, since that directory is also
                    // what `crate::cache` persists between runs - moving out
                    // of it would mean nothing gets cached.
                    fs_err::create_dir_all(&install_dir)?;

                    for (package, package_out_dir) in packages {
                        let pkg_id_lower = package.id.to_lowercase();
                        let version_lower = package.version.to_lowercase();
                        let src_dir = restore_packages_dir
                            .join(&pkg_id_lower)
                            .join(&version_lower);

                        let dest_dir = install_dir.join(&package.id);

                        if dest_dir.exists() {
                            // Remove any previous version.
                            fs_err::remove_dir_all(&dest_dir)?;
                        }

                        if src_dir.exists() {
                            crate::_util::copy_dir_all(&src_dir, &dest_dir)?;
                        } else {
                            anyhow::bail!(
                                "Package '{}' version '{}' was not found in restore output at '{}'",
                                package.id,
                                package.version,
                                src_dir.display()
                            );
                        }

                        let dest_abs = dest_dir.absolute()?;
                        for var in package_out_dir {
                            rt.write(var, &dest_abs);
                        }
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }

    /// Run `dotnet restore` against a synthetic `.csproj`, landing the
    /// restored packages directly in `restore_packages_dir` (in dotnet
    /// restore's native `{id_lower}/{version_lower}/` layout).
    fn run_dotnet_restore<'a>(
        rt: &mut RustRuntimeServices<'_>,
        dotnet_bin: &Path,
        nuget_config_file: &Path,
        restore_packages_dir: &Path,
        packages: impl Iterator<Item = &'a NugetPackage>,
    ) -> anyhow::Result<()> {
        // Generate a synthetic .csproj with PackageDownload items.
        // PackageDownload downloads the exact nupkg without resolving
        // transitive dependencies — this is intentional, as these
        // packages are standalone native binaries / firmware blobs
        // that do not have NuGet transitive dependencies.
        //
        // The project is never compiled, so all implicit framework
        // references / targeting + runtime pack downloads are
        // disabled. Without this, an SDK newer than the
        // `TargetFramework` below would try to restore the matching
        // targeting packs (e.g. `Microsoft.NETCore.App.Ref`) from
        // the configured feeds, which typically don't mirror them.
        let csproj_content = {
            let items: String = packages
                .map(|NugetPackage { id, version }| {
                    format!(r#"    <PackageDownload Include="{id}" Version="[{version}]" />"#)
                })
                .collect::<Vec<_>>()
                .join("\n");

            format!(
                r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <DisableImplicitFrameworkReferences>true</DisableImplicitFrameworkReferences>
    <EnableTargetingPackDownload>false</EnableTargetingPackDownload>
    <EnableRuntimePackDownload>false</EnableRuntimePackDownload>
    <EnableAppHostPackDownload>false</EnableAppHostPackDownload>
  </PropertyGroup>
  <ItemGroup>
{items}
  </ItemGroup>
</Project>
"#
            )
        };

        log::debug!("generated .csproj:\n{}", csproj_content);

        // Write the synthetic project to a unique temp directory so we
        // don't pollute the repo and avoid collisions with concurrent runs.
        // Only the .csproj/.config live here - packages are restored
        // straight into `restore_packages_dir`, which is not temporary.
        let restore_work_dir = tempfile::tempdir()?;
        let restore_work_dir_path = restore_work_dir.path();

        let csproj_path = restore_work_dir_path.join("NuGetRestore.csproj");
        fs_err::write(&csproj_path, csproj_content)?;

        fs_err::create_dir_all(restore_packages_dir)?;

        // Copy the nuget.config alongside the .csproj so dotnet
        // picks it up automatically, filtering out the
        // packages.config-era `repositoryPath` setting
        // that lives under `<config>` and conflicts with
        // the `--packages` flag we pass to `dotnet restore`.
        let local_nuget_config = restore_work_dir_path.join("nuget.config");
        let config_content = fs_err::read_to_string(nuget_config_file)?;
        let parsed = parse_nuget_config(&config_content)?;
        fs_err::write(&local_nuget_config, &parsed.filtered_config)?;

        // On the Local backend, obtain an Azure DevOps session
        // token from `az` CLI and pass it to the credential
        // provider via the VSS_NUGET_EXTERNAL_FEED_ENDPOINTS
        // env var (the same mechanism ADO CI uses).
        let feed_endpoints_json = if matches!(rt.backend(), FlowBackend::Local) {
            get_feed_endpoints_json(rt, parsed.feed_urls)?
        } else {
            None
        };

        let mut cmd = flowey::shell_cmd!(
            rt,
            "{dotnet_bin} restore {csproj_path} --packages {restore_packages_dir} --configfile {local_nuget_config}"
        );
        if let Some(json) = &feed_endpoints_json {
            cmd = cmd.env("VSS_NUGET_EXTERNAL_FEED_ENDPOINTS", json);
        }
        if let Err(e) = cmd.run() {
            if matches!(rt.backend(), FlowBackend::Local) {
                if feed_endpoints_json.is_some() {
                    log::error!(
                        "HINT: NuGet restore failed while using Azure DevOps feeds. \
                         You may need to install the Azure Artifacts Credential \
                         Provider and/or log in with `az login` to refresh your \
                         credentials."
                    );
                } else {
                    log::error!(
                        "HINT: NuGet restore failed. Check the restore output above \
                         for details and ensure your NuGet feeds are accessible from \
                         this environment."
                    );
                }
            }
            return Err(e.into());
        }

        Ok(())
    }
}

/// Parsed nuget.config with `repositoryPath` entries removed and
/// feed URLs extracted.
struct ParsedNugetConfig {
    /// The nuget.config content with `<add key="repositoryPath" …/>`
    /// entries under `<config>` removed.
    filtered_config: String,
    /// Feed URLs from `<packageSources>`.
    feed_urls: Vec<String>,
}

/// Parse a nuget.config file, stripping `repositoryPath` settings from
/// `<config>` sections (they conflict with `dotnet restore --packages`)
/// and extracting feed URLs from `<packageSources>`.
fn parse_nuget_config(config_content: &str) -> anyhow::Result<ParsedNugetConfig> {
    let doc = roxmltree::Document::parse(config_content)
        .map_err(|e| anyhow::anyhow!("failed to parse nuget.config: {e}"))?;

    // Find lines containing `<add key="repositoryPath" …/>`
    // that are direct children of a `<config>` element.
    let lines_to_remove: std::collections::HashSet<usize> = doc
        .descendants()
        .filter(|node| {
            node.tag_name().name() == "add"
                && node
                    .parent()
                    .is_some_and(|p| p.tag_name().name() == "config")
                && node
                    .attribute("key")
                    .is_some_and(|k| k.eq_ignore_ascii_case("repositorypath"))
        })
        .map(|node| {
            // Convert byte offset to 0-based line index.
            config_content[..node.range().start]
                .bytes()
                .filter(|&b| b == b'\n')
                .count()
        })
        .collect();

    let feed_urls: Vec<String> = doc
        .descendants()
        .filter(|node| {
            node.tag_name().name() == "add"
                && node
                    .parent()
                    .is_some_and(|p| p.tag_name().name() == "packageSources")
        })
        .filter_map(|node| node.attribute("value").map(String::from))
        .collect();

    let filtered_config = if lines_to_remove.is_empty() {
        config_content.to_owned()
    } else {
        config_content
            .lines()
            .enumerate()
            .filter(|(i, _)| !lines_to_remove.contains(i))
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n")
    };

    Ok(ParsedNugetConfig {
        filtered_config,
        feed_urls,
    })
}

/// Obtain an Azure DevOps session token via `az` CLI and build the
/// `VSS_NUGET_EXTERNAL_FEED_ENDPOINTS` JSON for the credential provider.
///
/// This uses the same env var that ADO's `NuGetAuthenticate@1` task sets
/// in CI pipelines — the credential provider reads it and supplies the
/// credentials to `dotnet restore` transparently.
///
/// Why not just let the credential provider authenticate interactively?
/// Because many orgs enforce Conditional Access Policies that block MSAL
/// interactive auth from non-compliant devices (like WSL). The `az` CLI
/// works because it runs on the Windows host (via WSL interop), which is
/// already authenticated and compliant.
///
/// The flow:
/// 1. `az account get-access-token --resource 499b84ac-...` → JWT bearer
/// 2. Exchange the JWT for a session token via the Azure DevOps REST API
/// 3. Build the `VSS_NUGET_EXTERNAL_FEED_ENDPOINTS` JSON with the token
///
/// Returns `None` if no Azure DevOps feeds are found in the nuget.config.
fn get_feed_endpoints_json(
    rt: &mut RustRuntimeServices<'_>,
    feed_urls: Vec<String>,
) -> anyhow::Result<Option<String>> {
    // Filter to Azure DevOps feeds first — avoid requiring az/curl when the
    // config only contains public or third-party feeds (e.g. nuget.org).
    let ado_feeds: Vec<String> = feed_urls
        .into_iter()
        .filter(|url| is_azure_devops_feed(url))
        .collect();

    if ado_feeds.is_empty() {
        log::info!("no Azure DevOps feeds found in nuget.config, skipping auth");
        return Ok(None);
    }

    // Resolve the `az` CLI binary. We use `which` instead of a bare "az"
    // because on Windows the CLI is installed as `az.cmd` and Rust's
    // Command does not consult PATHEXT to find it.
    let az_cli_bin = which::which("az").map_err(|_| {
        anyhow::anyhow!(
            "`az` CLI not found on PATH. \
             Install the Azure CLI and run `az login` to authenticate."
        )
    })?;

    // 1. Get a bearer token from az CLI.
    // The resource ID 499b84ac-1321-427f-aa17-267ca6975798 is Azure DevOps.
    // The output contains a credential, so mark the command as secret to
    // prevent it from appearing in process listings / logs.
    let bearer_token = flowey::shell_cmd!(
        rt,
        "{az_cli_bin} account get-access-token --resource 499b84ac-1321-427f-aa17-267ca6975798 --query accessToken -o tsv"
    )
    .secret()
    .read()
    .map_err(|e| anyhow::anyhow!(
        "failed to get Azure DevOps access token from `az` CLI. \
         Ensure you are logged in with `az login`. Error: {e}"
    ))?;

    if bearer_token.is_empty() {
        anyhow::bail!(
            "az CLI returned an empty access token. \
             Ensure you are logged in with `az login`."
        );
    }

    // 2. Exchange the bearer token for a short-lived session token.
    // Session tokens work with NuGet's Basic auth (unlike JWT bearer tokens).
    let session_token_body = serde_json::json!({
        "scope": "vso.packaging",
        "displayName": "flowey-nuget-restore",
    });

    // Pass the Authorization header via stdin (`-K -`) so the bearer
    // token never appears in process argument lists (visible via `ps`).
    let session_response = flowey::shell_cmd!(
        rt,
        "curl -s --fail -X POST https://app.vssps.visualstudio.com/_apis/token/sessiontokens?api-version=5.0-preview.1 -H Content-Type:application/json -K -"
    )
    .stdin(format!("header = \"Authorization: Bearer {bearer_token}\""))
    .arg("-d")
    .arg(session_token_body.to_string())
    .secret()
    .read()
    .map_err(|e| anyhow::anyhow!("failed to exchange bearer token for session token: {e}"))?;

    let session_json: serde_json::Value = serde_json::from_str(&session_response)
        .map_err(|_| anyhow::anyhow!("failed to parse session token response from Azure DevOps"))?;

    let session_token = session_json["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("session token response missing 'token' field"))?;

    log::info!("obtained Azure DevOps session token for nuget auth");

    // 3. Build the VSS_NUGET_EXTERNAL_FEED_ENDPOINTS JSON.
    // This is the same format that NuGetAuthenticate@1 uses in ADO CI.
    let endpoints: Vec<serde_json::Value> = ado_feeds
        .iter()
        .map(|url| {
            serde_json::json!({
                "endpoint": url,
                "username": "AzureDevOps",
                "password": session_token,
            })
        })
        .collect();

    let feed_json = serde_json::json!({
        "endpointCredentials": endpoints,
    })
    .to_string();

    Ok(Some(feed_json))
}

/// Check whether a feed URL is an Azure DevOps Artifacts feed.
fn is_azure_devops_feed(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("pkgs.dev.azure.com") || lower.contains(".pkgs.visualstudio.com")
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn cache_key_includes_nuget_config() -> anyhow::Result<()> {
        let package = NugetPackage {
            id: "Example".into(),
            version: "1.0".into(),
        };
        let packages = vec![package.clone()];

        assert_ne!(
            nuget_restore_cache_key(packages.clone(), "<source>A</source>")?,
            nuget_restore_cache_key(packages.clone(), "<source>B</source>")?,
        );
        assert_eq!(
            nuget_restore_cache_key(packages.clone(), "<source>A</source>")?,
            nuget_restore_cache_key(vec![package.clone(), package], "<source>A</source>")?,
        );
        assert_eq!(
            nuget_restore_cache_key(packages, "<source>A</source>")?,
            nuget_restore_cache_key(
                vec![NugetPackage {
                    id: "example".into(),
                    version: "1.0".into(),
                }],
                "<source>A</source>",
            )?,
        );
        Ok(())
    }

    #[test]
    fn cache_key_ignores_repository_path() -> anyhow::Result<()> {
        let config = "<configuration>\n<config>\n<add key=\"repositoryPath\" value=\"old\" />\n</config>\n</configuration>";
        let changed = config.replace("value=\"old\"", "value=\"new\"");
        assert_eq!(
            nuget_restore_cache_key(vec![], config)?,
            nuget_restore_cache_key(vec![], &changed)?,
        );
        Ok(())
    }
}
