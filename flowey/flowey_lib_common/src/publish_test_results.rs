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

#[derive(Serialize, Deserialize)]
pub enum Attachments {
    Logs(ReadVar<PathBuf>),
    NextestListJson(ReadVar<Option<PathBuf>>),
}

#[derive(Serialize, Deserialize)]
pub struct VmmTestResultsArtifacts {
    /// Light test results (junit.xml, nextest-list.json)
    pub test_results_light: Option<ReadVar<PathBuf>>,
    /// Full test results (all logs, dumps, etc)
    pub test_results_full: Option<ReadVar<PathBuf>>,
}

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
        /// Brief string used when publishing the test.
        /// Must be unique to the pipeline.
        pub test_label: String,
        /// Additional files or directories to upload.
        ///
        /// The boolean indicates whether the attachment is referenced in the
        /// JUnit XML file. On backends with native JUnit attachment support,
        /// these attachments will not be uploaded as distinct artifacts and
        /// will instead be uploaded via the JUnit integration.
        pub attachments: BTreeMap<String, (Attachments, bool)>,
        /// Copy the xml file and attachments to the provided directory.
        pub output_dirs: Option<VmmTestResultsArtifacts>,
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
            test_label: label,
            attachments,
            output_dirs,
            done,
        } in requests
        {
            resolve_side_effects.push(done);

            let step_name = format!("copy test results to artifact directory: {label} (JUnit XML)");
            let artifact_name = format!("{label}-junit-xml");

            let has_junit_xml = junit_xml.map(ctx, |p| p.is_some());
            let junit_xml = junit_xml.map(ctx, |p| p.unwrap_or_default());

            match ctx.backend() {
                // Since ADO supports native JUnit test result publishing,
                // we don't need to copy the XML file to an artifact directory.
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
                    // Copy the junit XML into the "light" results directory if one
                    // was provided. This covers both GitHub and local runs.
                    if let Some(output_dirs_ref) = output_dirs.as_ref() {
                        if let Some(test_results_light) = output_dirs_ref.test_results_light.clone()
                        {
                            use_side_effects.push(ctx.emit_rust_step(step_name, move |ctx| {
                                let output_dir = test_results_light.claim(ctx);
                                let has_junit_xml = has_junit_xml.claim(ctx);
                                let junit_xml = junit_xml.claim(ctx);

                                move |rt| {
                                    let output_dir = rt.read(output_dir);
                                    let has_junit_xml = rt.read(has_junit_xml);
                                    let junit_xml = rt.read(junit_xml);

                                    if has_junit_xml {
                                        fs_err::copy(
                                            junit_xml,
                                            output_dir.join(format!("{artifact_name}.xml")),
                                        )?;
                                    }

                                    Ok(())
                                }
                            }));
                        } else {
                            use_side_effects.push(has_junit_xml.into_side_effect());
                            use_side_effects.push(junit_xml.into_side_effect());
                        }
                    } else {
                        use_side_effects.push(has_junit_xml.into_side_effect());
                        use_side_effects.push(junit_xml.into_side_effect());
                    }
                }
            }

            for (attachment_label, (attachment_kind, publish_on_ado)) in attachments {
                let step_name = format!(
                    "copy attachments to artifacts directory: {label} ({attachment_label})"
                );
                let mut artifact_name = format!("{label}-{attachment_label}");
                let attachment_path_opt = match &attachment_kind {
                    Attachments::Logs(p) => p.clone().map(ctx, Some),
                    Attachments::NextestListJson(p) => {
                        artifact_name += ".json";
                        p.clone()
                    }
                };

                let attachment_exists = attachment_path_opt.map(ctx, |opt| {
                    opt.as_ref()
                        .map(|p| {
                            p.exists()
                                && (p.is_file()
                                    || p.read_dir()
                                        .expect("failed to read attachment dir")
                                        .next()
                                        .is_some())
                        })
                        .unwrap_or(false)
                });

                if matches!(ctx.backend(), FlowBackend::Ado) && !publish_on_ado {
                    use_side_effects.push(attachment_exists.into_side_effect());
                    use_side_effects.push(attachment_path_opt.into_side_effect());
                    continue;
                }

                // Decide which output directory to use based on the attachment kind.
                let maybe_output_dir =
                    output_dirs
                        .as_ref()
                        .and_then(|dirs| match &attachment_kind {
                            Attachments::Logs(_) => dirs.test_results_full.clone(),
                            Attachments::NextestListJson(_) => dirs.test_results_light.clone(),
                        });

                if let Some(output_dir) = maybe_output_dir {
                    use_side_effects.push(ctx.emit_rust_step(step_name, move |ctx| {
                        let output_dir = output_dir.claim(ctx);
                        let attachment_exists = attachment_exists.claim(ctx);
                        let attachment_path_opt = attachment_path_opt.claim(ctx);

                        move |rt| {
                            let output_dir = rt.read(output_dir);
                            let attachment_exists = rt.read(attachment_exists);
                            let attachment_path_opt = rt.read(attachment_path_opt);

                            if attachment_exists {
                                if let Some(attachment_path) = attachment_path_opt {
                                    if attachment_path.is_dir() {
                                        copy_dir_all(
                                            attachment_path,
                                            output_dir.join(artifact_name),
                                        )?;
                                    } else {
                                        fs_err::copy(
                                            attachment_path,
                                            output_dir.join(artifact_name),
                                        )?;
                                    }
                                }
                            }

                            Ok(())
                        }
                    }));
                } else {
                    use_side_effects.push(attachment_exists.into_side_effect());
                }
            }
        }
        ctx.emit_side_effect_step(use_side_effects, resolve_side_effects);

        Ok(())
    }
}
