// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some()
        && std::env::var("CARGO_CFG_TARGET_ENV").unwrap() == "msvc"
    {
        win_import_lib::build_import_lib("api-ms-win-security-base-private-l1-1-1").unwrap();
    }
}
