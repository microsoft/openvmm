// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(with_encryption)]

#[allow(unused_macros)]
macro_rules! activate {
    ($plat:ident) => {
        mod $plat;
        pub use $plat::vmgs_decrypt;
        pub use $plat::vmgs_encrypt;
    };
}

#[cfg(unix)]
cfg_if::cfg_if! {
    // HACK: ensure `encryption_ossl` is set first, so as to not break
    // `cargo test --all-features -p vmgs`.
    //
    // Yes, I know this is gross.
    if #[cfg(feature = "encryption_ossl")] {
        activate!(ossl);
    } else if #[cfg(feature = "encryption_win")] {
        compile_error!("cannot use encryption_win on unix!");
    } else {
        compile_error!("unreachable due to #![cfg(with_encryption)]");
    }
}

#[cfg(windows)]
cfg_if::cfg_if! {
    // HACK: ensure `encryption_win` is set first, so as to not break
    // `cargo test --all-features -p vmgs` on windows.
    //
    // Yes, I know this is gross.
    if #[cfg(feature = "encryption_win")] {
        activate!(win);
    } else if #[cfg(feature = "encryption_ossl")] {
        activate!(ossl);
    } else {
        compile_error!("unreachable due to #![cfg(with_encryption)]");
    }
}
