// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
use std::ffi::OsString;

// certain plugins (e.g: mdbook-admonish) also "helpfully" update book.toml as
// part of an `install` operation, without any way to opt-out.
fn preserve_book_toml(f: impl FnOnce()) {
    let old_book = std::fs::read("book.toml").unwrap();
    f();
    std::fs::write("book.toml", old_book).unwrap();
}

macro_rules! do_install {
    ($cmd:expr, $args:expr) => {
        std::process::Command::new(&$cmd)
            .args($args)
            .spawn()
            .unwrap()
            .wait()
            .unwrap();
    };
}

fn main() {
    let mut args = std::env::args_os().skip(1);

    let plugin = args.next().unwrap().into_string().unwrap();
    let args = args.collect::<Vec<OsString>>();

    eprintln!("plugin={plugin}, args={args:?}");

    let plugin_bin = {
        if let Ok(path) = std::env::var(format!("SHIM_{}", plugin.replace('-', "_").to_uppercase()))
        {
            path
        } else {
            plugin.clone()
        }
    };

    match plugin.as_ref() {
        "mdbook-admonish" => {
            if !std::fs::exists("mdbook-admonish.css").unwrap() {
                preserve_book_toml(|| {
                    do_install!(plugin_bin, ["install"]);
                })
            }
        }
        "mdbook-mermaid" => {
            if !std::fs::exists("mermaid.min.js").unwrap() {
                do_install!(plugin_bin, ["install"]);
            }
        }
        other => panic!("unknown plugin '{other}'"),
    };

    let status = std::process::Command::new(plugin_bin)
        .args(args)
        .spawn()
        .unwrap()
        .wait()
        .unwrap();

    std::process::exit(status.code().unwrap())
}
