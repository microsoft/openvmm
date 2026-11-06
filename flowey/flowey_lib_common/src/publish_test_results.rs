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
use crate::run_cargo_nextest_run::TestResults;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Contains whether all passed and the path to a junit.xml file
        ///
        /// HACK: this is an optional since `flowey` doesn't (yet?) have any way
        /// to perform conditional-requests, and there are instances where nodes
        /// will only conditionally output JUnit XML.
        ///
        /// To keep making forward progress, I've tweaked this node to accept an
        /// optional... but this ain't great.
        pub test_results: ReadVar<TestResults>,
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
        /// Upload logs on success (logs are always uploaded on failure)
        pub upload_logs_on_success: bool,
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
            test_results,
            test_label: label,
            attachments,
            output_dir,
            upload_logs_on_success,
            done,
        } in requests
        {
            resolve_side_effects.push(done);

            if output_dir.is_some() && !matches!(ctx.backend(), FlowBackend::Local) {
                anyhow::bail!(
                    "Copying to a custom output directory is only supported on local backend."
                )
            }

            let step_name = format!("publish test results: {label} (JUnit XML)");
            let artifact_name = format!("{label}-junit-xml");

            let native_results = matches!(ctx.backend(), FlowBackend::Ado);
            let should_publish_junit_xml = test_results.map(ctx, move |r| {
                should_publish_junit_xml(&r, upload_logs_on_success, native_results)
            });

            match ctx.backend() {
                FlowBackend::Ado => {
                    let results_file = test_results.map(ctx, |p| p.junit_xml.unwrap_or_default());
                    use_side_effects.push(ctx.reqv(|v| {
                        crate::ado_task_publish_test_results::Request {
                            step_name,
                            format:
                                crate::ado_task_publish_test_results::AdoTestResultsFormat::JUnit,
                            results_file,
                            test_title: label.clone(),
                            condition: Some(should_publish_junit_xml),
                            done: v,
                        }
                    }));
                }
                FlowBackend::Github => {
                    let junit_xml = test_results.map(ctx, |p| {
                        p.junit_xml
                            .map(|p| p.absolute().expect("invalid path").display().to_string())
                            .unwrap_or_default()
                    });

                    // Note: usually flowey's built-in artifact publishing API
                    // should be used instead of this, but here we need to
                    // manually upload the artifact now so that it is still
                    // uploaded even if the pipeline fails.
                    // actions/upload-artifact v7.0.1
                    use_side_effects.push(
                        ctx.emit_gh_step(
                            step_name,
                            "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
                        )
                        .condition(should_publish_junit_xml)
                        .with("name", artifact_name)
                        .with("path", junit_xml)
                        .finish(ctx),
                    );
                }
                FlowBackend::Local => {
                    if let Some(output_dir) = output_dir.clone() {
                        use_side_effects.push(ctx.emit_rust_step(step_name, |ctx| {
                            let output_dir = output_dir.claim(ctx);
                            let test_results = test_results.clone().claim(ctx);
                            let should_publish_junit_xml = should_publish_junit_xml.claim(ctx);

                            move |rt| {
                                let output_dir = rt.read(output_dir);
                                let test_results = rt.read(test_results);
                                let should_publish_junit_xml = rt.read(should_publish_junit_xml);

                                if let Some(junit_xml) = test_results.junit_xml
                                    && should_publish_junit_xml
                                {
                                    fs_err::copy(
                                        junit_xml,
                                        output_dir.join(format!("{artifact_name}.xml")),
                                    )?;
                                }

                                Ok(())
                            }
                        }));
                    } else {
                        use_side_effects.push(should_publish_junit_xml.into_side_effect());
                    }
                }
            }

            let resources = test_results.map(ctx, |r| r.resources.unwrap_or_default());
            let attachments = attachments
                .into_iter()
                .map(|(label, (path, ado))| (label, path, ado, false))
                .chain(std::iter::once(("resources".into(), resources, true, true)));
            for (attachment_label, attachment_path, publish_on_ado, always_upload) in attachments {
                let step_name = format!("publish test results: {label} ({attachment_label})");
                let artifact_name = format!("{label}-{attachment_label}");
                let should_publish =
                    attachment_path
                        .zip(ctx, test_results.clone())
                        .map(ctx, move |(p, r)| {
                            !p.as_os_str().is_empty()
                                && (always_upload || upload_logs_on_success || !r.all_tests_passed)
                                && p.exists()
                                && (p.is_file()
                                    || p.read_dir()
                                        .expect("failed to read attachment dir")
                                        .next()
                                        .is_some())
                        });
                let attachment_path_string = attachment_path.map(ctx, attachment_path_to_string);

                match ctx.backend() {
                    FlowBackend::Ado => {
                        if publish_on_ado {
                            let (published_read, published_write) = ctx.new_var();
                            use_side_effects.push(published_read);

                            // Note: usually flowey's built-in artifact publishing API
                            // should be used instead of this, but here we need to
                            // manually upload the artifact now so that it is still
                            // uploaded even if the pipeline fails.
                            ctx.emit_ado_step_with_condition(
                                step_name.clone(),
                                should_publish,
                                |ctx| {
                                    published_write.claim(ctx);
                                    let attachment_path_string = attachment_path_string.claim(ctx);
                                    move |rt| {
                                        let path_var =
                                            rt.get_var(attachment_path_string).as_raw_var_name();
                                        // Artifact name includes the JobAttempt to
                                        // differentiate between artifacts that were
                                        // generated when rerunning failed jobs.
                                        format!(
                                            r#"
                                            - publish: $({path_var})
                                              artifact: {artifact_name}-$({})
                                            "#,
                                            AdoRuntimeVar::SYSTEM_JOB_ATTEMPT.as_raw_var_name()
                                        )
                                    }
                                },
                            );
                        } else {
                            use_side_effects.push(should_publish.into_side_effect());
                            use_side_effects.push(attachment_path_string.into_side_effect());
                        }
                    }
                    FlowBackend::Github => {
                        // See above comment about manually publishing artifacts
                        // actions/upload-artifact v7.0.1
                        use_side_effects.push(
                            ctx.emit_gh_step(
                                step_name.clone(),
                                "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a",
                            )
                            .condition(should_publish)
                            .with("name", artifact_name)
                            .with("path", attachment_path_string)
                            .finish(ctx),
                        );
                    }
                    FlowBackend::Local => {
                        if let Some(output_dir) = output_dir.clone() {
                            use_side_effects.push(ctx.emit_rust_step(step_name, |ctx| {
                                let output_dir = output_dir.claim(ctx);
                                let should_publish = should_publish.claim(ctx);
                                let attachment_path = attachment_path.claim(ctx);

                                move |rt| {
                                    let output_dir = rt.read(output_dir);
                                    let should_publish = rt.read(should_publish);
                                    let attachment_path = rt.read(attachment_path);

                                    if should_publish {
                                        copy_dir_all(
                                            attachment_path,
                                            output_dir.join(artifact_name),
                                        )?;
                                    }

                                    Ok(())
                                }
                            }));
                        } else {
                            use_side_effects.push(should_publish.into_side_effect());
                        }
                        use_side_effects.push(attachment_path_string.into_side_effect());
                    }
                }
            }
        }
        ctx.emit_side_effect_step(use_side_effects, resolve_side_effects);

        Ok(())
    }
}

fn should_publish_junit_xml(
    results: &TestResults,
    upload_logs_on_success: bool,
    native_results: bool,
) -> bool {
    let bundled = results
        .resources
        .as_ref()
        .is_some_and(|dir| dir.join("junit.xml").is_file());
    results.junit_xml.is_some()
        && (upload_logs_on_success || !results.all_tests_passed)
        && (native_results || !bundled)
}

fn attachment_path_to_string(path: PathBuf) -> String {
    if path.as_os_str().is_empty() {
        String::new()
    } else {
        path.absolute().expect("invalid path").display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn bundled_junit_is_not_uploaded_separately() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs_err::write(dir.path().join("junit.xml"), "<testsuites/>")?;
        for all_tests_passed in [false, true] {
            let results = TestResults {
                all_tests_passed,
                junit_xml: Some(dir.path().join("junit.xml")),
                resources: Some(dir.path().to_owned()),
            };
            for upload_logs_on_success in [false, true] {
                assert!(!should_publish_junit_xml(
                    &results,
                    upload_logs_on_success,
                    false
                ));
                assert_eq!(
                    should_publish_junit_xml(&results, upload_logs_on_success, true),
                    upload_logs_on_success || !all_tests_passed,
                );
            }
        }
        Ok(())
    }

    #[test]
    fn unbundled_junit_preserves_retention_and_copy_failure_fallback() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for resources in [None, Some(dir.path().to_owned())] {
            for all_tests_passed in [false, true] {
                let mut results = TestResults {
                    all_tests_passed,
                    junit_xml: Some(dir.path().join("original-junit.xml")),
                    resources: resources.clone(),
                };
                for native_results in [false, true] {
                    for upload_logs_on_success in [false, true] {
                        assert_eq!(
                            should_publish_junit_xml(
                                &results,
                                upload_logs_on_success,
                                native_results
                            ),
                            upload_logs_on_success || !all_tests_passed,
                        );
                    }
                }
                results.junit_xml = None;
                assert!(!should_publish_junit_xml(&results, true, false));
            }
        }
        Ok(())
    }

    #[test]
    fn absent_attachment_path_stays_empty() {
        assert_eq!(attachment_path_to_string(PathBuf::new()), "");
    }

    #[test]
    fn present_attachment_path_is_absolute() -> anyhow::Result<()> {
        let expected = std::env::current_dir()?.join("resources");
        assert_eq!(
            attachment_path_to_string(PathBuf::from("resources")),
            expected.display().to_string()
        );
        Ok(())
    }
}
