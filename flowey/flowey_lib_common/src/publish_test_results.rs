// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Publish test results.
//!
//! - On ADO, this will hook into the backend's native JUnit handling.
//! - On Github, this will publish artifacts containing the raw JUnit XML file
//!   and any optional attachments.
//! - When running locally, this will optionally copy the XML files and any
//!   attachments to the provided artifact directory.

use crate::_util::copy_dir_all;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Path to a junit.xml file
        ///
        /// HACK: this is an optional since `flowey` doesn't (yet?) have any way
        /// to perform conditional-requests, and there are instances where nodes
        /// will only conditionally output JUnit XML.
        ///
        /// To keep making forward progress, I've tweaked this node to accept an
        /// optional... but this ain't great.
        pub junit_xml: ReadVar<Option<PathBuf>>,
        /// Path to the JSON output from `nextest list --json`.
        pub nextest_list_json: Option<ReadVar<Option<PathBuf>>>,
        /// Brief string used when publishing the test.
        /// Must be unique to the pipeline.
        pub test_label: String,
        /// Additional files or directories to upload.
        ///
        /// The boolean indicates whether the attachment is referenced in the
        /// JUnit XML file. On backends with native JUnit attachment support,
        /// these attachments will not be uploaded as distinct artifacts and
        /// will instead be uploaded via the JUnit integration.
        pub attachments: BTreeMap<String, (ReadVar<PathBuf>, bool)>,
        /// Copy the xml file and attachments to the provided directory.
        /// Only supported on local backend.
        pub output_dir: Option<ReadVar<PathBuf>>,
        /// Side-effect confirming that the publish has succeeded
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::ado_task_publish_test_results::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut use_side_effects = Vec::new();
        let mut resolve_side_effects = Vec::new();

        for Request {
            junit_xml,
            nextest_list_json,
            test_label: label,
            attachments,
            output_dir,
            done,
        } in requests
        {
            resolve_side_effects.push(done);

            let step_name = format!("publish test results: {label} (JUnit XML)");
            let artifact_name = format!("{label}-junit-xml");

            let has_junit_xml = junit_xml.map(ctx, |p| p.is_some());
            let junit_xml = junit_xml.map(ctx, |p| p.unwrap_or_default());

            let has_nextest_list = nextest_list_json
                .as_ref()
                .map(|v| v.map(ctx, |p| p.is_some()));
            let nextest_list_json = nextest_list_json
                .as_ref()
                .map(|v| v.map(ctx, |p| p.unwrap_or_default()));

            match ctx.backend() {
                FlowBackend::Ado => {
                    use_side_effects.push(ctx.reqv(|v| {
                        crate::ado_task_publish_test_results::Request {
                            step_name,
                            format:
                                crate::ado_task_publish_test_results::AdoTestResultsFormat::JUnit,
                            results_file: junit_xml,
                            test_title: label.clone(),
                            condition: Some(has_junit_xml),
                            done: v,
                        }
                    }));
                }
                FlowBackend::Github | FlowBackend::Local => {
                    if let Some(output_dir) = output_dir.clone() {
                        use_side_effects.push(ctx.emit_rust_step(step_name, |ctx| {
                            let output_dir = output_dir.claim(ctx);
                            let has_junit_xml = has_junit_xml.claim(ctx);
                            let junit_xml = junit_xml.claim(ctx);
                            let has_nextest_list = has_nextest_list.claim(ctx);
                            let nextest_list_json = nextest_list_json.claim(ctx);
                            let label = label.clone();

                            move |rt| {
                                let output_dir = rt.read(output_dir);
                                let has_junit_xml = rt.read(has_junit_xml);
                                let junit_xml = rt.read(junit_xml);

                                let has_nextest_list = if let Some(v) = has_nextest_list {
                                    rt.read(v)
                                } else {
                                    false
                                };
                                let nextest_list_json = if let Some(v) = nextest_list_json {
                                    rt.read(v)
                                } else {
                                    PathBuf::new()
                                };

                                if has_junit_xml {
                                    fs_err::copy(
                                        junit_xml,
                                        output_dir.join(format!("{artifact_name}.xml")),
                                    )?;
                                }

                                if has_nextest_list {
                                    fs_err::copy(
                                        nextest_list_json,
                                        output_dir.join(format!("{label}-nextest-list.json")),
                                    )?;
                                }

                                Ok(())
                            }
                        }));
                    } else {
                        use_side_effects.push(has_junit_xml.into_side_effect());
                        use_side_effects.push(junit_xml.into_side_effect());
                    }
                }
            }

            for (attachment_label, (attachment_path, publish_on_ado)) in attachments {
                let step_name = format!("publish test results: {label} ({attachment_label})");
                let artifact_name = format!("{label}-{attachment_label}");

                let attachment_exists = attachment_path.map(ctx, |p| {
                    p.exists()
                        && (p.is_file()
                            || p.read_dir()
                                .expect("failed to read attachment dir")
                                .next()
                                .is_some())
                });

                match ctx.backend() {
                    FlowBackend::Ado => {
                        if publish_on_ado {
                            use_side_effects.push(attachment_exists.into_side_effect());
                        } else if let Some(output_dir) = output_dir.clone() {
                            use_side_effects.push(ctx.emit_rust_step(step_name, |ctx| {
                                let output_dir = output_dir.claim(ctx);
                                let attachment_exists = attachment_exists.claim(ctx);
                                let attachment_path = attachment_path.claim(ctx);

                                move |rt| {
                                    let output_dir = rt.read(output_dir);
                                    let attachment_exists = rt.read(attachment_exists);
                                    let attachment_path = rt.read(attachment_path);

                                    if attachment_exists {
                                        copy_dir_all(
                                            attachment_path,
                                            output_dir.join(artifact_name),
                                        )?;
                                    }

                                    Ok(())
                                }
                            }));
                        } else {
                            use_side_effects.push(attachment_exists.into_side_effect());
                        }
                    }
                    FlowBackend::Github | FlowBackend::Local => {
                        if let Some(output_dir) = output_dir.clone() {
                            use_side_effects.push(ctx.emit_rust_step(step_name, |ctx| {
                                let output_dir = output_dir.claim(ctx);
                                let attachment_exists = attachment_exists.claim(ctx);
                                let attachment_path = attachment_path.claim(ctx);

                                move |rt| {
                                    let output_dir = rt.read(output_dir);
                                    let attachment_exists = rt.read(attachment_exists);
                                    let attachment_path = rt.read(attachment_path);

                                    if attachment_exists {
                                        copy_dir_all(
                                            attachment_path,
                                            output_dir.join(artifact_name),
                                        )?;
                                    }

                                    Ok(())
                                }
                            }));
                        } else {
                            use_side_effects.push(attachment_exists.into_side_effect());
                        }
                    }
                }
            }
        }
        ctx.emit_side_effect_step(use_side_effects, resolve_side_effects);

        Ok(())
    }
}
