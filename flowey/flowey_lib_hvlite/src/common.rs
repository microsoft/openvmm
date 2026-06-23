// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Vocabulary types for the most-common build profiles, architectures,
//! platforms, and target triples used in the OpenVMM/OpenHCL tree.
//!
//! Outside of a few binaries / libraries that are intimately tied to one
//! particular architecture / platform, most things in the hvlite tree run on a
//! common subset of supported target triples + build profiles.

use flowey::node::prelude::*;

/// Vocabulary type for artifacts that only get built using the two most
/// common cargo build profiles (i.e: `release` vs. `debug`).
///
/// More specialized artifacts should use the
/// [`BuildProfile`](crate::run_cargo_build::BuildProfile) type, which
/// enumerates _all_ build profiles defined in HvLite's `Cargo.toml` file.
#[derive(Serialize, Deserialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CommonProfile {
    Release,
    Debug,
}

impl CommonProfile {
    pub fn from_release(release: bool) -> Self {
        match release {
            true => Self::Release,
            false => Self::Debug,
        }
    }

    pub fn to_release(self) -> bool {
        match self {
            Self::Release => true,
            Self::Debug => false,
        }
    }
}

impl From<CommonProfile> for crate::run_cargo_build::BuildProfile {
    fn from(value: CommonProfile) -> Self {
        match value {
            CommonProfile::Release => crate::run_cargo_build::BuildProfile::Release,
            CommonProfile::Debug => crate::run_cargo_build::BuildProfile::Debug,
        }
    }
}

/// Vocabulary type for artifacts that only get built for the most common
/// actively-supported architectures in the hvlite tree.
#[derive(Serialize, Deserialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CommonArch {
    X86_64,
    Aarch64,
}

impl CommonArch {
    /// Convert to a [`target_lexicon::Architecture`].
    pub fn as_arch(&self) -> target_lexicon::Architecture {
        match self {
            CommonArch::X86_64 => target_lexicon::Architecture::X86_64,
            CommonArch::Aarch64 => {
                target_lexicon::Architecture::Aarch64(target_lexicon::Aarch64Architecture::Aarch64)
            }
        }
    }

    /// Convert from a [`target_lexicon::Triple`], failing if the triple's
    /// architecture is not one of the common architectures.
    pub fn from_triple(triple: &target_lexicon::Triple) -> anyhow::Result<Self> {
        Self::from_architecture(triple.architecture)
    }

    /// Convert from a [`target_lexicon::Architecture`], failing if it is not
    /// one of the common architectures.
    pub fn from_architecture(arch: target_lexicon::Architecture) -> anyhow::Result<Self> {
        Ok(match arch {
            target_lexicon::Architecture::Aarch64(target_lexicon::Aarch64Architecture::Aarch64) => {
                Self::Aarch64
            }
            target_lexicon::Architecture::X86_64 => Self::X86_64,
            _ => anyhow::bail!("unsupported arch {arch}"),
        })
    }
}

impl TryFrom<FlowArch> for CommonArch {
    type Error = anyhow::Error;

    fn try_from(arch: FlowArch) -> anyhow::Result<Self> {
        Ok(match arch {
            FlowArch::X86_64 => Self::X86_64,
            FlowArch::Aarch64 => Self::Aarch64,
            arch => anyhow::bail!("unsupported arch {arch}"),
        })
    }
}

/// Vocabulary type for artifacts that only get built for the most common
/// actively-supported platforms in the hvlite tree.
#[derive(Serialize, Deserialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum CommonPlatform {
    WindowsMsvc,
    LinuxGnu,
    LinuxMusl,
    MacOs,
}

impl TryFrom<FlowPlatform> for CommonPlatform {
    type Error = anyhow::Error;

    fn try_from(platform: FlowPlatform) -> anyhow::Result<Self> {
        Ok(match platform {
            FlowPlatform::Windows => Self::WindowsMsvc,
            FlowPlatform::Linux(_) => Self::LinuxGnu,
            FlowPlatform::MacOs => Self::MacOs,
            platform => anyhow::bail!("unsupported platform {platform}"),
        })
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum CommonTriple {
    Common {
        arch: CommonArch,
        platform: CommonPlatform,
    },
    Custom(target_lexicon::Triple),
}

impl CommonTriple {
    pub const X86_64_WINDOWS_MSVC: Self = Self::Common {
        arch: CommonArch::X86_64,
        platform: CommonPlatform::WindowsMsvc,
    };
    pub const X86_64_LINUX_GNU: Self = Self::Common {
        arch: CommonArch::X86_64,
        platform: CommonPlatform::LinuxGnu,
    };
    pub const X86_64_LINUX_MUSL: Self = Self::Common {
        arch: CommonArch::X86_64,
        platform: CommonPlatform::LinuxMusl,
    };
    pub const AARCH64_WINDOWS_MSVC: Self = Self::Common {
        arch: CommonArch::Aarch64,
        platform: CommonPlatform::WindowsMsvc,
    };
    pub const AARCH64_LINUX_GNU: Self = Self::Common {
        arch: CommonArch::Aarch64,
        platform: CommonPlatform::LinuxGnu,
    };
    pub const AARCH64_LINUX_MUSL: Self = Self::Common {
        arch: CommonArch::Aarch64,
        platform: CommonPlatform::LinuxMusl,
    };
}

impl std::fmt::Debug for CommonTriple {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.as_triple(), f)
    }
}

impl std::fmt::Display for CommonTriple {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.as_triple(), f)
    }
}

impl PartialOrd for CommonTriple {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CommonTriple {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_triple()
            .to_string()
            .cmp(&other.as_triple().to_string())
    }
}

impl CommonTriple {
    pub fn as_triple(&self) -> target_lexicon::Triple {
        match self {
            CommonTriple::Common { arch, platform } => match platform {
                CommonPlatform::WindowsMsvc => target_lexicon::Triple {
                    architecture: arch.as_arch(),
                    vendor: target_lexicon::Vendor::Pc,
                    operating_system: target_lexicon::OperatingSystem::Windows,
                    environment: target_lexicon::Environment::Msvc,
                    binary_format: target_lexicon::BinaryFormat::Coff,
                },
                CommonPlatform::LinuxGnu => target_lexicon::Triple {
                    architecture: arch.as_arch(),
                    vendor: target_lexicon::Vendor::Unknown,
                    operating_system: target_lexicon::OperatingSystem::Linux,
                    environment: target_lexicon::Environment::Gnu,
                    binary_format: target_lexicon::BinaryFormat::Elf,
                },
                CommonPlatform::LinuxMusl => target_lexicon::Triple {
                    architecture: arch.as_arch(),
                    vendor: target_lexicon::Vendor::Unknown,
                    operating_system: target_lexicon::OperatingSystem::Linux,
                    environment: target_lexicon::Environment::Musl,
                    binary_format: target_lexicon::BinaryFormat::Elf,
                },
                CommonPlatform::MacOs => target_lexicon::Triple {
                    architecture: arch.as_arch(),
                    vendor: target_lexicon::Vendor::Apple,
                    operating_system: target_lexicon::OperatingSystem::Darwin(None),
                    environment: target_lexicon::Environment::Unknown,
                    binary_format: target_lexicon::BinaryFormat::Macho,
                },
            },
            CommonTriple::Custom(t) => t.clone(),
        }
    }

    /// Get the common architecture of this triple, failing if the triple's
    /// architecture is not one of the common architectures.
    pub fn common_arch(&self) -> anyhow::Result<CommonArch> {
        match self {
            CommonTriple::Common { arch, .. } => Ok(*arch),
            CommonTriple::Custom(target) => CommonArch::from_triple(target),
        }
    }
}
