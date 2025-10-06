// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compares the size of the OpenHCL binary in the current PR with the size of the binary from the last successful merge to main.
use flowey::node::prelude::*;
use quick_xml::Reader;
use quick_xml::events::Event;

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

        let parse = ctx.emit_rust_step("collect test names from junit logs", |ctx| {
            let artifacts: Vec<_> = test_artifacts
                .into_iter()
                .map(|(name, path)| (name, path.claim(ctx)))
                .collect();

            move |rt| {
                for (name, path) in artifacts {
                    let mut file = rt.read(path);
                    println!("File: {}", file.display());
                    assert!(file.exists(), "expected artifact file to exist");

                    file = file.join(&name);
                    println!("Expanded to: {}", file.display());
                    assert!(file.exists(), "expected artifact dir to exist");

                    file = file.join("junit.xml");
                    println!("Name: {}, File: {}", name, file.display());
                    assert!(
                        file.exists(),
                        "expected junit.xml file to exist at {}",
                        file.display()
                    );
                    let test_names = get_testcase_names(&file)?;
                    println!("Test names in {}:", name);
                    for test_name in test_names {
                        println!("  {}", test_name);
                    }
                }

                Ok(())
            }
        });

        ctx.emit_side_effect_step(vec![parse], [done]);

        Ok(())
    }
}

fn get_testcase_names(junit_path: &PathBuf) -> anyhow::Result<Vec<String>> {
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
