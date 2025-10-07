// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Cmd;
use clap::Parser;
use clap::Subcommand;

#[derive(clap::ValueEnum, Clone)]
pub enum Generate {
    Overlay,
    Base,
}

#[derive(Subcommand)]
pub enum Command {
    /// Regenerate a new Cargo.lock file using two existing
    /// `Cargo.*-external-deps.lock` files.
    Regen,
    /// Regenerate a `Cargo.*-external-deps.lock` file
    GenExternal {
        /// Which external.lock file to generate
        which: Generate,
    },
}

#[derive(Parser)]
#[clap(
    about = "Tools to keep external dependencies in Cargo.lock files in-sync",
    disable_help_subcommand = true
)]
pub struct CargoLock {
    #[clap(subcommand)]
    pub cmd: Command,
}

impl Cmd for CargoLock {
    fn run(self, ctx: crate::CmdCtx) -> anyhow::Result<()> {
        let Self { cmd } = self;

        match cmd {
            Command::GenExternal { which } => {
                let (cargo_lock_path, cargo_external_lock_path) = match which {
                    Generate::Overlay => (
                        ctx.overlay_workspace.join("Cargo.lock"),
                        ctx.overlay_workspace
                            .join("Cargo.overlay.external-deps.lock"),
                    ),
                    Generate::Base => (
                        ctx.base_workspace.join("Cargo.lock"),
                        ctx.overlay_workspace.join("Cargo.base.external-deps.lock"),
                    ),
                };

                let cargo_lock = cargo_lock::Lockfile::load(cargo_lock_path)?;

                let mut external_packages = Vec::new();
                for package in cargo_lock.packages {
                    let cargo_lock::Package {
                        name,
                        version,
                        source,
                        checksum,
                        dependencies: _, // not relevant to Cargo.overlay.external-deps.lock
                        replace,
                    } = package;

                    assert!(replace.is_none(), "no support for `replace` directives");

                    let Some(source) = source else {
                        // this is a local dep, and doesn't need to be represented in
                        // `Cargo.overlay.external-deps.lock`
                        continue;
                    };

                    external_packages.push(cargo_external_lock::ExternalPackage {
                        name,
                        version,
                        source,
                        checksum,
                    });
                }

                let generated = format!(
                    "{}{}",
                    super::GENERATED_HEADER.trim_start(),
                    toml_edit::ser::to_string_pretty(&cargo_external_lock::ExternalLockfile {
                        package: external_packages,
                    })?
                );

                if ctx.check {
                    let existing = fs_err::read_to_string(&cargo_external_lock_path)?;

                    if generated != existing {
                        anyhow::bail!(
                            "{} is out of date!",
                            std::path::absolute(cargo_external_lock_path)?.display()
                        )
                    }
                } else {
                    fs_err::write(cargo_external_lock_path, generated.as_bytes())?;
                }

                Ok(())
            }
            Command::Regen => {
                let out = ctx.overlay_workspace.join("Cargo.lock");
                let base = ctx
                    .overlay_workspace
                    .join("Cargo.overlay.external-deps.lock");
                let overlay = ctx.overlay_workspace.join("Cargo.base.external-deps.lock");

                let mut base = {
                    let data = fs_err::read_to_string(base)?;
                    let data: cargo_external_lock::ExternalLockfile =
                        toml_edit::de::from_str(&data)?;
                    data
                };

                let mut overlay = {
                    let data = fs_err::read_to_string(overlay)?;
                    let data: cargo_external_lock::ExternalLockfile =
                        toml_edit::de::from_str(&data)?;
                    data
                };

                (base.package).sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
                (overlay.package).sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));

                let mut mps = Vec::new();
                let mut bps = base.package.into_iter().peekable();
                let mut ops = overlay.package.into_iter().peekable();

                macro_rules! next {
                    ($e:expr) => {
                        $e.next().unwrap()
                    };
                }

                macro_rules! is_upgrade {
                    ($a:expr => $b:expr) => {
                        semver::VersionReq::parse(&$a.version.to_string())
                            .unwrap()
                            .matches(&$b.version)
                    };
                }

                loop {
                    match (bps.peek(), ops.peek()) {
                        // base case - we're done
                        (None, None) => break,
                        // when there's only one iterator left - just drain it
                        (None, Some(_)) => mps.extend(&mut ops),
                        (Some(_), None) => mps.extend(&mut bps),
                        // merge iterators until there is a name match
                        (Some(bp), Some(op)) if bp.name < op.name => mps.push(next!(bps)),
                        (Some(bp), Some(op)) if bp.name > op.name => mps.push(next!(ops)),
                        // the two packages have the same name, so we need to
                        // consider semver
                        (Some(bp), Some(op)) => {
                            assert_eq!(bp.name, op.name);
                            let name = bp.name.clone();

                            let bps = std::iter::from_fn(|| bps.next_if(|p| p.name == name));
                            let ops = std::iter::from_fn(|| ops.next_if(|p| p.name == name));

                            let mut v = Vec::new();
                            // tag each package with an indication if its from
                            // the base or the overlay
                            v.extend(bps.map(|x| (x, false)));
                            v.extend(ops.map(|x| (x, true)));
                            // sort by descending order of version, with
                            // preference to overlay packages.
                            v.sort_by(|(a, ax), (b, bx)| {
                                (&a.version, ax).cmp(&(&b.version, bx)).reverse()
                            });
                            // de-dupe identical versions, with preference to
                            // the overlay's package (so git-based deps track
                            // the overlay).
                            v.dedup_by(|(a, _), (b, _)| a == b);
                            // next, de-dupe any "runs" of semver compatible
                            // versions to the latest version
                            //
                            // ideally, this is something cargo would do itself
                            // during regeneration... but for unclear reasons,
                            // it will always select the oldest semver
                            // compatible version.
                            v.dedup_by(|(a, _), (b, _)| is_upgrade!(a => b));

                            // reverse the order, to packages in ascending order
                            mps.extend(v.into_iter().rev().map(|(x, _)| x))
                        }
                    }
                }

                let new_lockfile = cargo_lock::Lockfile {
                    version: cargo_lock::ResolveVersion::V3,
                    packages: mps
                        .into_iter()
                        .map(
                            |cargo_external_lock::ExternalPackage {
                                 name,
                                 version,
                                 source,
                                 checksum,
                             }| cargo_lock::Package {
                                name,
                                version,
                                source: Some(source),
                                checksum,
                                dependencies: Vec::new(),
                                replace: None,
                            },
                        )
                        .collect(),
                    root: None,
                    metadata: Default::default(),
                    patch: Default::default(),
                };

                let old_lock = if ctx.check {
                    fs_err::read_to_string(&out).ok()
                } else {
                    None
                };

                fs_err::write(
                    &out,
                    toml_edit::ser::to_string_pretty(&new_lockfile)?.as_bytes(),
                )?;
                let out = std::path::absolute(out)?;

                let cargo_update = |offline: bool| {
                    std::process::Command::new("cargo")
                        .arg("update")
                        .arg("--workspace")
                        .arg("--quiet")
                        .args(offline.then_some("--offline"))
                        .stderr(if offline {
                            std::process::Stdio::null()
                        } else {
                            std::process::Stdio::inherit()
                        })
                        .current_dir(&ctx.overlay_workspace)
                        .status()
                };

                if !cargo_update(true)?.success() {
                    // Try again without `--offline` in case the registry index
                    // needs to be updated.
                    let status = cargo_update(false)?;
                    if !status.success() {
                        anyhow::bail!("cargo update failed with status: {}", status);
                    }
                }

                if ctx.check {
                    let new_lock = fs_err::read_to_string(&out)?;
                    if old_lock.as_ref() != Some(&new_lock) {
                        if let Some(old_lock) = old_lock {
                            fs_err::write(&out, old_lock.as_bytes())?;
                        }
                        anyhow::bail!("{} is out-of-date!", out.display())
                    }
                }

                Ok(())
            }
        }
    }
}

mod cargo_external_lock {
    use serde::Deserialize;
    use serde::Serialize;

    /// Root type for `Cargo.overlay.external-deps.lock` files
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ExternalLockfile {
        pub package: Vec<ExternalPackage>,
    }

    /// Simplified version of [`cargo_lock::Package`] for use on
    /// `Cargo.overlay.external-deps.lock` files.
    ///
    /// Only supports encoding external (i.e: not `path` based) dependencies,
    /// and removes certain unnecessary / deprecated fields.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct ExternalPackage {
        /// Name of the package
        pub name: cargo_lock::Name,
        /// Version of the package
        pub version: cargo_lock::Version,
        /// Source identifier for the package
        pub source: cargo_lock::SourceId,
        /// Checksum for this package
        pub checksum: Option<cargo_lock::Checksum>,
    }
}
