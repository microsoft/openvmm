// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Encapsulates the logic of both invoking `cargo build`, and tracking where
//! built artifacts are emitted (which varies depending on the crate's type and
//! platform).
//!
//! Takes into account bits of "global" configuration and dependency management,
//! such as setting global cargo flags (e.g: --verbose, --locked), ensuring any
//! required Rust dependencies are installed (i.e: toolchain, triples), etc...

use crate::_util::cargo_output;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone)]
pub enum CargoBuildProfile {
    /// Project-specific profile (will only work is profile is set up correctly
    /// in the project's `Cargo.toml` file).
    Custom(String),
    Debug,
    Release,
}

impl CargoBuildProfile {
    pub fn from_release(value: bool) -> Self {
        match value {
            true => CargoBuildProfile::Release,
            false => CargoBuildProfile::Debug,
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Default)]
pub enum CargoFeatureSet {
    All,
    Specific(Vec<String>),
    #[default]
    None,
}

impl<T, const N: usize> From<[T; N]> for CargoFeatureSet
where
    T: AsRef<str>,
{
    fn from(value: [T; N]) -> Self {
        Self::from(value.as_slice())
    }
}

impl<T> From<&[T]> for CargoFeatureSet
where
    T: AsRef<str>,
{
    fn from(value: &[T]) -> Self {
        CargoFeatureSet::Specific(value.iter().map(|s| s.as_ref().to_owned()).collect())
    }
}

impl CargoFeatureSet {
    pub fn to_cargo_arg_strings(&self) -> Vec<String> {
        match self {
            CargoFeatureSet::All => vec!["--all-features".into()],
            CargoFeatureSet::Specific(features) => {
                if features.is_empty() {
                    vec![]
                } else {
                    vec!["--features".into(), features.join(",")]
                }
            }
            CargoFeatureSet::None => vec![],
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum CargoCrateType {
    Bin,
    StaticLib,
    DynamicLib,
}

impl CargoCrateType {
    fn as_str(&self) -> &str {
        match self {
            CargoCrateType::Bin => "bin",
            CargoCrateType::StaticLib => "staticlib",
            CargoCrateType::DynamicLib => "cdylib",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub enum CargoBuildOutput {
    WindowsBin {
        exe: PathBuf,
        pdb: PathBuf,
    },
    ElfBin {
        bin: PathBuf,
    },
    LinuxStaticLib {
        a: PathBuf,
    },
    LinuxDynamicLib {
        so: PathBuf,
    },
    WindowsStaticLib {
        lib: PathBuf,
        pdb: PathBuf,
    },
    WindowsDynamicLib {
        dll: PathBuf,
        dll_lib: PathBuf,
        pdb: PathBuf,
    },
    UefiBin {
        efi: PathBuf,
        pdb: PathBuf,
    },
}

flowey_request! {
    pub struct Request {
        pub in_folder: ReadVar<PathBuf>,
        pub crate_name: String,
        pub out_name: String,
        pub profile: CargoBuildProfile,
        pub features: CargoFeatureSet,
        pub output_kind: CargoCrateType,
        pub target: Option<target_lexicon::Triple>,
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        pub config: Vec<String>,
        /// Wait for specified side-effects to resolve before running cargo-run.
        ///
        /// (e.g: to allow for some ambient packages / dependencies to get
        /// installed).
        pub pre_build_deps: Vec<ReadVar<SideEffect>>,
        pub output: WriteVar<CargoBuildOutput>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_cargo_common_flags::Node>();
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let rust_toolchain = ctx.reqv(crate::install_rust::Request::GetRustupToolchain);
        let flags = ctx.reqv(crate::cfg_cargo_common_flags::Request::GetFlags);

        for Request {
            in_folder,
            crate_name,
            out_name,
            profile,
            features,
            output_kind,
            target,
            extra_env,
            config,
            pre_build_deps,
            output,
        } in requests
        {
            if let Some(target) = &target {
                ctx.req(crate::install_rust::Request::InstallTargetTriple(
                    target.clone(),
                ));
            }

            ctx.emit_rust_step(format!("cargo build {crate_name}"), |ctx| {
                pre_build_deps.claim(ctx);
                let rust_toolchain = rust_toolchain.clone().claim(ctx);
                let flags = flags.clone().claim(ctx);
                let in_folder = in_folder.claim(ctx);
                let output = output.claim(ctx);
                let extra_env = extra_env.claim(ctx);
                move |rt| {
                    let rust_toolchain = rt.read(rust_toolchain);
                    let flags = rt.read(flags);
                    let in_folder = rt.read(in_folder);
                    let with_env = rt.read(extra_env).unwrap_or_default();

                    let crate::cfg_cargo_common_flags::Flags { locked, verbose } = flags;

                    let cargo_profile = match &profile {
                        CargoBuildProfile::Debug => "dev",
                        CargoBuildProfile::Release => "release",
                        CargoBuildProfile::Custom(s) => s,
                    };

                    // would be nice to use +{toolchain} syntax instead, but that
                    // doesn't work on windows via xshell for some reason...
                    let argv0 = if rust_toolchain.is_some() {
                        "rustup"
                    } else {
                        "cargo"
                    };

                    // FIXME: this flow is vestigial from a time when this node
                    // would return `CargoBuildCommand` back to the caller.
                    //
                    // this should be replaced with a easier to read + maintain
                    // `xshell` invocation
                    let cmd = CargoBuildCommand {
                        argv0: argv0.into(),
                        params: {
                            let mut v = Vec::new();
                            if let Some(rust_toolchain) = &rust_toolchain {
                                v.push("run".into());
                                v.push(rust_toolchain.into());
                                v.push("cargo".into());
                            }
                            v.push("build".into());
                            v.push("--message-format=json-render-diagnostics".into());
                            if verbose {
                                v.push("--verbose".into());
                            }
                            if locked {
                                v.push("--locked".into());
                            }
                            v.push("-p".into());
                            v.push(crate_name.clone());
                            v.extend(features.to_cargo_arg_strings().into_iter());
                            if let Some(target) = &target {
                                v.push("--target".into());
                                v.push(target.to_string());
                            }
                            v.push("--profile".into());
                            v.push(cargo_profile.into());
                            v.extend(config.iter().flat_map(|x| ["--config", x]).map(Into::into));
                            match output_kind {
                                CargoCrateType::Bin => {
                                    v.push("--bin".into());
                                    v.push(out_name.clone());
                                }
                                CargoCrateType::StaticLib | CargoCrateType::DynamicLib => {
                                    v.push("--lib".into());
                                }
                            }
                            v
                        },
                        with_env,
                        cargo_work_dir: in_folder.clone(),
                        out_name,
                        crate_type: output_kind,
                    };

                    let CargoBuildCommand {
                        argv0,
                        params,
                        mut with_env,
                        cargo_work_dir,
                        out_name,
                        crate_type,
                    } = cmd;

                    let out_dir = rt.sh.current_dir();

                    rt.sh.change_dir(cargo_work_dir);
                    let mut cmd = flowey::shell_cmd!(rt, "{argv0} {params...}");
                    if !matches!(rt.backend(), FlowBackend::Local) {
                        // if running in CI, no need to waste time with incremental
                        // build artifacts
                        with_env.insert("CARGO_INCREMENTAL".to_owned(), "0".to_owned());
                    } else {
                        // if build locally, use per-package target dirs
                        // to avoid rebuilding
                        // TODO: remove this once cargo's caching improves
                        cmd = cmd
                            .arg("--target-dir")
                            .arg(in_folder.join("target").join(&crate_name));
                    }
                    cmd = cmd.envs(&with_env);

                    log::info!(
                        "$ {}{cmd}",
                        with_env
                            .iter()
                            .map(|(k, v)| format!("{k}={v} "))
                            .collect::<Vec<_>>()
                            .concat()
                    );
                    let json = cmd.read()?;
                    let messages: Vec<cargo_output::Message> =
                        serde_json::Deserializer::from_str(&json)
                            .into_iter()
                            .collect::<Result<_, _>>()
                            .context("failed to deserialize cargo output")?;

                    rt.sh.change_dir(out_dir.clone());

                    let build_output =
                        rename_output(&messages, &crate_name, &out_name, crate_type, &out_dir)?;

                    rt.write(output, &build_output);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}

struct CargoBuildCommand {
    argv0: String,
    params: Vec<String>,
    with_env: BTreeMap<String, String>,
    cargo_work_dir: PathBuf,
    out_name: String,
    crate_type: CargoCrateType,
}

fn rename_output(
    messages: &[cargo_output::Message],
    crate_name: &str,
    out_name: &str,
    crate_type: CargoCrateType,
    out_dir: &Path,
) -> Result<CargoBuildOutput, anyhow::Error> {
    let filenames = messages
        .iter()
        .find_map(|msg| match msg {
            cargo_output::Message::CompilerArtifact {
                target: cargo_output::Target { name, kind },
                filenames,
            } if name == crate_name && kind.iter().any(|k| k == crate_type.as_str()) => {
                Some(filenames)
            }
            _ => None,
        })
        .with_context(|| {
            format!(
                "failed to find artifact {crate_name} of kind {kind}",
                kind = crate_type.as_str()
            )
        })?;

    let find_source = |name: &str| {
        filenames
            .iter()
            .find(|path| path.file_name().is_some_and(|f| f == name))
    };

    fn rename_or_copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> std::io::Result<()> {
        let res = fs_err::rename(from.as_ref(), to.as_ref());

        let needs_copy = match res {
            Ok(_) => false,
            Err(e) => match e.kind() {
                std::io::ErrorKind::CrossesDevices => true,
                _ => return Err(e),
            },
        };

        if needs_copy {
            fs_err::copy(from, to)?;
        }

        Ok(())
    }

    let do_rename = |ext: &str, no_dash: bool| -> anyhow::Result<_> {
        let mut file_name = if !no_dash {
            out_name.into()
        } else {
            out_name.replace('-', "_")
        };
        if !ext.is_empty() {
            file_name.push('.');
            file_name.push_str(ext);
        }

        let rename_path_base = out_dir.join(&file_name);
        rename_or_copy(
            find_source(&file_name)
                .with_context(|| format!("failed to find artifact file {file_name}"))?,
            &rename_path_base,
        )?;
        anyhow::Ok(rename_path_base)
    };

    let expected_output = match crate_type {
        CargoCrateType::Bin => {
            if find_source(&format!("{out_name}.exe")).is_some() {
                let exe = do_rename("exe", false)?;
                let pdb = do_rename("pdb", true)?;
                CargoBuildOutput::WindowsBin { exe, pdb }
            } else if find_source(&format!("{out_name}.efi")).is_some() {
                let efi = do_rename("efi", false)?;
                let pdb = do_rename("pdb", true)?;
                CargoBuildOutput::UefiBin { efi, pdb }
            } else if find_source(out_name).is_some() {
                let bin = do_rename("", false)?;
                CargoBuildOutput::ElfBin { bin }
            } else {
                anyhow::bail!("failed to find binary artifact for {out_name}");
            }
        }
        CargoCrateType::DynamicLib => {
            if find_source(&format!("{out_name}.dll")).is_some() {
                let dll = do_rename("dll", false)?;
                let dll_lib = do_rename("dll.lib", false)?;
                let pdb = do_rename("pdb", true)?;

                CargoBuildOutput::WindowsDynamicLib { dll, dll_lib, pdb }
            } else if let Some(source) = find_source(&format!("lib{out_name}.so")) {
                let so = {
                    let rename_path = out_dir.join(format!("lib{out_name}.so"));
                    rename_or_copy(source, &rename_path)?;
                    rename_path
                };

                CargoBuildOutput::LinuxDynamicLib { so }
            } else {
                anyhow::bail!("failed to find dynamic library artifact for {out_name}");
            }
        }
        CargoCrateType::StaticLib => {
            if find_source(&format!("{out_name}.lib")).is_some() {
                let lib = do_rename("lib", false)?;
                let pdb = do_rename("pdb", true)?;

                CargoBuildOutput::WindowsStaticLib { lib, pdb }
            } else if let Some(source) = find_source(&format!("lib{out_name}.a")) {
                let a = {
                    let rename_path = out_dir.join(format!("lib{out_name}.a"));
                    rename_or_copy(source, &rename_path)?;
                    rename_path
                };

                CargoBuildOutput::LinuxStaticLib { a }
            } else {
                anyhow::bail!("failed to find static library artifact for {out_name}");
            }
        }
    };

    Ok(expected_output)
}
