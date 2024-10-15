// Copyright (C) Microsoft Corporation. All rights reserved.

//! Crate used to fix up environment variables before running another cargo
//! command, to be used in cargo aliases.
//!
//! This crate is used to work around a bug in the `linkme` crate on macOS,
//! <https://github.com/dtolnay/linkme/issues/61>. If a binary is built without
//! LTO on macOS, sometimes `linkme` will fail to include all the elements of a
//! distributed slice. This causes flowey runs to fail.
//!
//! To work around this, we set the `CARGO_PROFILE_FLOWEY_LTO` environment
//! variable to `thin` before running the `cargo` binary. This will cause the
//! `flowey_hvlite` binary to be built with thin LTO, which will work around the
//! `linkme` bug.
//!
//! We don't want to set this for non-macOS environments because it slows down
//! builds.
//!
//! This crate can be removed when the `linkme` bug is fixed or when cargo gains
//! enough support to do this kind of thing natively.

use std::process::Command;

fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(&args[1..]);

    // Conditionally set LTO via environment variable. Note that this inherits
    // to any child invocations of cargo, which is what we want.
    //
    // This check isn't completely accurate, since we might be cross compiling
    // flowey from or to a different OS. But it's good enough for now.
    if cfg!(target_os = "macos") {
        cmd.env("CARGO_PROFILE_FLOWEY_LTO", "thin");
    }

    #[cfg(unix)]
    {
        let err = std::os::unix::process::CommandExt::exec(&mut cmd);
        panic!("failed to exec: {:?}", err);
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().expect("failed to run command");
        std::process::exit(status.code().unwrap_or(1));
    }
}
