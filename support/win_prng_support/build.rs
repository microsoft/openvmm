// Copyright (C) Microsoft Corporation. All rights reserved.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some()
        && std::env::var("CARGO_CFG_TARGET_ENV").unwrap() == "msvc"
    {
        win_import_lib::build_import_lib("ext-ms-win-cng-rng-l1-1-0").unwrap();
    }
}
