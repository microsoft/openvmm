// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that uses of `expect(unsafe_code)` or `allow(unsafe_code)` are
//! preceded by an `// UNSAFETY: ` comment explaining the justification.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use toml_edit::DocumentMut;

pub struct UnsafeCodeComment;

impl Lint for UnsafeCodeComment {
    fn new(_ctx: &LintCtx) -> Self {
        UnsafeCodeComment
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}
    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {}

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        // Exclude ourselves from the lint (we mention the patterns in strings).
        if content.path().ends_with(file!()) {
            return;
        }

        let mut in_comment = false;
        for (i, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.starts_with("// UNSAFETY: ") {
                in_comment = true;
                continue;
            }

            if (line.contains("expect(unsafe_code)") || line.contains("allow(unsafe_code)"))
                && !in_comment
            {
                content.unfixable(&format!(
                    "unjustified `expect(unsafe_code)` at line {}",
                    i + 1
                ));
            }

            if !line.starts_with("//") || (line.len() > 2 && line.as_bytes()[2] != b' ') {
                in_comment = false;
            }
        }
    }

    fn exit_crate(&mut self, _content: &mut Lintable<DocumentMut>) {}
    fn exit_workspace(&mut self, _content: &mut Lintable<DocumentMut>) {}
}
