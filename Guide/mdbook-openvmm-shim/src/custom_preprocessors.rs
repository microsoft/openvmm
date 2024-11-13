// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::Path;
use std::path::PathBuf;

/// Fixup `#include` directives to work with book-root relative paths.
///
/// Workaround for <https://github.com/rust-lang/mdBook/issues/1512>
pub fn fixup_include_book_relative_path(context: serde_json::Value, book: &mut serde_json::Value) {
    fn inner(book_root: &Path, sections: &mut serde_json::Value) {
        for e in sections.as_array_mut().unwrap() {
            let Some(c) = e.as_object_mut().and_then(|o| o.get_mut("Chapter")) else {
                continue;
            };

            inner(book_root, &mut c["sub_items"]);

            let content_folder = {
                let Some(src_path) = c["source_path"].as_str() else {
                    continue;
                };
                let src_path = Path::new(src_path);
                book_root.join("src").join(src_path.parent().unwrap())
            };

            let mut s = c["content"].as_str().unwrap();
            let mut new_s = String::new();
            while !s.is_empty() {
                let Some((prev, rem)) = s.split_once("{{") else {
                    new_s += s;
                    break;
                };
                let Some((possible_include, rest)) = rem.split_once("}}") else {
                    new_s += s;
                    break;
                };

                // cool, we are in a {{ template }} context
                s = rest;

                let mut args = possible_include.split_whitespace();
                if args.next() != Some("#include") {
                    new_s = format!("{new_s}{prev}{{{{{possible_include}}}}}");
                    continue;
                }

                // fixup the path
                let path = args.next().expect("invalid {{ #include }} syntax");
                let path = if let Some(path) = path.strip_prefix('/') {
                    pathdiff::diff_paths(book_root.join("src").join(path), &content_folder).unwrap()
                } else {
                    PathBuf::from(path)
                };

                new_s = format!("{new_s}{prev}{{{{#include {}}}}}", path.display());
            }

            c["content"] = new_s.into();
        }
    }

    inner(
        Path::new(context["root"].as_str().unwrap()),
        &mut book["sections"],
    );
}
