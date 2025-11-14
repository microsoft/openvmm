// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Glue to generate HTML LCOV-based coverage reports from `cargo-fuzz`
//! `coverage.profdata` files.

use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn generate_html_coverage_report(
    ctx: &crate::XtaskCtx,
    target_fuzz_dir: &Path,
    target_name: &str,
) -> Result<(), anyhow::Error> {
    // cargo-fuzz will always dump the data here
    let coverage_profdata_file = target_fuzz_dir
        .join("coverage")
        .join(target_name)
        .join("coverage.profdata");

    // would be great if there was an easy way to find where the resulting
    // `cargo fuzz coverage` bin gets dumped to... but this seems to work ok.
    let coverage_bin = {
        let mut coverage_bin: Option<PathBuf> = None;
        for e in walkdir::WalkDir::new(ctx.root.join("target")) {
            let e = e?;
            if e.file_name() != target_name
                || !e.path().components().any(|c| c.as_os_str() == "coverage")
            {
                continue;
            }

            // instead of immediately breaking, as a sanity check, keep looking
            // for other matches, erroring out if there is a dupe (which would
            // indicate that this logic needs some more tweaking)
            if let Some(existing_bin) = &coverage_bin {
                panic!(
                    "xtask bug: found multiple potential coverage bins: {} and {}",
                    existing_bin.display(),
                    e.path().display()
                )
            } else {
                coverage_bin = Some(e.into_path());
            }
        }
        coverage_bin.expect("xtask bug: failed to find the coverage-instrumented fuzzer bin")
    };

    let llvm_tools_dir = 'llvm_tools_dir: {
        let output = std::process::Command::new("rustc")
            .args(["+nightly", "--print", "sysroot"])
            .output()
            .context("failed to run `rustc +nightly --print sysroot`")?;
        let rustc_sysroot = String::from_utf8_lossy(&output.stdout).to_string();
        let rustc_sysroot = rustc_sysroot.trim();

        for e in walkdir::WalkDir::new(rustc_sysroot) {
            let e = e?;
            if e.file_name() == "llvm-profdata" {
                let mut path = e.into_path();
                path.pop();
                break 'llvm_tools_dir path;
            }
        }

        anyhow::bail!(
            "failed to find `llvm-tools` directory. did you run `rustup +nightly component add llvm-tools`?"
        )
    };

    if which::which("lcov").is_err() {
        anyhow::bail!(
            "could not find `lcov` on your $PATH! make sure it's installed (e.g: `apt install lcov`)"
        )
    }

    let coverage_dir = coverage_bin.parent().unwrap();
    let coverage_lcov_file = coverage_dir.join("coverage.lcov");
    {
        let lcov_output = std::fs::File::create(&coverage_lcov_file)?;

        let mut cmd = std::process::Command::new(llvm_tools_dir.join("llvm-cov"));
        let mut cmd = cmd
            .arg("export")
            .arg("-instr-profile")
            .arg(coverage_profdata_file)
            .arg("-format=lcov")
            .arg("-object")
            .arg(&coverage_bin)
            .args(["--ignore-filename-regex", "rustc"])
            .args(["--ignore-filename-regex", "openssl-sys"])
            .stdout(std::process::Stdio::from(lcov_output))
            .spawn()?;
        if !cmd.wait()?.success() {
            anyhow::bail!("failed while running `llvm-cov`")
        }
    }

    let html_report_dir = coverage_dir.join(format!("lcov_html_{}", target_name));
    {
        // summarize the coverage information
        let mut cmd = std::process::Command::new("lcov");
        let mut cmd = cmd.arg("--summary").arg(&coverage_lcov_file).spawn()?;
        if !cmd.wait()?.success() {
            anyhow::bail!("failed while running `lcov`")
        };

        // make an output directory for the html report
        if html_report_dir.exists() {
            fs_err::remove_dir_all(&html_report_dir)?;
        }
        fs_err::create_dir(&html_report_dir)?;

        // generate the html report
        let mut cmd = std::process::Command::new("genhtml");
        let mut cmd = cmd
            .arg("-o")
            .arg(&html_report_dir)
            .arg("--legend")
            .arg("--highlight")
            .arg(coverage_lcov_file)
            .spawn()?;
        if !cmd.wait()?.success() {
            anyhow::bail!("failed while running `genhtml`")
        }
    }

    log::info!("");
    log::info!(
        "success! html report generated at {}",
        html_report_dir.join("index.html").display()
    );

    Ok(())
}
