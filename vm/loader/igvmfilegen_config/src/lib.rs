// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration for generating IGVM files. These are deserialized from a JSON
//! manifest file used by the file builder.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::ffi::CString;
use std::path::PathBuf;

/// The UEFI config type to pass to the UEFI loader.
#[derive(Serialize, Deserialize, Debug, Copy, Clone)]
#[serde(rename_all = "snake_case")]
pub enum UefiConfigType {
    /// No UEFI config set at load time.
    None,
    /// UEFI config is specified via IGVM parameters.
    Igvm,
}

/// The interrupt injection type that should be used for VMPL0 on SNP.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SnpInjectionType {
    /// Normal injection.
    Normal,
    /// Restricted injection.
    Restricted,
}

/// The isolation type that should be used for the loader.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ConfigIsolationType {
    /// No isolation is present.
    None,
    /// Hypervisor based isolation (VBS) is present.
    Vbs {
        /// Boolean representing if the guest allows debugging
        enable_debug: bool,
    },
    /// AMD SEV-SNP.
    Snp {
        /// The optional shared GPA boundary to configure for the guest. A
        /// `None` value represents a guest that no shared GPA boundary is to be
        /// configured.
        shared_gpa_boundary_bits: Option<u8>,
        /// The SEV-SNP policy for the guest.
        policy: u64,
        /// Boolean representing if the guest allows debugging
        enable_debug: bool,
        /// The interrupt injection type to use for the highest vmpl.
        injection_type: SnpInjectionType,
    },
    /// Intel TDX.
    Tdx {
        /// Boolean representing if the guest allows debugging
        enable_debug: bool,
        /// Boolean representing if the guest is disallowed from handling
        /// virtualization exceptions
        sept_ve_disable: bool,
    },
}

/// Configuration on what to load.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Image {
    /// Load nothing.
    None,
    /// Load UEFI.
    Uefi { config_type: UefiConfigType },
    /// Load the OpenHCL paravisor.
    Openhcl {
        /// The paravisor kernel command line.
        #[serde(default)]
        command_line: String,
        /// If false, the host may provide additional kernel command line
        /// parameters at runtime.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        static_command_line: bool,
        /// The base page number for paravisor memory. None means relocation is used.
        #[serde(skip_serializing_if = "Option::is_none")]
        memory_page_base: Option<u64>,
        /// The number of pages for paravisor memory.
        memory_page_count: u64,
        /// Include the UEFI firmware for loading into the guest.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        uefi: bool,
        /// Include the Linux kernel for loading into the guest.
        #[serde(skip_serializing_if = "Option::is_none")]
        linux: Option<LinuxImage>,
        static_elf: Option<StaticElfImage>,
    },
    /// Load the Linux kernel.
    /// TODO: Currently, this only works with underhill.
    Linux(LinuxImage),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct LinuxImage {
    /// Load with an initrd.
    pub use_initrd: bool,
    /// The command line to boot the kernel with.
    pub command_line: CString,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct StaticElfImage {
    /// Load address.
    pub start_address: usize,
    /// Load offset.
    pub load_offset: usize,
    /// Whether the code is position independent or not.
    pub assume_pic: bool,
}

impl Image {
    /// Get the required resources for this image config.
    pub fn required_resources(&self) -> Vec<ResourceType> {
        match *self {
            Image::None => vec![],
            Image::Uefi { .. } => vec![ResourceType::Uefi],
            Image::Openhcl {
                uefi, ref linux, ..
            } => [
                ResourceType::UnderhillKernel,
                ResourceType::OpenhclBoot,
                ResourceType::UnderhillInitrd,
            ]
            .into_iter()
            .chain(if uefi { Some(ResourceType::Uefi) } else { None })
            .chain(linux.iter().flat_map(|linux| linux.required_resources()))
            .collect(),
            Image::Linux(ref linux) => linux.required_resources(),
        }
    }
}

impl LinuxImage {
    fn required_resources(&self) -> Vec<ResourceType> {
        [ResourceType::LinuxKernel]
            .into_iter()
            .chain(if self.use_initrd {
                Some(ResourceType::LinuxInitrd)
            } else {
                None
            })
            .collect()
    }
}

/// The config used to describe an initial guest context to be generated by the
/// tool.
#[derive(Serialize, Deserialize, Debug)]
pub struct GuestConfig {
    /// The SVN of this guest.
    pub guest_svn: u32,
    /// The maximum VTL to be enabled for the guest.
    pub max_vtl: u8,
    /// The isolation type to be used for the guest.
    pub isolation_type: ConfigIsolationType,
    /// The image to load into the guest.
    pub image: Image,
}

/// The architecture of the igvm file.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum GuestArch {
    /// x64
    X64,
    /// AArch64 aka ARM64
    Aarch64,
}

/// The config used to describe a multi-architecture IGVM file containing
/// multiple guests.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct Config {
    /// The architecture of the igvm file.
    pub guest_arch: GuestArch,
    /// The array of guest configs to be used to generate a single IGVM file.
    pub guest_configs: Vec<GuestConfig>,
}

impl Config {
    /// Get a vec representing the required resources for this config.
    pub fn required_resources(&self) -> Vec<ResourceType> {
        let mut resources = vec![];
        for guest_config in &self.guest_configs {
            resources.extend(guest_config.image.required_resources());
        }
        resources
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    StaticElf,
    Uefi,
    UnderhillKernel,
    OpenhclBoot,
    UnderhillInitrd,
    UnderhillSidecar,
    LinuxKernel,
    LinuxInitrd,
}

/// Resources used by igvmfilegen to generate IGVM files. These are generated by
/// build tooling and not checked into the repo.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct Resources {
    /// The set of resources to use to generate IGVM files. These paths must be
    /// absolute.
    #[serde(deserialize_with = "parse::resources")]
    resources: HashMap<ResourceType, PathBuf>,
}

mod parse {
    use super::*;
    use serde::Deserialize;
    use serde::Deserializer;
    use std::collections::HashMap;

    pub fn resources<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<HashMap<ResourceType, PathBuf>, D::Error> {
        let resources: HashMap<ResourceType, PathBuf> = Deserialize::deserialize(d)?;

        for (resource, path) in &resources {
            if !path.is_absolute() {
                return Err(serde::de::Error::custom(AbsolutePathError(
                    *resource,
                    path.clone(),
                )));
            }
        }

        Ok(resources)
    }
}

/// Error returned when required resources are missing.
#[derive(Debug)]
pub struct MissingResourcesError(pub Vec<ResourceType>);

impl std::fmt::Display for MissingResourcesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "missing resources: {:?}", self.0)
    }
}

impl std::error::Error for MissingResourcesError {}

/// Error returned when a resource is not an absolute path.
#[derive(Debug)]
pub struct AbsolutePathError(ResourceType, PathBuf);

impl std::fmt::Display for AbsolutePathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "resource {:?} path is not absolute: {:?}",
            self.0, self.1
        )
    }
}

impl std::error::Error for AbsolutePathError {}

impl Resources {
    /// Create a new set of resources. Returns an error if any of the paths are
    /// not absolute.
    pub fn new(resources: HashMap<ResourceType, PathBuf>) -> Result<Self, AbsolutePathError> {
        for (resource, path) in &resources {
            if !path.is_absolute() {
                return Err(AbsolutePathError(*resource, path.clone()));
            }
        }

        Ok(Resources { resources })
    }

    /// Get the resources for this set.
    pub fn resources(&self) -> &HashMap<ResourceType, PathBuf> {
        &self.resources
    }

    /// Get the resource path for a given resource type.
    pub fn get(&self, resource: ResourceType) -> Option<&PathBuf> {
        self.resources.get(&resource)
    }

    /// Check that the required resources are present. On error, returns which
    /// resources are missing.
    pub fn check_required(&self, required: &[ResourceType]) -> Result<(), MissingResourcesError> {
        let mut missing = vec![];
        for resource in required {
            if !self.resources.contains_key(resource) {
                missing.push(*resource);
            }
        }

        if missing.is_empty() {
            Ok(())
        } else {
            Err(MissingResourcesError(missing))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn non_absolute_path_new() {
        let mut resources = HashMap::new();
        resources.insert(ResourceType::Uefi, PathBuf::from("./uefi"));
        let result = Resources::new(resources);
        assert!(result.is_err());
    }

    #[test]
    fn parse_non_absolute_path() {
        let resources = r#"{"uefi":"./uefi"}"#;
        let result: Result<Resources, _> = serde_json::from_str(resources);
        assert!(result.is_err());
    }

    #[test]
    fn missing_resources() {
        let resources = Resources {
            resources: HashMap::new(),
        };
        let required = vec![ResourceType::Uefi];
        let result = resources.check_required(&required);
        assert!(result.is_err());
    }
}
