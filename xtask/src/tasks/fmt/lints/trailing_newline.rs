// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that text files end with exactly one trailing newline.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use toml_edit::DocumentMut;

const CHECKED_EXTENSIONS: &[&str] = &[
    "c", "md", "proto", "py", "rs", "sh", "toml", "txt", "yml", "js", "ts",
];

pub struct TrailingNewline;

impl Lint for TrailingNewline {
    fn new(_ctx: &LintCtx) -> Self {
        TrailingNewline
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}
    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {}

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        let mut ending_newlines = 0;
        for c in content.as_bytes().iter().rev() {
            if *c == b'\n' {
                ending_newlines += 1;
            } else {
                break;
            }
        }

        if ending_newlines == 0 {
            content.fix("missing trailing newline", |content| content.push('\n'));
        } else if ending_newlines != 1 {
            content.fix("too many trailing newlines", |content| {
                // Trim all trailing newlines but one.
                content.truncate(content.len() - ending_newlines + 1);
            });
        }
    }

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        // toml_edit unfortunately makes checking for a trailing newline
        // inconvenient, as it parses into the decor of the last item, not the
        // document as a whole. It's easier to just check the raw file content.
        let mut ending_newlines = 0;
        for c in content.raw().unwrap().as_bytes().iter().rev() {
            if *c == b'\n' {
                ending_newlines += 1;
            } else {
                break;
            }
        }

        // Writing a trailing newline is also annoying, as it likes to synthesize
        // one if it was originally missing. So we just trim all trailing newlines
        // and let it add one back if needed, being careful not to remove any trailing
        // comments or the like.
        if ending_newlines != 1 {
            content.fix("missing or too many trailing newlines", |content| {
                content.set_trailing(content.trailing().as_str().unwrap().trim_end().to_owned());
            });
        }
    }

    fn exit_workspace(&mut self, content: &mut Lintable<DocumentMut>) {
        self.exit_crate(content)
    }

    fn visit_nonrust_file(&mut self, extension: &str, content: &mut Lintable<String>) {
        // workaround for `mdbook-docfx` emitting yaml with no trailing newline
        if content.path().file_name().unwrap_or_default() == "toc.yml" {
            return;
        }

        // TODO: Should we just check everything?
        if CHECKED_EXTENSIONS.contains(&extension) {
            self.visit_file(content)
        }
    }
}
