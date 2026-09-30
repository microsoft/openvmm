// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the `incubator` binary

use crate::common::CommonProfile;
use crate::common::CommonTriple;
use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct IncubatorOutput {
    #[serde(rename = "incubator")]
    pub bin: PathBuf,
    #[serde(rename = "incubator.dbg")]
    pub dbg: Option<PathBuf>,
}

impl Artifact for IncubatorOutput {}

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub profile: CommonProfile,
        pub incubator: WriteVar<IncubatorOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::run_cargo_build::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            profile,
            incubator,
        } = request;

        let output = ctx.reqv(|v| crate::run_cargo_build::Request {
            crate_name: "incubator".into(),
            out_name: "incubator".into(),
            crate_type: flowey_lib_common::run_cargo_build::CargoCrateType::Bin,
            profile: profile.into(),
            features: Default::default(),
            target: target.as_triple(),
            no_split_dbg_info: false,
            extra_env: None,
            pre_build_deps: Vec::new(),
            output: v,
        });

        ctx.emit_minor_rust_step("report built incubator", |ctx| {
            let incubator = incubator.claim(ctx);
            let output = output.claim(ctx);
            move |rt| {
                let output = match rt.read(output) {
                    crate::run_cargo_build::CargoBuildOutput::ElfBin { bin, dbg } => {
                        IncubatorOutput { bin, dbg }
                    }
                    _ => unreachable!(),
                };

                rt.write(incubator, &output);
            }
        });

        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub enum IncubatorProfileNameOrPath {
    Name(String),
    Path(PathBuf),
}

impl IncubatorProfileNameOrPath {
    pub fn resolve(self, repo_root: &Path) -> PathBuf {
        match self {
            IncubatorProfileNameOrPath::Name(name) => incubator_profile_path(repo_root, &name),
            IncubatorProfileNameOrPath::Path(path) => path,
        }
    }
}

impl std::fmt::Display for IncubatorProfileNameOrPath {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IncubatorProfileNameOrPath::Name(name) => f.write_str(name),
            IncubatorProfileNameOrPath::Path(path) => f.write_str(path.to_string_lossy().as_ref()),
        }
    }
}

pub fn incubator_profile_dir() -> PathBuf {
    PathBuf::new()
        .join("petri")
        .join("incubator")
        .join("profiles")
}

/// Path to incubator profile given name and repo root
pub fn incubator_profile_path(repo_root: &Path, name: &str) -> PathBuf {
    repo_root
        .join(incubator_profile_dir())
        .join(format!("{name}.toml"))
}

/// Default incubator profile for a target
pub fn default_incubator_profile(target: &CommonTriple) -> Option<&'static str> {
    match *target {
        CommonTriple::AARCH64_LINUX_MUSL => Some("aarch64-tcg-pcie"),
        _ => None,
    }
}
