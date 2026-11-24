// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration for generating IGVM files. These are deserialized from a TOML
//! manifest file used by the file builder.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use igvm_defs::PAGE_SIZE_4K;
use page_table::IdentityMapSize;
use page_table::x64::X64_PTE_ADDRESS_BIT_RANGE;
use product_policy::ManifestProductPolicy;
#[cfg(test)]
use product_policy::ProductPolicy;
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
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnpInjectionType {
    /// Normal injection.
    Normal,
    /// Restricted injection.
    Restricted,
}

/// Secure AVIC type.
#[derive(Serialize, Default, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecureAvicType {
    /// Offload AVIC to the hardware.
    Enabled,
    /// The paravisor emulates APIC.
    #[default]
    Disabled,
}

/// The isolation type that should be used for the loader. In a manifest, the
/// key of each `guest_configs` entry selects the type, and the entry's
/// `isolation` table holds its settings.
#[derive(Debug)]
pub enum ConfigIsolationType {
    /// No isolation is present.
    None,
    /// Hypervisor based isolation (VBS) is present.
    Vbs(VbsIsolation),
    /// AMD SEV-SNP.
    Snp(SnpIsolation),
    /// Intel TDX.
    Tdx(TdxIsolation),
}

/// VBS isolation settings.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct VbsIsolation {
    /// Boolean representing if the guest allows debugging
    pub enable_debug: bool,
}

/// AMD SEV-SNP isolation settings.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct SnpIsolation {
    /// The optional shared GPA boundary to configure for the guest. A
    /// `None` value represents a guest that no shared GPA boundary is to be
    /// configured.
    pub shared_gpa_boundary_bits: Option<u8>,
    /// The SEV-SNP policy for the guest.
    pub policy: u64,
    /// Boolean representing if the guest allows debugging
    pub enable_debug: bool,
    /// The interrupt injection type to use for the highest vmpl.
    pub injection_type: SnpInjectionType,
    /// Secure AVIC
    #[serde(default)]
    pub secure_avic: SecureAvicType,
}

/// Intel TDX isolation settings.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TdxIsolation {
    /// Boolean representing if the guest allows debugging
    pub enable_debug: bool,
    /// Boolean representing if the guest is disallowed from handling
    /// virtualization exceptions
    pub sept_ve_disable: bool,
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
        /// Optional measured product policy. When `Some`, the IGVM
        /// build appends the policy as a file in the measured OpenHCL initrd.
        /// The manifest schema is the wire schema; see
        /// [`product_policy`]. We don't gate this property behind a feature
        /// to avoid having multiple igvmfilegen tools with and without the feature.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_policy: Option<ProductPolicyConfig>,
    },
    /// Load the Linux kernel.
    /// TODO: Currently, this only works with underhill.
    Linux(LinuxImage),
    /// Load Linux directly into a self-contained SNP guest.
    SnpLinuxDirect {
        /// The Linux image to load.
        linux: LinuxImage,
        /// The number of 4-KiB RAM pages that the image measures. This is the
        /// minimum guest RAM. The host device tree describes the guest's CPUs
        /// and any RAM above this.
        memory_page_count: u64,
        /// The page-table address bit used as the SNP encryption bit.
        c_bit_position: u8,
        /// When the guest's RAM is accepted.
        memory_acceptance: SnpMemoryAcceptance,
    },
}

/// When the SNP Linux-direct bootshim accepts the guest's RAM.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SnpMemoryAcceptance {
    /// The bootshim accepts only the memory that it and the Linux EFI stub
    /// need. It reports all other RAM as unaccepted, and Linux accepts it when
    /// it first uses it. The kernel needs `CONFIG_UNACCEPTED_MEMORY`.
    Lazy,
    /// The bootshim accepts all RAM before it calls the Linux EFI stub.
    Eager,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub struct LinuxImage {
    /// Load with an initrd.
    pub use_initrd: bool,
    /// The command line to boot the kernel with.
    pub command_line: CString,
}

/// An OpenHCL image's product policy. The product's table may include a
/// `device_policy` that sets its PCI devices.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ProductPolicyConfig {
    /// The measured product policy and its optional device policy: the PCI
    /// devices that the VPCI relay may expose to VTL0, and when the relay
    /// runs. Without a device policy, the relay is off by default and VPCI
    /// devices reach VTL0 unfiltered; enabling it with
    /// `OPENHCL_ENABLE_VPCI_RELAY=1` fails the boot, because no device is
    /// allowed.
    pub policy: ManifestProductPolicy<DevicePolicyConfig>,
}

/// Device catalog and VPCI relay settings for an OpenHCL image.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DevicePolicyConfig {
    /// Device catalog path, relative to the manifest.
    pub devices: PathBuf,
    /// Names in the catalog that the relay may expose. `"*"` allows every device.
    pub allowed_pci_devices: Vec<String>,
    /// When the relay runs. Defaults to off when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vpci_relay: Option<product_policy::VpciRelayMode>,
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
            Image::SnpLinuxDirect { ref linux, .. } => linux
                .required_resources()
                .into_iter()
                .chain([ResourceType::SnpBootshim])
                .collect(),
        }
    }

    /// Validate constraints intrinsic to this image configuration.
    pub fn validate(&self) -> Result<(), ImageValidationError> {
        if let Image::SnpLinuxDirect {
            memory_page_count,
            c_bit_position,
            ..
        } = *self
        {
            if memory_page_count == 0 {
                return Err(ImageValidationError::ZeroMemoryPageCount);
            }

            let memory_byte_count = memory_page_count
                .checked_mul(PAGE_SIZE_4K)
                .ok_or(ImageValidationError::MemoryByteCountTooLarge { memory_page_count })?;
            if u32::try_from(memory_byte_count).is_err() {
                return Err(ImageValidationError::MemoryByteCountTooLarge { memory_page_count });
            }

            // The Linux startup page tables identity-map the lower 4 GiB and
            // set the C-bit by ORing it into each PTE. The bit must therefore
            // be outside that mapped address range as well as inside the
            // architectural PTE address field.
            let valid_c_bit = X64_PTE_ADDRESS_BIT_RANGE.contains(&c_bit_position)
                && (1u64 << c_bit_position) >= IdentityMapSize::Size4Gb.address_space_size();
            if !valid_c_bit {
                return Err(ImageValidationError::InvalidCBitPosition { c_bit_position });
            }
        }

        Ok(())
    }
}

/// Error returned when an image configuration contains invalid intrinsic fields.
#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum ImageValidationError {
    /// The image requests no guest memory.
    #[error("memory_page_count must be nonzero")]
    ZeroMemoryPageCount,
    /// The requested memory byte count cannot be represented by an IGVM
    /// required-memory directive.
    #[error("memory_page_count {memory_page_count} has a byte count that does not fit in u32")]
    MemoryByteCountTooLarge {
        /// The invalid number of 4-KiB pages.
        memory_page_count: u64,
    },
    /// The SNP C-bit overlaps the identity map or lies outside the x64 PTE
    /// address field.
    #[error(
        "c_bit_position {c_bit_position} overlaps the 4-GiB identity map or lies outside the x64 PTE address field"
    )]
    InvalidCBitPosition {
        /// The invalid C-bit position.
        c_bit_position: u8,
    },
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
#[derive(Debug)]
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
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GuestArch {
    /// x64
    X64,
    /// AArch64 aka ARM64
    Aarch64,
}

/// The config used to describe a multi-architecture IGVM file containing
/// multiple guests.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Config {
    /// The architecture of the igvm file.
    pub guest_arch: GuestArch,
    /// The guest configs to be used to generate a single IGVM file, in
    /// manifest order. The manifest keys each one by its isolation type:
    /// `none`, `vbs`, `snp`, or `tdx`.
    #[serde(deserialize_with = "parse::guest_configs")]
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
    Uefi,
    UnderhillKernel,
    OpenhclBoot,
    UnderhillInitrd,
    UnderhillSidecar,
    LinuxKernel,
    LinuxInitrd,
    SnpBootshim,
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
    use serde::de::Error as _;
    use serde::de::MapAccess;
    use std::collections::HashMap;

    #[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum IsolationKey {
        None,
        Vbs,
        Snp,
        Tdx,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct NonIsolatedGuest {
        guest_svn: u32,
        max_vtl: u8,
        image: Image,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct IsolatedGuest<I> {
        guest_svn: u32,
        max_vtl: u8,
        isolation: I,
        image: Image,
    }

    impl<I> IsolatedGuest<I> {
        fn into_config(self, isolation_type: impl FnOnce(I) -> ConfigIsolationType) -> GuestConfig {
            GuestConfig {
                guest_svn: self.guest_svn,
                max_vtl: self.max_vtl,
                isolation_type: isolation_type(self.isolation),
                image: self.image,
            }
        }
    }

    /// Reads the guest configs, keyed by isolation type, in manifest order.
    pub fn guest_configs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<GuestConfig>, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Vec<GuestConfig>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a table of guest configs keyed by `none`, `vbs`, `snp`, or `tdx`")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut keys = Vec::new();
                let mut configs = Vec::new();
                while let Some(key) = map.next_key::<IsolationKey>()? {
                    if keys.contains(&key) {
                        return Err(A::Error::custom(format_args!(
                            "duplicate guest config for {key:?}"
                        )));
                    }
                    keys.push(key);
                    configs.push(match key {
                        IsolationKey::None => {
                            let guest: NonIsolatedGuest = map.next_value()?;
                            GuestConfig {
                                guest_svn: guest.guest_svn,
                                max_vtl: guest.max_vtl,
                                isolation_type: ConfigIsolationType::None,
                                image: guest.image,
                            }
                        }
                        IsolationKey::Vbs => map
                            .next_value::<IsolatedGuest<VbsIsolation>>()?
                            .into_config(ConfigIsolationType::Vbs),
                        IsolationKey::Snp => map
                            .next_value::<IsolatedGuest<SnpIsolation>>()?
                            .into_config(ConfigIsolationType::Snp),
                        IsolationKey::Tdx => map
                            .next_value::<IsolatedGuest<TdxIsolation>>()?
                            .into_config(ConfigIsolationType::Tdx),
                    });
                }
                Ok(configs)
            }
        }

        d.deserialize_map(Visitor)
    }

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
    use test_with_tracing::test;

    fn snp_linux_direct_image(
        use_initrd: bool,
        memory_page_count: u64,
        c_bit_position: u8,
    ) -> Image {
        Image::SnpLinuxDirect {
            linux: LinuxImage {
                use_initrd,
                command_line: CString::new("console=ttyS0").unwrap(),
            },
            memory_page_count,
            c_bit_position,
            memory_acceptance: SnpMemoryAcceptance::Lazy,
        }
    }

    #[test]
    fn parse_snp_linux_direct_manifest() {
        let config: Config =
            toml_edit::de::from_str(include_str!("../../manifests/snp-linux-direct.toml")).unwrap();

        assert!(matches!(config.guest_arch, GuestArch::X64));
        let [guest] = config.guest_configs.as_slice() else {
            panic!("expected one guest config");
        };
        assert_eq!(guest.guest_svn, 1);
        assert_eq!(guest.max_vtl, 0);
        match &guest.isolation_type {
            ConfigIsolationType::Snp(SnpIsolation {
                shared_gpa_boundary_bits,
                policy,
                enable_debug,
                injection_type,
                secure_avic,
            }) => {
                assert_eq!(*shared_gpa_boundary_bits, None);
                assert_eq!(*policy, 196608);
                assert!(*enable_debug);
                assert!(matches!(injection_type, SnpInjectionType::Normal));
                assert!(matches!(secure_avic, SecureAvicType::Disabled));
            }

            isolation_type => panic!("unexpected isolation type: {isolation_type:?}"),
        }
        match &guest.image {
            Image::SnpLinuxDirect {
                linux,
                memory_page_count,
                c_bit_position,
                memory_acceptance,
            } => {
                assert!(linux.use_initrd);
                assert_eq!(*memory_acceptance, SnpMemoryAcceptance::Lazy);
                assert_eq!(
                    linux.command_line.as_bytes(),
                    b"console=ttyS0 earlyprintk=serial earlycon panic=-1"
                );
                assert_eq!(*memory_page_count, 40960);
                assert_eq!(*c_bit_position, 51);
                guest.image.validate().unwrap();
            }
            image => panic!("unexpected image: {image:?}"),
        }
    }

    #[test]
    fn parse_restricted_snp_linux_direct_manifest() {
        let config: Config = toml_edit::de::from_str(include_str!(
            "../../manifests/snp-linux-direct-restricted.toml"
        ))
        .unwrap();
        let [guest] = config.guest_configs.as_slice() else {
            panic!("expected one guest config");
        };
        assert!(matches!(
            guest.isolation_type,
            ConfigIsolationType::Snp(SnpIsolation {
                injection_type: SnpInjectionType::Restricted,
                ..
            })
        ));
        guest.image.validate().unwrap();
    }

    #[test]
    fn snp_linux_direct_required_resources_with_initrd() {
        let image = snp_linux_direct_image(true, 40960, 51);

        assert_eq!(
            image.required_resources(),
            vec![
                ResourceType::LinuxKernel,
                ResourceType::LinuxInitrd,
                ResourceType::SnpBootshim,
            ]
        );
    }

    #[test]
    fn snp_linux_direct_required_resources_without_initrd() {
        let image = snp_linux_direct_image(false, 40960, 51);

        assert_eq!(
            image.required_resources(),
            vec![ResourceType::LinuxKernel, ResourceType::SnpBootshim]
        );
    }

    #[test]
    fn snp_linux_direct_requires_bootshim_resource() {
        let image = snp_linux_direct_image(false, 40960, 51);
        let resources = Resources {
            resources: [(ResourceType::LinuxKernel, PathBuf::from("/kernel"))]
                .into_iter()
                .collect(),
        };

        assert_eq!(
            resources
                .check_required(&image.required_resources())
                .unwrap_err()
                .0,
            [ResourceType::SnpBootshim]
        );
    }

    #[test]
    fn snp_linux_direct_rejects_zero_memory() {
        let image = snp_linux_direct_image(false, 0, 51);

        assert_eq!(
            image.validate(),
            Err(ImageValidationError::ZeroMemoryPageCount)
        );
    }

    #[test]
    fn snp_linux_direct_rejects_oversized_memory() {
        let memory_page_count = u64::from(u32::MAX) / PAGE_SIZE_4K + 1;
        let image = snp_linux_direct_image(false, memory_page_count, 51);

        assert_eq!(
            image.validate(),
            Err(ImageValidationError::MemoryByteCountTooLarge { memory_page_count })
        );
    }

    #[test]
    fn snp_linux_direct_rejects_invalid_c_bit_positions() {
        for c_bit_position in [11, 31, 52] {
            let image = snp_linux_direct_image(false, 40960, c_bit_position);

            assert_eq!(
                image.validate(),
                Err(ImageValidationError::InvalidCBitPosition { c_bit_position })
            );
        }

        snp_linux_direct_image(false, 40960, 32).validate().unwrap();
    }

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

    /// A manifest without a product policy builds an image with no policy
    /// file in the initrd. Reading such a manifest must not invent a policy,
    /// and writing the image config back out must leave the field out rather
    /// than emit an empty one.
    #[test]
    fn openhcl_image_without_product_policy_round_trips() {
        let toml = r#"
            [openhcl]
            command_line = ""
            memory_page_count = 10
            uefi = true
        "#;
        let parsed: Image = toml_edit::de::from_str(toml).unwrap();
        match &parsed {
            Image::Openhcl { product_policy, .. } => assert!(product_policy.is_none()),
            other => panic!("unexpected parse: {other:?}"),
        }
        let reserialized = toml_edit::ser::to_string(&parsed).unwrap();
        assert!(
            !reserialized.contains("product_policy"),
            "policy field should be omitted when None: {reserialized}"
        );
    }

    /// The SIVM product policy and its device policy are measured and
    /// enforced by OpenHCL. Every rule written in the manifest, including the
    /// base64 custom UEFI data and the relay mode, must reach the parsed
    /// config unchanged; a lost or misread rule would build an image that
    /// enforces less than the manifest says, with no build error.
    #[test]
    fn openhcl_image_with_sivm_product_policy_deserializes() {
        let toml = r#"
            [openhcl]
            command_line = ""
            memory_page_count = 10
            uefi = true

            [openhcl.product_policy.policy.sivm]
            require_ephemeral_vmgs = true
            require_secure_boot = true
            require_secure_boot_vars = true
            require_bcd_integrity = true
            custom_uefi_json = "ZGVhZGJlZWY="

            [openhcl.product_policy.policy.sivm.device_policy]
            devices = "policies/devices.toml"
            allowed_pci_devices = ["nvidia-gpu"]
            vpci_relay = "must_be_on"
        "#;
        let parsed: Image = toml_edit::de::from_str(toml).unwrap();
        match parsed {
            Image::Openhcl {
                product_policy: Some(config),
                ..
            } => {
                match config.policy.policy() {
                    ProductPolicy::Sivm(p) => {
                        assert!(p.require_ephemeral_vmgs);
                        assert!(p.require_secure_boot);
                        assert!(p.require_secure_boot_vars);
                        assert!(p.require_bcd_integrity);
                        assert_eq!(p.custom_uefi_json, b"deadbeef");
                    }
                    ProductPolicy::Cwcow(p) => panic!("unexpected Cwcow policy: {p:?}"),
                    ProductPolicy::Azure(p) => panic!("unexpected Azure policy: {p:?}"),
                }
                let device_policy = config.policy.device_policy().unwrap();
                assert_eq!(device_policy.allowed_pci_devices, ["nvidia-gpu"]);
                assert_eq!(
                    device_policy.vpci_relay,
                    Some(product_policy::VpciRelayMode::MustBeOn)
                );
            }
            other => panic!("unexpected parse: {other:?}"),
        }
    }

    /// A device policy in the wrong place, or a misspelled field, must be an
    /// error. Otherwise it would be silently ignored, and the image would be
    /// built without the device restrictions its author intended.
    #[test]
    fn product_policy_rejects_misplaced_or_unknown_fields() {
        let device_policy = r#"
            devices = "policies/devices.toml"
            allowed_pci_devices = ["nvme"]
        "#;
        for (toml, unknown) in [
            // The old location, beside `policy`, is no longer accepted.
            (
                format!("policy = {{ azure = {{}} }}\n[device_policy]\n{device_policy}"),
                "device_policy",
            ),
            (
                format!("[policy.azure]\nextra = 1\n[policy.azure.device_policy]\n{device_policy}"),
                "extra",
            ),
            // Only the device policy may set the relay settings.
            (
                "[policy.azure.vpci_relay]\nmode = \"must_be_on\"".to_owned(),
                "vpci_relay",
            ),
        ] {
            let error = toml_edit::de::from_str::<ProductPolicyConfig>(&toml).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("unknown field `{unknown}`")),
                "{toml}: {error}"
            );
        }
    }

    /// Every in-tree manifest, keyed by file stem. The rule tests below load
    /// the real manifests, so they check exactly what the build uses.
    fn in_tree_manifests() -> std::collections::BTreeMap<String, Config> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../manifests");
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .map(|path| {
                let config = toml_edit::de::from_str(&std::fs::read_to_string(&path).unwrap())
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                (
                    path.file_stem().unwrap().to_str().unwrap().to_owned(),
                    config,
                )
            })
            .collect()
    }

    /// Every in-tree manifest must load and pass image validation. A broken
    /// manifest otherwise fails only when an image is built from it, which
    /// may be much later, in a CI or release build. The count catches a
    /// manifest that was deleted or renamed by mistake.
    #[test]
    fn in_tree_manifests_parse() {
        let manifests = in_tree_manifests();
        for (name, config) in &manifests {
            assert!(!config.guest_configs.is_empty(), "{name}");
            for guest in &config.guest_configs {
                guest.image.validate().unwrap();
            }
        }
        assert_eq!(manifests.len(), 16);
    }

    fn isolation_names(config: &Config) -> Vec<&'static str> {
        config
            .guest_configs
            .iter()
            .map(|guest| match guest.isolation_type {
                ConfigIsolationType::None => "none",
                ConfigIsolationType::Vbs(_) => "vbs",
                ConfigIsolationType::Snp(_) => "snp",
                ConfigIsolationType::Tdx(_) => "tdx",
            })
            .collect()
    }

    /// Guest configs are built in the order the manifest lists them, and that
    /// order affects the IGVM file's layout. If the parser reordered them,
    /// for example alphabetically, images such as `uefi-x64` (TDX before SNP)
    /// would change silently. This checks a manifest whose order isn't
    /// alphabetical.
    #[test]
    fn guest_configs_keep_manifest_order() {
        let uefi: Config =
            toml_edit::de::from_str(include_str!("../../manifests/uefi-x64.toml")).unwrap();
        assert_eq!(isolation_names(&uefi), ["tdx", "snp"]);
        let cvm: Config =
            toml_edit::de::from_str(include_str!("../../manifests/openhcl-x64-cvm-release.toml"))
                .unwrap();
        assert_eq!(isolation_names(&cvm), ["snp", "tdx", "vbs"]);
    }

    /// Isolation settings decide how a confidential guest is protected and
    /// measured. A mistake in them must be an error, not silently ignored or
    /// filled in: an isolation table on a non-isolated guest, a missing one on
    /// an isolated guest, a misspelled setting, an unknown platform, or a
    /// manifest still in the old `[[guest_configs]]` layout.
    #[test]
    fn guest_configs_reject_invalid_entries() {
        let image = "[guest_configs.{key}.image.uefi]\nconfig_type = \"none\"";
        let guest = |key: &str, extra: &str| {
            format!(
                "guest_arch = \"x64\"\n[guest_configs.{key}]\nguest_svn = 1\nmax_vtl = 0\n{extra}\n{}",
                image.replace("{key}", key)
            )
        };
        for (toml, message) in [
            // The isolation table belongs only to isolated platforms.
            (
                guest("none", "isolation = { enable_debug = false }"),
                "unknown field `isolation`",
            ),
            (guest("vbs", ""), "missing field `isolation`"),
            (
                guest("vbs", "isolation = { enable_debug = false, extra = 1 }"),
                "unknown field `extra`",
            ),
            (
                guest("tdx", "isolation_type = \"none\""),
                "unknown field `isolation_type`",
            ),
            (guest("sev", ""), "unknown variant `sev`"),
            // The previous array layout is not accepted.
            (
                "guest_arch = \"x64\"\n[[guest_configs]]\nguest_svn = 1".to_owned(),
                "invalid type",
            ),
        ] {
            let error = toml_edit::de::from_str::<Config>(&toml).unwrap_err();
            assert!(error.to_string().contains(message), "{toml}\n{error}");
        }
    }

    /// Whether the loader will measure a debuggable SNP policy. The loader
    /// replaces the policy's debug bit with `enable_debug`.
    fn snp_debug(snp: &SnpIsolation) -> bool {
        igvm_defs::SnpPolicy::from(snp.policy)
            .with_debug(snp.enable_debug as u8)
            .debug()
            == 1
    }

    /// Isolation settings other than debugging.
    #[derive(Debug, PartialEq)]
    enum IsolationWithoutDebug<'a> {
        None,
        Vbs,
        Snp {
            shared_gpa_boundary_bits: Option<u8>,
            policy: u64,
            injection_type: &'a SnpInjectionType,
            secure_avic: &'a SecureAvicType,
        },
        Tdx {
            sept_ve_disable: bool,
        },
    }

    fn isolation_without_debug(isolation: &ConfigIsolationType) -> IsolationWithoutDebug<'_> {
        match isolation {
            ConfigIsolationType::None => IsolationWithoutDebug::None,
            ConfigIsolationType::Vbs(_) => IsolationWithoutDebug::Vbs,
            ConfigIsolationType::Snp(snp) => IsolationWithoutDebug::Snp {
                shared_gpa_boundary_bits: snp.shared_gpa_boundary_bits,
                policy: igvm_defs::SnpPolicy::from(snp.policy).with_debug(0).into(),
                injection_type: &snp.injection_type,
                secure_avic: &snp.secure_avic,
            },
            ConfigIsolationType::Tdx(tdx) => IsolationWithoutDebug::Tdx {
                sept_ve_disable: tdx.sept_ve_disable,
            },
        }
    }

    /// What an image loads, without sizes or command lines.
    fn image_kind(image: &Image) -> String {
        match image {
            Image::None => "none".into(),
            Image::Uefi { config_type } => format!("uefi {config_type:?}"),
            Image::Openhcl { uefi, linux, .. } => {
                format!("openhcl uefi={uefi} linux={}", linux.is_some())
            }
            Image::Linux(_) => "linux".into(),
            Image::SnpLinuxDirect { .. } => "snp_linux_direct".into(),
        }
    }

    /// An OpenHCL image's product policy and device policy.
    fn openhcl_policies(image: &Image) -> Option<(ProductPolicy, Option<&DevicePolicyConfig>)> {
        match image {
            Image::Openhcl {
                product_policy: Some(config),
                ..
            } => Some((config.policy.policy(), config.policy.device_policy())),
            _ => None,
        }
    }

    /// A debuggable confidential image lets the host inspect or change the
    /// guest, so shipping one would undo its isolation. Release images must
    /// never be debuggable: the measured VBS, SNP, and TDX policies must have
    /// debugging off, and OpenHCL's measured command line must not enable
    /// confidential debugging. This catches a debug setting copied from a dev
    /// manifest into a release one.
    #[test]
    fn release_manifests_are_not_debuggable() {
        let mut checked = 0;
        for (name, config) in in_tree_manifests() {
            if !name.ends_with("-release") {
                continue;
            }
            for guest in &config.guest_configs {
                let debug = match &guest.isolation_type {
                    ConfigIsolationType::None => false,
                    ConfigIsolationType::Vbs(vbs) => vbs.enable_debug,
                    ConfigIsolationType::Snp(snp) => snp_debug(snp),
                    ConfigIsolationType::Tdx(tdx) => tdx.enable_debug,
                };
                assert!(!debug, "{name}: {:?} is debuggable", guest.isolation_type);
                if let Image::Openhcl { command_line, .. } = &guest.image {
                    assert!(
                        !command_line.contains("OPENHCL_CONFIDENTIAL_DEBUG"),
                        "{name}: {command_line}"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    /// Developers test with the dev manifest, but the release manifest is what
    /// ships. If a change is made to only one of the pair, such as a new
    /// allowed device or a different SNP or TDX setting, the shipped image
    /// would differ from the tested one without anyone noticing. Each pair
    /// must build the same image: only debugging, SVNs, command lines, and
    /// memory sizes may differ.
    #[test]
    fn dev_and_release_manifests_match() {
        let manifests = in_tree_manifests();
        let mut pairs = 0;
        for (name, dev) in &manifests {
            if let Some(base) = name.strip_suffix("-release") {
                assert!(
                    manifests.contains_key(&format!("{base}-dev")),
                    "{name} has no dev manifest"
                );
            }
            let Some(base) = name.strip_suffix("-dev") else {
                continue;
            };
            let release = manifests
                .get(&format!("{base}-release"))
                .unwrap_or_else(|| panic!("{name} has no release manifest"));
            assert_eq!(dev.guest_arch, release.guest_arch, "{base}");
            assert_eq!(isolation_names(dev), isolation_names(release), "{base}");
            for (dev, release) in dev.guest_configs.iter().zip(&release.guest_configs) {
                assert_eq!(dev.max_vtl, release.max_vtl, "{base}");
                assert_eq!(
                    isolation_without_debug(&dev.isolation_type),
                    isolation_without_debug(&release.isolation_type),
                    "{base}"
                );
                assert_eq!(image_kind(&dev.image), image_kind(&release.image), "{base}");
                assert_eq!(
                    openhcl_policies(&dev.image),
                    openhcl_policies(&release.image),
                    "{base}"
                );
            }
            pairs += 1;
        }
        assert!(pairs > 0);
    }

    /// The SNP, TDX, and VBS entries in one manifest describe the same image
    /// on different hardware, and each repeats the shared settings. An edit
    /// applied to only one platform, such as a bumped SVN or a removed device,
    /// would make that platform behave or attest differently from the others.
    /// They must share their SVN, what they load, and their product and
    /// device policies.
    #[test]
    fn platforms_in_a_manifest_match() {
        let mut checked = 0;
        for (name, config) in in_tree_manifests() {
            let [first, rest @ ..] = config.guest_configs.as_slice() else {
                panic!("{name} has no guest configs");
            };
            for guest in rest {
                assert_eq!(guest.guest_svn, first.guest_svn, "{name}");
                assert_eq!(image_kind(&guest.image), image_kind(&first.image), "{name}");
                assert_eq!(
                    openhcl_policies(&guest.image),
                    openhcl_policies(&first.image),
                    "{name}"
                );
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    /// A device policy that leaves out `vpci_relay` must get the documented
    /// default, `default_off`, rather than some other mode. A misspelled
    /// setting, such as `vpci_realy` for `vpci_relay`, must be an error: if
    /// it were ignored, a policy meant to require the relay would silently
    /// fall back to the default.
    #[test]
    fn inline_device_policy_defaults_and_rejects_unknown_fields() {
        let toml = r#"
            devices = "policies/devices.toml"
            allowed_pci_devices = ["nvme"]
        "#;
        let policy: DevicePolicyConfig = toml_edit::de::from_str(toml).unwrap();
        assert_eq!(policy.devices, PathBuf::from("policies/devices.toml"));
        assert_eq!(policy.allowed_pci_devices, ["nvme"]);
        assert_eq!(policy.vpci_relay, None);
        let error = toml_edit::de::from_str::<DevicePolicyConfig>(&format!(
            "{toml}\nvpci_realy = \"must_be_on\""
        ))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }
}
