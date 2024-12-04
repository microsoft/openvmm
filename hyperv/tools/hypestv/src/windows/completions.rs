// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interactive tab completion handling.

use super::InteractiveCommand;
use mesh::rpc::RpcSend;
use rustyline::Helper;
use rustyline::Highlighter;
use rustyline::Hinter;
use rustyline::Validator;

#[derive(Helper, Highlighter, Hinter, Validator)]
pub(crate) struct OpenvmmRustylineEditor {
    pub(crate) req: std::sync::Arc<mesh::Sender<super::Request>>,
}

impl rustyline::completion::Completer for OpenvmmRustylineEditor {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let Ok(cmd) = shell_words::split(line) else {
            return Ok((0, Vec::with_capacity(0)));
        };

        let completions = futures::executor::block_on(
            clap_dyn_complete::Complete {
                cmd,
                raw: Some(line.into()),
                position: Some(pos),
            }
            .generate_completions::<InteractiveCommand>(None, self),
        );

        let pos_from_end = {
            let line = line.chars().take(pos).collect::<String>();

            let trailing_ws = line.len() - line.trim_end().len();

            if trailing_ws > 0 {
                line.len() - trailing_ws + 1 // +1 for the space
            } else {
                let last_word = shell_words::split(&line)
                    .unwrap_or_default()
                    .last()
                    .cloned()
                    .unwrap_or_default();

                line.len() - last_word.len()
            }
        };

        Ok((pos_from_end, completions))
    }
}

impl clap_dyn_complete::CustomCompleterFactory for &OpenvmmRustylineEditor {
    type CustomCompleter = OpenvmmComplete;
    async fn build(&self, _ctx: &clap_dyn_complete::RootCtx<'_>) -> Self::CustomCompleter {
        OpenvmmComplete {
            req: self.req.clone(),
        }
    }
}

pub struct OpenvmmComplete {
    req: std::sync::Arc<mesh::Sender<super::Request>>,
}

impl clap_dyn_complete::CustomCompleter for OpenvmmComplete {
    async fn complete(
        &self,
        ctx: &clap_dyn_complete::RootCtx<'_>,
        subcommand_path: &[&str],
        arg_id: &str,
    ) -> Vec<String> {
        match (subcommand_path, arg_id) {
            (["hypestv", "paravisor", "inspect"], "element") => {
                let on_error = vec!["failed/to/connect".into()];

                let (parent_path, to_complete) = (ctx.to_complete)
                    .rsplit_once('/')
                    .unwrap_or(("", ctx.to_complete));

                let node = {
                    let r = self
                        .req
                        .call_failable(
                            super::Request::Inspect,
                            (super::InspectTarget::Paravisor, parent_path.to_owned()),
                        )
                        .await;
                    let Ok(node) = r else {
                        return on_error;
                    };

                    node
                };

                let mut completions = Vec::new();

                if let inspect::Node::Dir(dir) = node {
                    for entry in dir {
                        if entry.name.starts_with(to_complete) {
                            if parent_path.is_empty() {
                                completions.push(format!("{}/", entry.name))
                            } else {
                                completions.push(format!(
                                    "{}/{}{}",
                                    parent_path,
                                    entry.name,
                                    if matches!(entry.node, inspect::Node::Dir(..)) {
                                        "/"
                                    } else {
                                        ""
                                    }
                                ))
                            }
                        }
                    }
                } else {
                    return on_error;
                }

                completions
            }
            _ => Vec::new(),
        }
    }
}
