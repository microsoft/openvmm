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
        // comments or the like. It only synthesizes a newline after the last
        // item, so a trailing comment needs its newline added back here.
        if ending_newlines != 1 {
            content.fix("missing or too many trailing newlines", |content| {
                let trailing = content.trailing().as_str().unwrap().trim_end();
                let trailing = if trailing.is_empty() {
                    String::new()
                } else {
                    format!("{trailing}\n")
                };
                content.set_trailing(trailing);
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

#[cfg(test)]
mod tests {
    use super::super::test_support::failed;
    use super::super::test_support::lintable;
    use super::super::test_support::manifest;
    use super::*;

    fn fixed(text: &str) -> (String, bool) {
        let mut file = lintable("src/lib.rs", text.to_owned(), true);
        TrailingNewline.visit_file(&mut file);
        (file.content.clone(), file.modified)
    }

    // Test: Fix files with no, one, several and only trailing newlines.
    // Expected: Every fixed file ends with exactly one newline, interior blank
    //       lines are untouched, and an already-correct file is not modified.
    #[test]
    fn fixes_trailing_newlines_to_exactly_one() {
        assert_eq!(fixed("fn a() {}"), ("fn a() {}\n".into(), true));
        assert_eq!(fixed("a\n\nb\n"), ("a\n\nb\n".into(), false));
        assert_eq!(fixed("a\n\n\n\n"), ("a\n".into(), true));
        assert_eq!(fixed("\n\n"), ("\n".into(), true));
        assert_eq!(fixed(""), ("\n".into(), true));
    }

    // Test: Fix Cargo.toml manifests with extra trailing newlines or none, with
    //       and without a trailing comment, then check a correct manifest.
    // Expected: Each fixed manifest ends with exactly one newline and keeps its
    //       trailing comment; the correct manifest is not modified.
    #[test]
    fn fixes_manifest_trailing_newlines_and_keeps_comments() {
        let fixed = |raw: &str| {
            let mut file = manifest("a/Cargo.toml", raw, true);
            TrailingNewline.exit_crate(&mut file);
            (file.content.to_string(), file.modified)
        };
        let commented = "[package]\nname = \"a\"\n# end\n";
        let plain = "[package]\nname = \"a\"\n";
        assert_eq!(
            fixed("[package]\nname = \"a\"\n# end\n\n\n"),
            (commented.to_owned(), true)
        );
        assert_eq!(
            fixed("[package]\nname = \"a\"\n# end"),
            (commented.to_owned(), true)
        );
        assert_eq!(
            fixed("[package]\nname = \"a\"\n\n\n"),
            (plain.to_owned(), true)
        );
        assert_eq!(fixed(commented), (commented.to_owned(), false));
    }

    // Test: Check, without fixing, a file that lacks its trailing newline, and
    //       visit non-Rust files with checked, unchecked and exempt names.
    // Expected: Check mode reports failure without editing; unchecked extensions
    //       and the generated toc.yml are ignored.
    #[test]
    fn check_mode_reports_and_respects_extension_filters() {
        let mut file = lintable("src/lib.rs", "x".to_owned(), false);
        TrailingNewline.visit_file(&mut file);
        assert!(failed(&file));
        assert_eq!((file.content.as_str(), file.modified), ("x", false));

        for (path, ext, expect_failure) in [
            ("docs/a.md", "md", true),
            ("img/a.svg", "svg", false),
            ("Guide/toc.yml", "yml", false),
            ("ci/a.yml", "yml", true),
        ] {
            let mut file = lintable(path, "x".to_owned(), false);
            TrailingNewline.visit_nonrust_file(ext, &mut file);
            assert_eq!(failed(&file), expect_failure, "{path}");
        }
    }
}
