// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compares the size of the OpenHCL binary in the current PR with the size of the binary from the last successful merge to main.
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

        // TODO: testing with GitHub first, will add ADO support later.
        if ctx.backend() != FlowBackend::Github {
            return Ok(());
        }

        let parse = ctx.emit_rust_step(
            "parse and analyze junit logs and nextest list output",
            |ctx| {
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

                        let junit_xml = prefix.clone() + "vmm-tests-junit-xml.xml";
                        let nextest_list = prefix.clone() + "vmm-tests-nextest-list.json";

                        let junit_xml = artifact_dir.clone().join(&junit_xml);
                        let nextest_list = artifact_dir.clone().join(&nextest_list);

                        assert!(
                            junit_xml.exists(),
                            "expected junit.xml file to exist at {}",
                            junit_xml.display()
                        );
                        assert!(
                            nextest_list.exists(),
                            "expected nextest list file to exist at {}",
                            nextest_list.display()
                        );

                        let junit_test_names = get_testcase_names_from_junit_xml(&junit_xml)?;
                        println!("Test names in {}:", junit_xml.display());
                        for test_name in &junit_test_names {
                            println!("{}", test_name);
                        }

                        let nextest_test_names =
                            get_testcase_names_from_nextest_list_json(&nextest_list)?;
                        println!("Test names in {}:", nextest_list.display());
                        for test_name in &nextest_test_names {
                            println!("{}", test_name);
                        }

                        combined_junit_testcases.extend(junit_test_names.into_iter());
                        combined_nextest_testcases.extend(nextest_test_names.into_iter());
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

fn get_testcase_names_from_junit_xml(junit_path: &PathBuf) -> anyhow::Result<Vec<String>> {
    let mut reader = Reader::from_file(junit_path)?;

    let mut buf = Vec::new();
    let mut test_names = Vec::new();

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

                test_names.push(classname.unwrap() + "::" + &name.unwrap());
            }

            Event::Eof => break,
            _ => {}
        }
    }

    Ok(test_names)
}

fn get_testcase_names_from_nextest_list_json(
    nextest_list_output_path: &PathBuf,
) -> anyhow::Result<Vec<String>> {
    let data = fs_err::read_to_string(nextest_list_output_path)?;
    let root: Root = serde_json::from_str(&data)?;
    let mut test_names = Vec::new();

    for (suite_name, suite) in root.rust_suites {
        for test_name in suite.testcases.keys() {
            test_names.push(format!("{}::{}", suite_name, test_name));
        }
    }

    Ok(test_names)
}
