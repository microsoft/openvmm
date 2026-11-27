// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks that source files have the correct copyright and license header.
//!
//! The expected header for most file types is:
//!
//! ```text
//! // Copyright (c) Microsoft Corporation.
//! // Licensed under the MIT License.
//!
//! ```
//!
//! Files may start with a shebang (`#!`) or `<!DOCTYPE html>` before the
//! header. A blank line is expected between the shebang and the header.
//!
//! Files with a non-Microsoft copyright are left alone.

use super::Lint;
use super::LintCtx;
use super::Lintable;
use toml_edit::DocumentMut;

const HEADER_MIT_FIRST: &str = "Copyright (c) Microsoft Corporation.";
const HEADER_MIT_SECOND: &str = "Licensed under the MIT License.";

const CHECKED_EXTENSIONS: &[&str] = &[
    "c", "css", "html", "js", "proto", "ps1", "py", "rs", "toml", "ts", "tsx",
];

/// Returns the comment prefix and suffix for a given file extension.
fn comment_delimiters(ext: &str) -> (&'static str, &'static str) {
    match ext {
        "rs" | "c" | "proto" | "ts" | "tsx" | "js" => ("//", ""),
        "toml" | "py" | "ps1" => ("#", ""),
        "css" => ("/*", " */"),
        "html" => ("<!--", " -->"),
        _ => unreachable!(),
    }
}

pub struct Copyright {
    is_msft_internal: bool,
}

impl Lint for Copyright {
    fn new(_ctx: &LintCtx) -> Self {
        Copyright {
            is_msft_internal: std::env::var("XTASK_FMT_COPYRIGHT_ALLOW_MISSING_MIT").is_ok(),
        }
    }

    fn enter_workspace(&mut self, _content: &Lintable<DocumentMut>) {}
    fn enter_crate(&mut self, _content: &Lintable<DocumentMut>) {}

    fn visit_file(&mut self, content: &mut Lintable<String>) {
        self.check(content, "rs");
    }

    fn exit_crate(&mut self, content: &mut Lintable<DocumentMut>) {
        self.check_toml(content, "package");
    }
    fn exit_workspace(&mut self, content: &mut Lintable<DocumentMut>) {
        self.check_toml(content, "workspace");
    }

    fn visit_nonrust_file(&mut self, extension: &str, content: &mut Lintable<String>) {
        // TODO: should we check everything regardless of extension?
        if CHECKED_EXTENSIONS.contains(&extension) {
            self.check(content, extension);
        }
    }
}

impl Copyright {
    fn check_toml(&self, content: &mut Lintable<DocumentMut>, section_name: &str) {
        let table = content[section_name].as_table().unwrap();
        let prefix = table
            .decor()
            .prefix()
            .and_then(|x| x.as_str())
            .unwrap_or("");

        // TEMP: until we have more robust infrastructure for distinct
        // microsoft-internal checks, include this "escape hatch" for preserving
        // non-MIT licensed files when running `xtask fmt` in the msft internal
        // repo. This uses a job-specific env var, instead of being properly plumbed
        // through via `clap`, to make it easier to remove in the future.
        if self.is_msft_internal {
            // Support both new and existing copyright banner styles
            if !(prefix.contains("Copyright") && prefix.contains("Microsoft")) {
                let prefix = prefix.trim().to_owned();
                content.fix("missing or incorrect internal copyright header", |content| {
                    let table = content[section_name].as_table_mut().unwrap();
                    let new_prefix = format!(
                        "# Copyright (C) Microsoft Corporation. All rights reserved.\n\n{prefix}",
                    );
                    table.decor_mut().set_prefix(new_prefix);
                });
            }
        } else if !(prefix.starts_with("# ")
            && prefix[2..].starts_with(HEADER_MIT_FIRST)
            && prefix[3 + HEADER_MIT_FIRST.len()..].starts_with("# ")
            && prefix[5 + HEADER_MIT_FIRST.len()..].contains(HEADER_MIT_SECOND))
        {
            let prefix = prefix.trim().to_owned();
            content.fix("missing or incorrect copyright header", |content| {
                let table = content[section_name].as_table_mut().unwrap();
                let new_prefix =
                    format!("# {HEADER_MIT_FIRST}\n# {HEADER_MIT_SECOND}\n\n{prefix}",);
                table.decor_mut().set_prefix(new_prefix);
            });
        }
    }

    fn check(&self, content: &mut Lintable<String>, ext: &str) {
        // Skip a leading UTF-8 BOM if present.
        let has_bom = content.starts_with('\u{feff}');
        let mut lines = content.strip_prefix('\u{feff}').unwrap_or(content).lines();
        let first_line = lines.next().unwrap_or("").to_owned();

        // Someone may decide to put a script interpreter line (aka "shebang")
        // in a .config or a .toml file, and mark the file as executable. While
        // that's not common, we choose not to constrain creativity.
        //
        // The shebang (`#!`) is part of the valid grammar of Rust, and does not
        // indicate that the file should be interpreted as a script. So we don't
        // allow that line in Rust files.
        //
        // Some HTML files may start with a `<!DOCTYPE html>` line, so let that line pass as well
        let (has_special, blank_after_special, header_first) = if (first_line.starts_with("#!")
            && ext != "rs")
            || (first_line.starts_with("<!DOCTYPE html>") && ext == "html")
        {
            let second = lines.next().unwrap_or("").to_owned();
            let blank = second.is_empty();
            let header_start = if blank {
                lines.next().unwrap_or("").to_owned()
            } else {
                second
            };
            (true, blank, header_start)
        } else {
            (false, false, first_line.clone())
        };

        let header_second = lines.next().unwrap_or("").to_owned();
        let after_header_line = lines.next().unwrap_or("").to_owned();

        // Preserve any files which are copyright, but not by Microsoft.
        if header_first.contains("Copyright") && !header_first.contains("Microsoft") {
            return;
        }

        let (prefix, suffix) = comment_delimiters(ext);

        let expected_first = format!("{prefix} {HEADER_MIT_FIRST}{suffix}");
        let expected_second = format!("{prefix} {HEADER_MIT_SECOND}{suffix}");

        let has_first = header_first.contains(HEADER_MIT_FIRST);
        let has_second = header_second.contains(HEADER_MIT_SECOND);
        let mut missing_banner = !has_first || !has_second;
        let mut expected_header_lines = 2;

        // TEMP: until we have more robust infrastructure for distinct
        // microsoft-internal checks, include this "escape hatch" for preserving
        // non-MIT licensed files when running `xtask fmt` in the msft internal
        // repo. This uses a job-specific env var, instead of being properly plumbed
        // through via `clap`, to make it easier to remove in the future.
        if self.is_msft_internal && missing_banner {
            // Support both new and existing copyright banner styles
            missing_banner =
                !(header_first.contains("Copyright") && header_first.contains("Microsoft"));
            expected_header_lines = 1;
        }

        let missing_blank_after_header = if missing_banner {
            // Will be fixed as part of inserting the banner.
            false
        } else if expected_header_lines == 1 {
            !header_second.is_empty()
        } else {
            !after_header_line.is_empty()
        };

        let missing_blank_after_special = has_special && !blank_after_special;

        if !missing_banner && !missing_blank_after_header && !missing_blank_after_special {
            return;
        }

        content.fix("missing or incorrect copyright header", |content| {
            // Build the replacement header.
            let mut hdr = String::new();
            if has_bom {
                hdr.push('\u{feff}');
            }
            if has_special {
                hdr.push_str(&first_line);
                hdr.push_str("\n\n");
            }
            hdr.push_str(&expected_first);
            hdr.push('\n');
            if expected_header_lines == 2 {
                hdr.push_str(&expected_second);
                hdr.push('\n');
            }
            hdr.push('\n');

            // Count leading lines to replace and find their byte length.
            let skip = (has_special as usize)
                + (has_special && blank_after_special) as usize
                + if !missing_banner {
                    expected_header_lines + (!missing_blank_after_header) as usize
                } else {
                    0
                };
            let skip_bytes = if skip > 0 {
                content
                    .match_indices('\n')
                    .nth(skip - 1)
                    .map_or(content.len(), |(i, _)| i + 1)
            } else if has_bom {
                '\u{feff}'.len_utf8()
            } else {
                0
            };

            content.replace_range(..skip_bytes, &hdr);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::failed;
    use super::super::test_support::lintable;
    use super::*;

    const RS: &str =
        "// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.\n\n";
    const PY: &str =
        "# Copyright (c) Microsoft Corporation.\n# Licensed under the MIT License.\n\n";

    fn fix(ext: &str, text: &str) -> (String, bool) {
        let mut file = lintable("f", text.to_owned(), true);
        Copyright {
            is_msft_internal: false,
        }
        .check(&mut file, ext);
        (file.content.clone(), file.modified)
    }

    // Test: Fix Rust, Python, CSS and HTML files lacking a header, and a Rust
    //       file whose header lacks the following blank line.
    // Expected: Each gets the exact two-line header with its comment syntax and
    //       one blank line, while the original code follows unchanged.
    #[test]
    fn inserts_exact_header_with_language_delimiters() {
        assert_eq!(fix("rs", "fn a() {}\n").0, format!("{RS}fn a() {{}}\n"));
        assert_eq!(fix("py", "print()\n").0, format!("{PY}print()\n"));
        assert_eq!(
            fix("css", "a {}\n").0,
            "/* Copyright (c) Microsoft Corporation. */\n/* Licensed under the MIT License. */\n\na {}\n"
        );
        assert_eq!(
            fix("html", "<p/>\n").0,
            "<!-- Copyright (c) Microsoft Corporation. -->\n<!-- Licensed under the MIT License. -->\n\n<p/>\n"
        );
        let tight = "// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.\nfn a() {}\n";
        assert_eq!(fix("rs", tight).0, format!("{RS}fn a() {{}}\n"));
    }

    // Test: Check correctly headed files, a non-Microsoft copyright, a script
    //       shebang and HTML doctype with and without a following blank line.
    // Expected: Correct and third-party files stay untouched; a special first
    //       line is kept and separated from the header by exactly one blank line.
    #[test]
    fn preserves_valid_third_party_and_special_first_lines() {
        assert_eq!(fix("rs", &format!("{RS}fn a() {{}}\n")).1, false);
        let other = "// Copyright (c) Contoso.\n// Apache-2.0\nfn a() {}\n";
        assert_eq!(fix("rs", other), (other.to_owned(), false));

        let shebang_ok = format!("#!/usr/bin/env python3\n\n{PY}print()\n");
        assert_eq!(fix("py", &shebang_ok).1, false);
        let shebang_tight = format!("#!/usr/bin/env python3\n{PY}print()\n");
        assert_eq!(fix("py", &shebang_tight).0, shebang_ok);
        let doctype = "<!DOCTYPE html>\n<p/>\n";
        assert_eq!(
            fix("html", doctype).0,
            "<!DOCTYPE html>\n\n<!-- Copyright (c) Microsoft Corporation. -->\n<!-- Licensed under the MIT License. -->\n\n<p/>\n"
        );
    }

    // Test: Fix a Rust file starting with `#!`, a file starting with a BOM with
    //       and without a header, then check a headerless file in check mode.
    // Expected: Rust shebang-like lines are not treated as special, a BOM stays
    //       first exactly once, and check mode reports failure without editing
    //       the file.
    #[test]
    fn handles_rust_shebang_bom_and_check_mode() {
        assert_eq!(fix("rs", "#![no_std]\n").0, format!("{RS}#![no_std]\n"));
        assert_eq!(
            fix("rs", "\u{feff}fn a() {}\n").0,
            format!("\u{feff}{RS}fn a() {{}}\n")
        );
        let bom_tight = "\u{feff}// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.\nfn a() {}\n";
        assert_eq!(fix("rs", bom_tight).0, format!("\u{feff}{RS}fn a() {{}}\n"));

        let mut file = lintable("f.rs", "fn a() {}\n".to_owned(), false);
        Copyright {
            is_msft_internal: false,
        }
        .check(&mut file, "rs");
        assert!(failed(&file));
        assert_eq!(file.content.as_str(), "fn a() {}\n");
    }
}
