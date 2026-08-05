// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM product version and build identity.

#![expect(missing_docs)]

#[cfg(test)]
#[path = "../version.rs"]
mod version;

#[derive(Debug)]
pub struct BuildInfo {
    version: &'static str,
    long_version: &'static str,
}

impl BuildInfo {
    pub const fn new() -> Self {
        Self {
            version: env!("OPENVMM_VERSION"),
            long_version: include_str!(concat!(env!("OUT_DIR"), "/long_version.txt")),
        }
    }

    pub const fn version(&self) -> &'static str {
        self.version
    }

    pub const fn long_version(&self) -> &'static str {
        self.long_version
    }
}

impl Default for BuildInfo {
    fn default() -> Self {
        Self::new()
    }
}

static BUILD_INFO: BuildInfo = BuildInfo::new();

pub fn get() -> &'static BuildInfo {
    &BUILD_INFO
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn long_version_carries_the_identity() {
        let info = get();
        let long = info.long_version();
        assert!(long.starts_with(info.version()), "{long:?}");
        assert!(long.contains("version:"), "{long:?}");
        assert!(long.contains("commit:"), "{long:?}");
        assert!(long.contains("host:"), "{long:?}");
    }
}
