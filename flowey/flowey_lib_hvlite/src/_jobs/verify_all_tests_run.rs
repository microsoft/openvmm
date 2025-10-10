// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Verifies that all tests that are built are run at least once over the course of an entire pipeline run.
use flowey::node::prelude::*;
use quick_xml::Reader;
use quick_xml::events::Event;
use serde::Deserialize;
use std::collections::HashMap;
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
struct Root {
    #[serde(rename = "rust-suites")]
    rust_suites: HashMap<String, Suite>,
}

#[derive(Debug, Deserialize)]
struct Suite {
    testcases: HashMap<String, serde_json::Value>, // we don't care about contents
}

flowey_request! {
    pub struct Request {
        pub test_artifacts: Vec<(String, ReadVar<PathBuf>)>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_artifacts,
            done,
        } = request;

        // It doesn't make sense for this node to run locally since there is no way for one machine
        // to run all the vmm_tests we have.
        if ctx.backend() == FlowBackend::Local {
            return Ok(());
        }

        let parse = ctx.emit_rust_step(
            "parse and analyze junit logs and nextest list output",
            |ctx| {
                // This step takes all of the junit XML files (i.e. the tests that were run) and the nextest list output (i.e. the tests that were built)
                // and verifies that the set of all tests that were built is the same as the set of all tests that were run.
                // If these sets were to differ it would be because a test was built but not run, which indicates a test gap.
                // We have automation in the test run step that will automatically skip tests that are not meant to run on a given host because the host does
                // not meet the test case requirements. For example, TDX/SNP tests are skipped on non-compatible hardware.
                let artifacts: Vec<_> = test_artifacts
                    .into_iter()
                    .map(|(prefix, path)| (prefix, path.claim(ctx)))
                    .collect();

                move |rt| {
                    let mut combined_junit_testcases: HashSet<String> = HashSet::new();
                    let mut combined_nextest_testcases: HashSet<String> = HashSet::new();

                    for (prefix, path) in artifacts {
                        let artifact_dir = rt.read(path);
                        println!("Artifact dir: {}", artifact_dir.display());
                        assert!(artifact_dir.exists(), "expected artifact dir to exist");

                        let junit_xml = prefix.clone() + "-vmm-tests-junit-xml.xml";
                        let nextest_list = prefix.clone() + "-vmm-tests-nextest-list.json";

                        let junit_xml = artifact_dir.clone().join(&junit_xml);
                        let nextest_list = artifact_dir.clone().join(&nextest_list);

                        get_testcase_names_from_junit_xml(
                            &junit_xml,
                            &mut combined_junit_testcases,
                        )?;

                        get_testcase_names_from_nextest_list_json(
                            &nextest_list,
                            &mut combined_nextest_testcases,
                        )?;
                    }

                    assert!(
                        combined_junit_testcases == combined_nextest_testcases,
                        "Mismatch between test cases in junit XML and nextest list JSON.\n\
                        Test cases in junit XML but not in nextest list JSON: {:?}\n\
                        Test cases in nextest list JSON but not in junit XML: {:?}",
                        combined_junit_testcases
                            .difference(&combined_nextest_testcases)
                            .collect::<Vec<_>>(),
                        combined_nextest_testcases
                            .difference(&combined_junit_testcases)
                            .collect::<Vec<_>>(),
                    );

                    Ok(())
                }
            },
        );

        ctx.emit_side_effect_step(vec![parse], [done]);

        Ok(())
    }
}

fn get_testcase_names_from_junit_xml(
    junit_path: &PathBuf,
    test_names: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let mut reader = Reader::from_file(junit_path)?;

    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) if e.name().as_ref() == b"testcase" => {
                let mut name = None;
                let mut classname = None;

                for attr in e.attributes() {
                    let attr = attr?;
                    match attr.key.as_ref() {
                        b"name" => name = Some(attr.unescape_value()?.to_string()),
                        b"classname" => classname = Some(attr.unescape_value()?.to_string()),
                        _ => {}
                    }
                }

                test_names.insert(classname.unwrap() + "::" + &name.unwrap());
            }

            Event::Eof => break,
            _ => {}
        }
    }

    Ok(())
}

fn get_testcase_names_from_nextest_list_json(
    nextest_list_output_path: &PathBuf,
    test_names: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let data = fs_err::read_to_string(nextest_list_output_path)?;
    let root: Root = serde_json::from_str(&data)?;

    for (suite_name, suite) in root.rust_suites {
        for test_name in suite.testcases.keys() {
            test_names.insert(format!("{}::{}", suite_name, test_name));
        }
    }

    Ok(())
}
