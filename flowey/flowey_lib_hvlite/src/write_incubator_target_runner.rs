// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Write a cargo-nextest target runner script that launches tests in an incubator.

use anyhow::Context;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;

const INCUBATOR_ENV_POLICY: &[&str] = &[
    "RUST_LOG",
    "RUST_BACKTRACE",
    "OPENVMM_LOG",
    "OPENVMM_SHOW_SPANS",
    "OPENVMM_LOG_SPANS",
    "PETRI_REMOTE_ARTIFACTS",
    "PETRI_REUSE_PREPPED_VHDS",
    "PETRI_IGNORE_UNSTABLE_FAILURES",
    "OPENVMM_REQUIRE_2MB_HUGETLB",
    "VMM_TESTS_CONTENT_DIR/p",
    "TEST_OUTPUT_PATH/p",
    "VMM_TEST_IMAGES/p",
    "NEXTEST_WORKSPACE_ROOT/p",
    "CARGO_MANIFEST_DIR/p",
    "CARGO_BIN_EXE_*/p",
    "NEXTEST_BIN_EXE_*/p",
];

const NEXTEST_ARCHIVE_TMP_DIR: &str = "nextest-archive-tmp";
const DEFAULT_INCUBATOR_RUST_LOG: &str = "info";

pub fn cargo_target_runner_env_var(target: &target_lexicon::Triple) -> String {
    format!(
        "CARGO_TARGET_{}_RUNNER",
        target.to_string().replace('-', "_").to_ascii_uppercase()
    )
}

pub fn add_incubator_target_runner_env(
    env: &mut BTreeMap<String, String>,
    target: &target_lexicon::Triple,
    target_runner: &Path,
) {
    env.insert(
        cargo_target_runner_env_var(target),
        target_runner.display().to_string(),
    );
    if let Some(target_runner_dir) = target_runner.parent() {
        env.insert(
            "TMPDIR".into(),
            target_runner_dir
                .join(NEXTEST_ARCHIVE_TMP_DIR)
                .display()
                .to_string(),
        );
    }
    env.entry("RUST_LOG".into()).or_insert_with(|| {
        std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_INCUBATOR_RUST_LOG.into())
    });
    env.insert("INCUBATOR_ENV".into(), INCUBATOR_ENV_POLICY.join(":"));
}

pub fn add_incubator_target_runner(
    ctx: &mut NodeCtx<'_>,
    target: target_lexicon::Triple,
    extra_env: ReadVar<BTreeMap<String, String>>,
    request: impl FnOnce(WriteVar<PathBuf>) -> Request,
) -> (ReadVar<BTreeMap<String, String>>, ReadVar<SideEffect>) {
    let target_runner = ctx.reqv(request);
    let target_runner_for_env = target_runner.clone();
    let extra_env =
        extra_env
            .zip(ctx, target_runner_for_env)
            .map(ctx, move |(mut env, target_runner)| {
                add_incubator_target_runner_env(&mut env, &target, &target_runner);
                env
            });

    (extra_env, target_runner.into_side_effect())
}

flowey_request! {
    pub struct Request {
        /// Path to the incubator binary.
        pub incubator_bin: ReadVar<PathBuf>,
        /// Path to the incubator profile TOML file.
        pub profile_path: ReadVar<PathBuf>,
        /// Path to the guest kernel image. If omitted, incubator auto-detects it.
        pub kernel: Option<ReadVar<PathBuf>>,
        /// Path to the base initrd. If omitted, incubator auto-detects it.
        pub initrd: Option<ReadVar<PathBuf>>,
        /// Path to the OpenVMM repo root.
        pub workspace_dir: ReadVar<PathBuf>,
        /// Directory containing VMM test runtime artifacts and test outputs.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Additional host paths that must be visible in the incubator share.
        pub extra_share_paths: Vec<ReadVar<PathBuf>>,
        /// Additional environment variables used to discover path roots that
        /// must be visible in the incubator share.
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Optional pipette binary to copy into the shared test content directory.
        pub pipette_bin: Option<ReadVar<PathBuf>>,
        /// Copy the incubator binary into the shared test content directory before
        /// generating the runner script, so the script remains valid after
        /// temporary build outputs are cleaned up.
        pub copy_incubator_bin: bool,
        /// Path to the QEMU binary (overrides the profile's binary setting).
        pub qemu_binary: Option<ReadVar<PathBuf>>,
        /// Path to the generated target runner script.
        pub target_runner: WriteVar<PathBuf>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            incubator_bin,
            profile_path,
            kernel,
            initrd,
            workspace_dir,
            test_content_dir,
            extra_share_paths,
            extra_env,
            pipette_bin,
            copy_incubator_bin,
            qemu_binary,
            target_runner,
        } = request;

        ctx.emit_rust_step("write incubator target runner", |ctx| {
            let incubator_bin = incubator_bin.claim(ctx);
            let profile_path = profile_path.claim(ctx);
            let kernel = kernel.claim(ctx);
            let initrd = initrd.claim(ctx);
            let workspace_dir = workspace_dir.claim(ctx);
            let test_content_dir = test_content_dir.claim(ctx);
            let extra_share_paths = extra_share_paths.claim(ctx);
            let extra_env = extra_env.claim(ctx);
            let pipette_bin = pipette_bin.claim(ctx);
            let qemu_binary = qemu_binary.claim(ctx);
            let target_runner = target_runner.claim(ctx);

            move |rt| {
                let mut incubator_bin = rt.read(incubator_bin).absolute()?;
                let profile_path = rt.read(profile_path).absolute()?;
                let kernel = kernel.map(|v| rt.read(v).absolute()).transpose()?;
                let initrd = initrd.map(|v| rt.read(v).absolute()).transpose()?;
                let workspace_dir = rt.read(workspace_dir).absolute()?;
                let test_content_dir = rt.read(test_content_dir).absolute()?;
                let extra_share_paths = rt
                    .read(extra_share_paths)
                    .into_iter()
                    .map(|p| p.absolute().map_err(Into::into))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let extra_env = extra_env.map(|v| rt.read(v)).unwrap_or_default();
                let pipette_bin = pipette_bin.map(|v| rt.read(v).absolute()).transpose()?;
                let qemu_binary = qemu_binary.map(|v| rt.read(v).absolute()).transpose()?;

                let mut share_paths = vec![workspace_dir.as_path(), test_content_dir.as_path()];
                share_paths.extend(extra_share_paths.iter().map(|p| p.as_path()));
                let images_dir = extra_env.get("VMM_TEST_IMAGES").map(PathBuf::from);
                if let Some(ref images_dir) = images_dir {
                    share_paths.push(images_dir.as_path());
                }
                let share_root = common_ancestor(&share_paths)?;

                let guest_test_content_dir = guest_path(&share_root, &test_content_dir)?;
                let output_dir = test_content_dir.join("test_results");
                fs_err::create_dir_all(&output_dir)?;
                fs_err::create_dir_all(test_content_dir.join(NEXTEST_ARCHIVE_TMP_DIR))?;
                if copy_incubator_bin {
                    let dst = test_content_dir.join("incubator");
                    fs_err::copy(&incubator_bin, &dst)?;
                    dst.make_executable()?;
                    incubator_bin = dst;
                }
                if let Some(pipette_bin) = pipette_bin {
                    let dst = test_content_dir.join("pipette");
                    fs_err::copy(&pipette_bin, &dst)?;
                    dst.make_executable()?;
                }

                let runner_path = test_content_dir.join("incubator-target-runner.sh");
                let script = target_runner_script(TargetRunnerScript {
                    incubator_bin: &incubator_bin,
                    profile_path: &profile_path,
                    kernel: kernel.as_deref(),
                    initrd: initrd.as_deref(),
                    share_root: &share_root,
                    output_dir: &output_dir,
                    guest_pipette: &format!("{guest_test_content_dir}/pipette"),
                    guest_current_dir: &guest_test_content_dir,
                    qemu_binary: qemu_binary.as_deref(),
                });

                fs_err::write(&runner_path, script)?;
                runner_path.make_executable()?;
                incubator_bin.make_executable()?;
                if let Some(qemu_binary) = &qemu_binary {
                    qemu_binary.make_executable()?;
                }

                rt.write(target_runner, &runner_path);

                Ok(())
            }
        });

        Ok(())
    }
}

pub struct TargetRunnerScript<'a> {
    pub incubator_bin: &'a Path,
    pub profile_path: &'a Path,
    pub kernel: Option<&'a Path>,
    pub initrd: Option<&'a Path>,
    pub share_root: &'a Path,
    pub output_dir: &'a Path,
    pub guest_pipette: &'a str,
    pub guest_current_dir: &'a str,
    pub qemu_binary: Option<&'a Path>,
}

pub fn target_runner_script(config: TargetRunnerScript<'_>) -> String {
    let TargetRunnerScript {
        incubator_bin,
        profile_path,
        kernel,
        initrd,
        share_root,
        output_dir,
        guest_pipette,
        guest_current_dir,
        qemu_binary,
    } = config;

    let continuation = "\\";
    let mut args = vec![format!(
        "    --profile {} {continuation}",
        sh_quote(profile_path)
    )];

    if let Some(kernel) = kernel {
        args.push(format!("    --kernel {} {continuation}", sh_quote(kernel)));
    }

    if let Some(initrd) = initrd {
        args.push(format!("    --initrd {} {continuation}", sh_quote(initrd)));
    }

    args.extend([
        format!("    --share {} {continuation}", sh_quote(share_root)),
        format!("    --output-dir {} {continuation}", sh_quote(output_dir)),
        format!(
            "    --guest-pipette {} {continuation}",
            sh_quote(guest_pipette)
        ),
        format!(
            "    --guest-current-dir {} {continuation}",
            sh_quote(guest_current_dir)
        ),
    ]);

    if let Some(qemu_binary) = qemu_binary {
        args.push(format!(
            "    --qemu-binary {} {continuation}",
            sh_quote(qemu_binary)
        ));
    }

    args.push(format!("    --map-command-path {continuation}"));
    args.push("    -- \"$@\"".to_string());

    let command = args.join("\n");
    format!(
        "#!/bin/sh\nif [ -t 0 ]; then\nexec {} < /dev/null {continuation}\n{}\nelse\ncat | exec {} {continuation}\n{}\nfi\n",
        sh_quote(incubator_bin),
        command,
        sh_quote(incubator_bin),
        command,
    )
}

fn sh_quote(value: impl AsRef<OsStr>) -> String {
    format!(
        "'{}'",
        value.as_ref().to_string_lossy().replace('\'', "'\"'\"'")
    )
}

fn guest_path(share_root: &Path, path: &Path) -> anyhow::Result<String> {
    let relative = path.strip_prefix(share_root).with_context(|| {
        format!(
            "{} is not under share root {}",
            path.display(),
            share_root.display()
        )
    })?;

    if relative.as_os_str().is_empty() {
        Ok("/share".to_string())
    } else {
        Ok(format!("/share/{}", relative.display()))
    }
}

fn common_ancestor(paths: &[&Path]) -> anyhow::Result<PathBuf> {
    let mut candidate = paths
        .first()
        .context("no paths for share root")?
        .to_path_buf();

    loop {
        if paths.iter().all(|path| path.starts_with(&candidate)) {
            return Ok(candidate);
        }

        if !candidate.pop() {
            anyhow::bail!("paths do not share a common root")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(sh_quote("with space"), "'with space'");
        assert_eq!(sh_quote("it's ok"), "'it'\"'\"'s ok'");
    }

    #[test]
    fn maps_guest_share_paths() {
        assert_eq!(
            guest_path(Path::new("/tmp/share"), Path::new("/tmp/share/bin/test")).unwrap(),
            "/share/bin/test"
        );
        assert_eq!(
            guest_path(Path::new("/tmp/share"), Path::new("/tmp/share")).unwrap(),
            "/share"
        );
    }

    #[test]
    fn writes_incubator_target_runner_script() {
        let script = target_runner_script(TargetRunnerScript {
            incubator_bin: Path::new("/tmp/tools/incubator"),
            profile_path: Path::new("/tmp/profiles/aarch64-tcg.toml"),
            kernel: Some(Path::new("/tmp/kernel Image")),
            initrd: Some(Path::new("/tmp/initrd.gz")),
            share_root: Path::new("/tmp/test content"),
            output_dir: Path::new("/tmp/test content/test_results"),
            guest_pipette: "/share/pipette",
            guest_current_dir: "/share",
            qemu_binary: Some(Path::new("/tmp/qemu/system-aarch64")),
        });

        assert_eq!(script, expected_target_runner_script());
    }

    fn expected_target_runner_script() -> &'static str {
        concat!(
            "#!/bin/sh\n",
            "if [ -t 0 ]; then\n",
            "exec '/tmp/tools/incubator' < /dev/null \\\n",
            "    --profile '/tmp/profiles/aarch64-tcg.toml' \\\n",
            "    --kernel '/tmp/kernel Image' \\\n",
            "    --initrd '/tmp/initrd.gz' \\\n",
            "    --share '/tmp/test content' \\\n",
            "    --output-dir '/tmp/test content/test_results' \\\n",
            "    --guest-pipette '/share/pipette' \\\n",
            "    --guest-current-dir '/share' \\\n",
            "    --qemu-binary '/tmp/qemu/system-aarch64' \\\n",
            "    --map-command-path \\\n",
            "    -- \"$@\"\n",
            "else\n",
            "cat | exec '/tmp/tools/incubator' \\\n",
            "    --profile '/tmp/profiles/aarch64-tcg.toml' \\\n",
            "    --kernel '/tmp/kernel Image' \\\n",
            "    --initrd '/tmp/initrd.gz' \\\n",
            "    --share '/tmp/test content' \\\n",
            "    --output-dir '/tmp/test content/test_results' \\\n",
            "    --guest-pipette '/share/pipette' \\\n",
            "    --guest-current-dir '/share' \\\n",
            "    --qemu-binary '/tmp/qemu/system-aarch64' \\\n",
            "    --map-command-path \\\n",
            "    -- \"$@\"\n",
            "fi\n",
        )
    }

    #[test]
    fn builds_cargo_target_runner_env_var() {
        assert_eq!(
            cargo_target_runner_env_var(&target_lexicon::triple!("aarch64-unknown-linux-musl")),
            "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER"
        );
    }

    #[test]
    fn adds_incubator_target_runner_env() {
        let mut env = BTreeMap::new();
        let runner = Path::new("tmp").join("runner.sh");
        add_incubator_target_runner_env(
            &mut env,
            &target_lexicon::triple!("aarch64-unknown-linux-musl"),
            &runner,
        );

        assert_eq!(
            env.get("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER")
                .unwrap(),
            &runner.display().to_string()
        );
        assert_eq!(
            env.get("TMPDIR").unwrap(),
            &Path::new("tmp")
                .join(NEXTEST_ARCHIVE_TMP_DIR)
                .display()
                .to_string()
        );
        assert_eq!(
            env.get("RUST_LOG").unwrap(),
            &std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_INCUBATOR_RUST_LOG.into())
        );
        assert_eq!(
            env.get("INCUBATOR_ENV").unwrap(),
            &INCUBATOR_ENV_POLICY.join(":")
        );
        assert!(
            !env.get("INCUBATOR_ENV")
                .unwrap()
                .contains("LD_LIBRARY_PATH")
        );
    }

    #[test]
    fn keeps_explicit_incubator_rust_log() {
        let mut env = BTreeMap::from([("RUST_LOG".into(), "warn,mesh=off".into())]);
        add_incubator_target_runner_env(
            &mut env,
            &target_lexicon::triple!("aarch64-unknown-linux-musl"),
            Path::new("/tmp/runner.sh"),
        );

        assert_eq!(env.get("RUST_LOG").unwrap(), "warn,mesh=off");
    }
}
