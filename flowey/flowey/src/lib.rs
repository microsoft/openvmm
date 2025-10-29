// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

//! The user-facing flowey API.
//!
//! Relying on `flowey_core` directly is not advised, as many APIs exposed at
//! that level are only supposed to be used by flowey _infrastructure_ (e.g: in
//! `flowey_cli`).

/// Types and traits for implementing flowey nodes.
pub mod node {
    pub mod prelude {
        // include all user-facing types in the prelude
        pub use flowey_core::match_arch;
        pub use flowey_core::node::user_facing::*;

        // ...in addition, export various types/traits that node impls are
        // almost certainly going to require
        pub use anyhow;
        pub use anyhow::Context;
        pub use fs_err;
        pub use log;
        pub use serde::Deserialize;
        pub use serde::Serialize;
        pub use std::path::Path;
        pub use std::path::PathBuf;

        /// Extension trait to streamline working with [`Path`] in flowey.
        pub trait FloweyPathExt {
            /// Alias for [`std::path::absolute`]
            fn absolute(&self) -> std::io::Result<PathBuf>;

            /// Helper to make files executable on unix-like platforms
            fn make_executable(&self) -> std::io::Result<()>;
        }

        impl<T> FloweyPathExt for T
        where
            T: AsRef<Path>,
        {
            fn absolute(&self) -> std::io::Result<PathBuf> {
                std::path::absolute(self)
            }

            fn make_executable(&self) -> std::io::Result<()> {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let path = self.as_ref();
                    let old_mode = path.metadata()?.permissions().mode();
                    fs_err::set_permissions(
                        path,
                        std::fs::Permissions::from_mode(old_mode | 0o111),
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// Types and traits for implementing flowey pipelines.
pub mod pipeline {
    pub mod prelude {
        pub use flowey_core::pipeline::user_facing::*;
    }
}

/// Types and traits for implementing flowey patch functions.
pub mod patch {
    pub use flowey_core::patch::*;
    pub use flowey_core::register_patch;
}

/// Utility functions.
pub mod util {
    pub use flowey_core::util::*;
}
