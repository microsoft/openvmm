// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Encapsulates the logic of both invoking `cargo build`, and tracking where
//! built artifacts are emitted (which varies depending on the crate's type and
//! platform).
//!
//! Takes into account bits of "global" configuration and dependency management,
//! such as setting global cargo flags (e.g: --verbose, --locked), ensuring any
//! required Rust dependencies are installed (i.e: toolchain, triples), etc...

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

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

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub enum CargoCrateType {
    Bin,
    StaticLib,
    DynamicLib,
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
        pub features: BTreeSet<String>,
        pub output_kind: CargoCrateType,
        pub target: target_lexicon::Triple,
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
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
            pre_build_deps,
            output,
        } in requests
        {
            ctx.req(crate::install_rust::Request::InstallTargetTriple(
                target.clone(),
            ));

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
                    let with_env = extra_env.map(|x| rt.read(x)).unwrap_or_default();

                    let crate::cfg_cargo_common_flags::Flags { locked, verbose } = flags;

                    let features = features.into_iter().collect::<Vec<_>>().join(",");

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

                    let cargo_out_dir = {
                        // DEVNOTE: this is a _pragmatic_ implementation of this
                        // logic, and is written with the undersatnding that
                        // there are undoubtedly many "edge-cases" that may
                        // result in the final target directory changing.
                        //
                        // One possible way to make this handling more robust
                        // would be to start using `--message-format=json` when
                        // invoking `cargo`, and then parsing the machine
                        // readable output in order to obtain the output
                        // artifact path _after_ the compilation has succeeded.
                        if let Ok(dir) = std::env::var("CARGO_TARGET_DIR") {
                            PathBuf::from(dir)
                        } else {
                            in_folder.join("target")
                        }
                    }
                    .join(target.to_string())
                    .join(match profile {
                        CargoBuildProfile::Debug => "debug",
                        _ => cargo_profile,
                    });

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
                            if verbose {
                                v.push("--verbose".into());
                            }
                            if locked {
                                v.push("--locked".into());
                            }
                            v.push("-p".into());
                            v.push(crate_name.clone());
                            if !features.is_empty() {
                                v.push("--features".into());
                                v.push(features);
                            }
                            v.push("--target".into());
                            v.push(target.to_string());
                            v.push("--profile".into());
                            v.push(cargo_profile.into());
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
                        cargo_out_dir,
                        out_name,
                        crate_type: output_kind,
                        target,
                    };

                    let CargoBuildCommand {
                        argv0,
                        params,
                        with_env,
                        cargo_work_dir,
                        cargo_out_dir,
                        out_name,
                        crate_type,
                        target,
                    } = cmd;

                    let sh = xshell::Shell::new()?;

                    let out_dir = sh.current_dir();

                    let do_rename_output = |dry_run| {
                        rename_output(
                            &out_name,
                            &target,
                            crate_type,
                            &out_dir,
                            &cargo_out_dir,
                            dry_run,
                        )
                    };

                    let check_paths = match do_rename_output(true)? {
                        CargoBuildOutput::WindowsBin { exe, pdb } => vec![exe, pdb],
                        CargoBuildOutput::ElfBin { bin } => vec![bin],
                        CargoBuildOutput::LinuxStaticLib { a } => vec![a],
                        CargoBuildOutput::LinuxDynamicLib { so } => vec![so],
                        CargoBuildOutput::WindowsStaticLib { lib, pdb } => vec![lib, pdb],
                        CargoBuildOutput::WindowsDynamicLib { dll, dll_lib, pdb } => {
                            vec![dll, dll_lib, pdb]
                        }
                        CargoBuildOutput::UefiBin { efi, pdb } => vec![efi, pdb],
                    };

                    for path in check_paths {
                        if path.exists() {
                            anyhow::bail!("BUG: The `cargo_build` helper requires the Node ensure the build directory is fresh!")
                        }
                    }

                    sh.change_dir(cargo_work_dir);
                    let mut cmd = xshell::cmd!(sh, "{argv0} {params...}");
                    // if running in CI, no need to waste time with incremental
                    // build artifacts
                    if !matches!(rt.backend(), FlowBackend::Local) {
                        cmd = cmd.env("CARGO_INCREMENTAL", "0");
                    }
                    for (key, val) in with_env {
                        log::info!("extra_env: {key}={val}");
                        cmd = cmd.env(key, val);
                    }
                    cmd.run()?;

                    sh.change_dir(out_dir.clone());

                    let build_output = do_rename_output(false)?;

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
    cargo_out_dir: PathBuf,
    out_name: String,
    crate_type: CargoCrateType,
    target: target_lexicon::Triple,
}

fn rename_output(
    out_name: &str,
    target: &target_lexicon::Triple,
    crate_type: CargoCrateType,
    out_dir: &Path,
    cargo_out_dir: &Path,
    dry_run: bool,
) -> Result<CargoBuildOutput, anyhow::Error> {
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
        let file_name = if !no_dash {
            out_name.into()
        } else {
            out_name.replace('-', "_")
        };

        let out_path_base = cargo_out_dir.join(&file_name);
        let rename_path_base = out_dir.join(&file_name);

        if !dry_run {
            rename_or_copy(
                out_path_base.with_extension(ext),
                rename_path_base.with_extension(ext),
            )?;
        }

        anyhow::Ok(rename_path_base.with_extension(ext))
    };

    let expected_output = match (crate_type, target.operating_system) {
        (CargoCrateType::Bin, target_lexicon::OperatingSystem::Windows) => {
            let exe = do_rename("exe", false)?;
            let pdb = do_rename("pdb", true)?;
            CargoBuildOutput::WindowsBin { exe, pdb }
        }
        (
            CargoCrateType::Bin,
            target_lexicon::OperatingSystem::Linux | target_lexicon::OperatingSystem::None_,
        ) => {
            let bin = do_rename("", false)?;
            CargoBuildOutput::ElfBin { bin }
        }
        (CargoCrateType::DynamicLib, target_lexicon::OperatingSystem::Windows) => {
            let dll = do_rename("dll", false)?;
            let dll_lib = do_rename("dll.lib", false)?;
            let pdb = do_rename("pdb", true)?;

            CargoBuildOutput::WindowsDynamicLib { dll, dll_lib, pdb }
        }
        (CargoCrateType::DynamicLib, target_lexicon::OperatingSystem::Linux) => {
            let so = {
                let rename_path = out_dir.join(format!("lib{out_name}.so"));
                if !dry_run {
                    rename_or_copy(
                        cargo_out_dir.join(format!("lib{out_name}.so")),
                        &rename_path,
                    )?;
                }
                rename_path
            };

            CargoBuildOutput::LinuxDynamicLib { so }
        }
        (CargoCrateType::StaticLib, target_lexicon::OperatingSystem::Windows) => {
            let lib = do_rename("lib", false)?;
            let pdb = do_rename("pdb", true)?;

            CargoBuildOutput::WindowsStaticLib { lib, pdb }
        }
        (CargoCrateType::StaticLib, target_lexicon::OperatingSystem::Linux) => {
            let a = {
                let rename_path = out_dir.join(format!("lib{out_name}.a"));
                if !dry_run {
                    rename_or_copy(cargo_out_dir.join(format!("lib{out_name}.a")), &rename_path)?;
                }
                rename_path
            };

            CargoBuildOutput::LinuxStaticLib { a }
        }
        (CargoCrateType::Bin, target_lexicon::OperatingSystem::Uefi) => {
            let efi = do_rename("efi", false)?;
            let pdb = do_rename("pdb", true)?;

            CargoBuildOutput::UefiBin { efi, pdb }
        }
        _ => {
            anyhow::bail!(
                "missing support for building {:?} on {}",
                crate_type,
                target
            )
        }
    };

    Ok(expected_output)
}
