// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ADO Task Wrapper: `PublishTestResults@2`

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize)]
pub enum AdoTestResultsFormat {
    JUnit,
    NUnit,
    VSTest,
    XUnit,
    CTest,
}

impl std::fmt::Display for AdoTestResultsFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdoTestResultsFormat::JUnit => write!(f, "JUnit"),
            AdoTestResultsFormat::NUnit => write!(f, "NUnit"),
            AdoTestResultsFormat::VSTest => write!(f, "VSTest"),
            AdoTestResultsFormat::XUnit => write!(f, "XUnit"),
            AdoTestResultsFormat::CTest => write!(f, "CTest"),
        }
    }
}

flowey_request! {
    pub struct Request {
        pub step_name: String,
        pub format: AdoTestResultsFormat,
        pub results_file: ReadVar<PathBuf>,
        pub test_title: String,
        pub condition: Option<ReadVar<bool>>,
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        for Request {
            step_name,
            format,
            results_file,
            test_title,
            condition,
            done,
        } in requests
        {
            let results_file = results_file.map(ctx, |f| {
                f.absolute().expect("invalid path").display().to_string()
            });
            ctx.emit_ado_step_with_condition_optional(step_name, condition, |ctx| {
                done.claim(ctx);
                let results_file = results_file.claim(ctx);
                move |rt| {
                    let results_file = rt.get_var(results_file).as_raw_var_name();
                    format!(
                        r#"
                            - task: PublishTestResults@2
                              inputs:
                                testResultsFormat: '{format}'
                                testResultsFiles: '$({results_file})'
                                testRunTitle: '{test_title}'
                        "#
                    )
                }
            });
        }

        Ok(())
    }
}
