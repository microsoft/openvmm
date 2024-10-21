// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publish JUnit test results.
//!
//! On supported platforms (ADO), this will hook into the backend's native JUnit
//! handling. On Github, this will publish an artifacts with the raw XML files.
//! When running locally, this will optionally copy the XML files to the provided
//! artifact directory.

use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Register a XML file to be published
        Register {
            /// Path to a junit.xml file
            ///
            /// HACK: this is an optional since `flowey` doesn't (yet?) have any way
            /// to perform conditional-requests, and there are instances where nodes
            /// will only conditionally output JUnit XML.
            ///
            /// To keep making forward progress, I've tweaked this node to accept an
            /// optional... but this ain't great.
            junit_xml: ReadVar<Option<PathBuf>>,
            /// Brief string used when publishing the test.
            test_label: String,
            /// Side-effect confirming that the publish has succeeded
            done: WriteVar<SideEffect>,
        },
        /// (Optional) publish all registered JUnit XML files to the provided dir
        /// Only supported on local backend
        PublishToArtifact(ReadVar<PathBuf>, WriteVar<SideEffect>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut xmls = Vec::new();
        let mut artifact_dir = None;

        for req in requests {
            match req {
                Request::Register {
                    junit_xml,
                    test_label,
                    done,
                } => xmls.push((junit_xml, test_label, done)),
                Request::PublishToArtifact(a, b) => same_across_all_reqs_backing_var(
                    "PublishToArtifact",
                    &mut artifact_dir,
                    (a, b),
                )?,
            }
        }

        let xmls = xmls;
        let artifact_dir = artifact_dir;

        if artifact_dir.is_some() && !matches!(ctx.backend(), FlowBackend::Local) {
            anyhow::bail!("Copying to a custom artifact directory is only supported locally.")
        }

        match ctx.backend() {
            FlowBackend::Ado => {
                for (junit_xml, label, done) in xmls {
                    let has_path = junit_xml.map(ctx, |p| p.is_some());
                    let path = junit_xml.map(ctx, |p| {
                        p.map(|p| p.absolute().expect("TEMP").display().to_string())
                            .unwrap_or_default()
                    });
                    ctx.emit_ado_step_with_condition(
                        format!("publish JUnit test results: {label}"),
                        has_path,
                        |ctx| {
                            done.claim(ctx);
                            let path = path.claim(ctx);
                            move |rt| {
                                let path = rt.get_var(path).as_raw_var_name();
                                format!(
                                    r#"
                                    - task: PublishTestResults@2
                                      inputs:
                                        testResultsFormat: 'JUnit'
                                        testResultsFiles: '$({path})'
                                        testRunTitle: '{label}'
                                "#
                                )
                            }
                        },
                    );
                }
            }
            FlowBackend::Github => {
                let mut use_side_effects = Vec::new();
                let mut resolve_side_effects = Vec::new();
                for (junit_xml, label, done) in xmls {
                    let has_path = junit_xml.map(ctx, |p| p.is_some());
                    let path = junit_xml.map(ctx, |p| {
                        p.map(|p| p.absolute().expect("TEMP").display().to_string())
                            .unwrap_or_default()
                    });

                    resolve_side_effects.push(done);
                    use_side_effects.push(
                        ctx.emit_gh_step(
                            format!("publish JUnit test results: {label}"),
                            "actions/upload-artifact@v4",
                        )
                        .condition(has_path)
                        .with("name", label)
                        .with("path", path)
                        .finish(ctx),
                    );
                }
                ctx.emit_side_effect_step(use_side_effects, resolve_side_effects);
            }
            FlowBackend::Local => {
                let did_copy = if let Some((artifact_dir, done)) = artifact_dir {
                    let se = ctx.emit_rust_step("copy JUnit test results to artifact dir", |ctx| {
                        done.claim(ctx);
                        let artifact_dir = artifact_dir.claim(ctx);
                        let xmls = xmls
                            .iter()
                            .map(|(junit_xml, label, _done)| {
                                (junit_xml.clone().claim(ctx), label.clone())
                            })
                            .collect::<Vec<_>>();
                        |rt| {
                            let artifact_dir = rt.read(artifact_dir);

                            for (idx, (path, label)) in xmls.into_iter().enumerate() {
                                let Some(path) = rt.read(path) else {
                                    continue;
                                };
                                fs_err::copy(
                                    path,
                                    artifact_dir.join(format!(
                                        "results_{idx}_{}.xml",
                                        label.replace(' ', "_")
                                    )),
                                )?;
                            }

                            Ok(())
                        }
                    });
                    Some(se)
                } else {
                    None
                };

                let all_done = xmls.into_iter().map(|(_, _, done)| done);
                ctx.emit_side_effect_step(did_copy, all_done);
            }
        }

        Ok(())
    }
}
