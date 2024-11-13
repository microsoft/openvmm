// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ffi::OsString;

mod custom_preprocessors;

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

    if plugin == "openvmm-custom" {
        return openvmm_custom(args);
    }

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

fn openvmm_custom(args: Vec<OsString>) {
    if args.first().map(|s| s == "supports").unwrap_or(false) {
        // no need to inspect what backend is being used - the current custom
        // preprocessors we implement support all backends.
        std::process::exit(0);
    }

    // avoid taking a dependency on the `mdbook` library for now, since its
    // pretty heavy.
    //
    // if we decide to do some more involved preprocessing for whatever reason,
    // then the calculus here likely changes.
    let [context, mut book]: [serde_json::Value; 2] =
        serde_json::from_reader(std::io::stdin().lock()).unwrap();

    custom_preprocessors::fixup_include_book_relative_path(context, &mut book);

    println!("{book}")
}
