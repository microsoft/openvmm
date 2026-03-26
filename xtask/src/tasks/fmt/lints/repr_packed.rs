// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that `#[repr(packed)]` is not used without `C` — it should be
//! `#[repr(C, packed)]`.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use toml_edit::DocumentMut;

pub struct ReprPacked;

impl Lint for ReprPacked {
    fn new(_ctx: &LintCtx) -> Self {
        ReprPacked
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}
    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {}

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        // Collect byte offsets of `repr(packed` on lines where the attribute
        // is the sole content, so we skip occurrences in comments or strings.

        let mut offsets: Vec<usize> = Vec::new();
        let mut pos = 0;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed == "#[repr(packed)]"
                || (trimmed.starts_with("#[repr(packed(")
                    && trimmed.ends_with(")]")
                    && !trimmed.contains("C"))
            {
                let repr_off = line.find("repr(packed").unwrap();
                offsets.push(pos + repr_off);
            }
            pos += line.len() + 1; // +1 for the newline
        }

        if offsets.is_empty() {
            return;
        }

        // Process offsets in reverse order so that replacements don't
        // invalidate later positions.
        for &off in offsets.iter().rev() {
            content.fix(
                "`#[repr(packed)]` should be `#[repr(C, packed)]`",
                |content| {
                    content.insert_str(off + "repr(".len(), "C, ");
                },
            );
        }
    }

    fn exit_crate(&mut self, _content: &mut Lintable<DocumentMut>) {}
    fn exit_workspace(&mut self, _content: &mut Lintable<DocumentMut>) {}
}
