// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use petri_artifacts_common::tags::IsTestIso;
use petri_artifacts_common::tags::IsTestVhd;
use petri_artifacts_common::tags::MachineArch;
use proc_macro2::Ident;
use proc_macro2::Span;
use proc_macro2::TokenStream;
use quote::ToTokens;
use quote::quote;
use syn::Error;
use syn::ItemFn;
use syn::Path;
use syn::Token;
use syn::parse::Parse;
use syn::parse::ParseStream;
use syn::parse_macro_input;
use syn::spanned::Spanned;

struct Config {
    vmm: Option<Vmm>,
    firmware: Firmware,
    arch: MachineArch,
    span: Span,
    extra_deps: Vec<Path>,
    unstable: bool,
}

struct ResolvedConfig {
    vmm: Vmm,
    firmware: Firmware,
    arch: MachineArch,
    extra_deps: Vec<Path>,
    unstable: bool,
    requires_vpci: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Vmm {
    OpenVmm,
    HyperV,
}

enum Firmware {
    LinuxDirect,
    Pcat(PcatGuest),
    Uefi(UefiGuest),
    OpenhclLinuxDirect,
    OpenhclPcat(PcatGuest),
    OpenhclUefi(OpenhclUefiOptions, UefiGuest),
}

#[derive(Default)]
struct OpenhclUefiOptions {
    isolation: Option<IsolationType>,
}

enum IsolationType {
    Vbs,
    Snp,
    Tdx,
}

enum PcatGuest {
    Vhd(ImageInfo),
    Iso(ImageInfo),
}

enum UefiGuest {
    Vhd(ImageInfo),
    GuestTestUefi(MachineArch),
    None,
}

struct ImageInfo {
    image_artifact: TokenStream,
    arch: MachineArch,
    name_prefix: String,
}

struct Args {
    configs: Vec<Config>,
}

struct ArgsWithOverrides {
    args: Args,
    vmm: Option<Vmm>,
    unstable: bool,
    with_vtl0_pipette: bool,
    requires_vpci: bool,
}

struct ResolvedArgs {
    configs: Vec<ResolvedConfig>,
    with_vtl0_pipette: bool,
}

fn arch_to_str(arch: MachineArch) -> &'static str {
    match arch {
        MachineArch::X86_64 => "x64",
        MachineArch::Aarch64 => "aarch64",
    }
}

fn arch_to_tokens(arch: MachineArch) -> TokenStream {
    match arch {
        MachineArch::X86_64 => quote!(::petri_artifacts_common::tags::MachineArch::X86_64),
        MachineArch::Aarch64 => quote!(::petri_artifacts_common::tags::MachineArch::Aarch64),
    }
}

impl ResolvedConfig {
    fn name_prefix(&self) -> String {
        let arch_prefix = arch_to_str(self.arch);

        let vmm_prefix = match self.vmm {
            Vmm::OpenVmm => "openvmm",
            Vmm::HyperV => "hyperv",
        };

        let firmware_prefix = match &self.firmware {
            Firmware::LinuxDirect => "linux",
            Firmware::Pcat(_) => "pcat",
            Firmware::Uefi(_) => "uefi",
            Firmware::OpenhclLinuxDirect => "openhcl_linux",
            Firmware::OpenhclPcat(..) => "openhcl_pcat",
            Firmware::OpenhclUefi(..) => "openhcl_uefi",
        };

        let guest_prefix = match &self.firmware {
            Firmware::LinuxDirect | Firmware::OpenhclLinuxDirect => None,
            Firmware::Pcat(guest) | Firmware::OpenhclPcat(guest) => Some(guest.name_prefix()),
            Firmware::Uefi(guest) | Firmware::OpenhclUefi(_, guest) => guest.name_prefix(),
        };

        let options_prefix = match &self.firmware {
            Firmware::LinuxDirect
            | Firmware::Pcat(_)
            | Firmware::Uefi(_)
            | Firmware::OpenhclLinuxDirect
            | Firmware::OpenhclPcat(_) => None,
            Firmware::OpenhclUefi(opt, _) => opt.name_prefix(),
        };

        let mut name_prefix = format!("{}_{}_{}", vmm_prefix, firmware_prefix, arch_prefix);
        if let Some(guest_prefix) = guest_prefix {
            name_prefix.push('_');
            name_prefix.push_str(&guest_prefix);
        }
        if let Some(options_prefix) = options_prefix {
            name_prefix.push('_');
            name_prefix.push_str(&options_prefix);
        }

        name_prefix
    }
}

impl PcatGuest {
    fn name_prefix(&self) -> String {
        match self {
            PcatGuest::Vhd(vhd) => vhd.name_prefix.clone(),
            PcatGuest::Iso(iso) => iso.name_prefix.clone(),
        }
    }
}

impl ToTokens for PcatGuest {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        tokens.extend(match self {
            PcatGuest::Vhd(known_vhd) => {
                let vhd = known_vhd.image_artifact.clone();
                quote!(::petri::PcatGuest::Vhd(petri::BootImageConfig::from_vhd(resolver.require_source(#vhd, ::petri::RemoteAccess::Allow))))
            }
            PcatGuest::Iso(known_iso) => {
                let iso = known_iso.image_artifact.clone();
                quote!(::petri::PcatGuest::Iso(petri::BootImageConfig::from_iso(resolver.require_source(#iso, ::petri::RemoteAccess::Allow))))
            }
        });
    }
}

impl UefiGuest {
    fn name_prefix(&self) -> Option<String> {
        match self {
            UefiGuest::Vhd(known_vhd) => Some(known_vhd.name_prefix.clone()),
            UefiGuest::GuestTestUefi(arch) => Some(format!("guest_test_{}", arch_to_str(*arch))),
            UefiGuest::None => None,
        }
    }
}

impl ToTokens for UefiGuest {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        tokens.extend(match self {
            UefiGuest::Vhd(known_vhd) => {
                let v = known_vhd.image_artifact.clone();
                quote!(::petri::UefiGuest::Vhd(petri::BootImageConfig::from_vhd(resolver.require_source(#v, ::petri::RemoteAccess::Allow))))
            }
            UefiGuest::GuestTestUefi(arch) => {
                let arch_tokens = arch_to_tokens(*arch);
                quote!(::petri::UefiGuest::guest_test_uefi(resolver, #arch_tokens))
            }
            UefiGuest::None => quote!(::petri::UefiGuest::None),
        });
    }
}

struct FirmwareAndArch {
    firmware: Firmware,
    arch: MachineArch,
}

impl ToTokens for FirmwareAndArch {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let arch = arch_to_tokens(self.arch);
        tokens.extend(match &self.firmware {
            Firmware::LinuxDirect => {
                quote!(::petri::Firmware::linux_direct(resolver, #arch))
            }
            Firmware::Pcat(guest) => {
                quote!(::petri::Firmware::pcat(resolver, #guest))
            }
            Firmware::Uefi(guest) => {
                quote!(::petri::Firmware::uefi(resolver, #arch, #guest))
            }
            Firmware::OpenhclLinuxDirect => {
                quote!(::petri::Firmware::openhcl_linux_direct(resolver, #arch))
            }
            Firmware::OpenhclPcat(guest) => {
                quote!(::petri::Firmware::openhcl_pcat(resolver, #guest))
            }
            Firmware::OpenhclUefi(OpenhclUefiOptions { isolation }, guest) => {
                let isolation = match isolation {
                    Some(i) => quote!(Some(#i)),
                    None => quote!(None),
                };
                quote!(::petri::Firmware::openhcl_uefi(resolver, #arch, #guest, #isolation))
            }
        })
    }
}

impl Parse for ArgsWithOverrides {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut unstable = None;
        let mut with_vtl0_pipette = None;
        let mut vmm = None;
        let mut requires_vpci = None;

        let word = input.parse::<Ident>()?;
        let conflict_err = || Err::<Self, Error>(Error::new(word.span(), "conflicting override"));
        for subword in word.to_string().split('_') {
            match subword {
                "unstable" => {
                    if unstable.is_some() {
                        return conflict_err();
                    }
                    unstable = Some(true);
                }
                "noagent" => {
                    if with_vtl0_pipette.is_some() {
                        return conflict_err();
                    }
                    with_vtl0_pipette = Some(false);
                }
                "vpci" => {
                    if requires_vpci.is_some() {
                        return conflict_err();
                    }
                    requires_vpci = Some(true);
                }
                "hyperv" => {
                    if vmm.is_some() {
                        return conflict_err();
                    }
                    vmm = Some(Vmm::HyperV);
                }
                "openvmm" => {
                    if vmm.is_some() {
                        return conflict_err();
                    }
                    vmm = Some(Vmm::OpenVmm);
                }
                _ => return Err(Error::new(word.span(), "unrecognized vmm test override")),
            }
        }

        let unstable = unstable.unwrap_or(false);
        let with_vtl0_pipette = with_vtl0_pipette.unwrap_or(true);
        let requires_vpci = requires_vpci.unwrap_or(false);

        let parens;
        syn::parenthesized!(parens in input);
        let args = parens.parse::<Args>()?;

        Ok(ArgsWithOverrides {
            args,
            vmm,
            with_vtl0_pipette,
            unstable,
            requires_vpci,
        })
    }
}

impl ArgsWithOverrides {
    fn resolve(self) -> syn::Result<ResolvedArgs> {
        let ArgsWithOverrides {
            args: Args { configs },
            vmm,
            unstable,
            with_vtl0_pipette,
            requires_vpci,
        } = self;

        let mut resolved_configs = Vec::new();

        for config in configs.into_iter() {
            resolved_configs.push(ResolvedConfig {
                vmm: match (vmm, config.vmm) {
                    (Some(Vmm::HyperV), Some(Vmm::HyperV))
                    | (Some(Vmm::HyperV), None)
                    | (None, Some(Vmm::HyperV)) => Vmm::HyperV,
                    (Some(Vmm::OpenVmm), Some(Vmm::OpenVmm))
                    | (Some(Vmm::OpenVmm), None)
                    | (None, Some(Vmm::OpenVmm)) => Vmm::OpenVmm,
                    (None, None) => return Err(Error::new(config.span, "vmm must be specified")),
                    _ => return Err(Error::new(config.span, "vmm mismatch")),
                },
                firmware: config.firmware,
                arch: config.arch,
                extra_deps: config.extra_deps,
                unstable: config.unstable || unstable,
                requires_vpci,
            });
        }

        Ok(ResolvedArgs {
            configs: resolved_configs,
            with_vtl0_pipette,
        })
    }
}

impl Parse for Args {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error("expected at least one firmware entry"));
        }

        let configs: Vec<_> = input
            .parse_terminated(Config::parse, Token![,])?
            .into_iter()
            .collect();

        for config in &configs {
            #[expect(clippy::single_match)] // more patterns coming later
            match config.firmware {
                Firmware::Uefi(UefiGuest::Vhd(ImageInfo { arch, .. })) => {
                    if config.arch != arch {
                        return Err(Error::new(
                            config.span,
                            "firmware architecture must match guest architecture",
                        ));
                    }
                }
                _ => {}
            }
        }

        Ok(Args { configs })
    }
}

impl Parse for Config {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let word = input.parse::<Ident>()?;
        let word_string = word.to_string();

        let (unstable, remainder) = if let Some(remainder) = word_string.strip_prefix("unstable_") {
            (true, remainder)
        } else {
            (false, word_string.as_str())
        };

        let (vmm, remainder) = if let Some(remainder) = remainder.strip_prefix("hyperv_") {
            (Some(Vmm::HyperV), remainder)
        } else if let Some(remainder) = remainder.strip_prefix("openvmm_") {
            (Some(Vmm::OpenVmm), remainder)
        } else {
            (None, remainder)
        };

        let (arch, firmware) = match remainder {
            "linux_direct_x64" => (MachineArch::X86_64, Firmware::LinuxDirect),
            "linux_direct_aarch64" => (MachineArch::Aarch64, Firmware::LinuxDirect),
            "openhcl_linux_direct_x64" => (MachineArch::X86_64, Firmware::OpenhclLinuxDirect),
            "pcat_x64" => (
                MachineArch::X86_64,
                Firmware::Pcat(parse_pcat_guest(input)?),
            ),
            "uefi_x64" => (
                MachineArch::X86_64,
                Firmware::Uefi(parse_uefi_guest(input)?),
            ),
            "uefi_aarch64" => (
                MachineArch::Aarch64,
                Firmware::Uefi(parse_uefi_guest(input)?),
            ),
            "openhcl_pcat_x64" => (
                MachineArch::X86_64,
                Firmware::OpenhclPcat(parse_pcat_guest(input)?),
            ),
            "openhcl_uefi_x64" => (
                MachineArch::X86_64,
                Firmware::OpenhclUefi(parse_openhcl_uefi_options(input)?, parse_uefi_guest(input)?),
            ),
            "openhcl_uefi_aarch64" => (
                MachineArch::Aarch64,
                Firmware::OpenhclUefi(parse_openhcl_uefi_options(input)?, parse_uefi_guest(input)?),
            ),
            "openhcl_linux_direct_aarch64" | "pcat_aarch64" => {
                return Err(Error::new(
                    word.span(),
                    "aarch64 is not supported for this firmware, use x64 instead",
                ));
            }
            _ => return Err(Error::new(word.span(), "unrecognized firmware")),
        };

        let extra_deps = parse_extra_deps(input)?;

        Ok(Config {
            vmm,
            firmware,
            arch,
            span: input.span(),
            extra_deps,
            unstable,
        })
    }
}

fn parse_pcat_guest(input: ParseStream<'_>) -> syn::Result<PcatGuest> {
    let parens;
    syn::parenthesized!(parens in input);
    parens.parse::<PcatGuest>()
}

fn parse_uefi_guest(input: ParseStream<'_>) -> syn::Result<UefiGuest> {
    let parens;
    syn::parenthesized!(parens in input);
    parens.parse::<UefiGuest>()
}

impl Parse for PcatGuest {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let word = input.parse::<Ident>()?;
        match &*word.to_string() {
            "vhd" => {
                let parens;
                syn::parenthesized!(parens in input);
                let vhd = parse_vhd(&parens, Generation::Gen1)?;
                Ok(PcatGuest::Vhd(vhd))
            }
            "iso" => {
                let parens;
                syn::parenthesized!(parens in input);
                let iso = parse_iso(&parens)?;
                Ok(PcatGuest::Iso(iso))
            }
            _ => Err(Error::new(word.span(), "unrecognized pcat guest")),
        }
    }
}

impl Parse for UefiGuest {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let word = input.parse::<Ident>()?;
        match &*word.to_string() {
            "guest_test_uefi_x64" => Ok(UefiGuest::GuestTestUefi(MachineArch::X86_64)),
            "guest_test_uefi_aarch64" => Ok(UefiGuest::GuestTestUefi(MachineArch::Aarch64)),
            "none" => Ok(UefiGuest::None),
            "vhd" => {
                let parens;
                syn::parenthesized!(parens in input);
                let vhd = parse_vhd(&parens, Generation::Gen2)?;
                Ok(UefiGuest::Vhd(vhd))
            }
            _ => Err(Error::new(word.span(), "unrecognized uefi guest")),
        }
    }
}

enum Generation {
    Gen1,
    Gen2,
}

fn parse_vhd(input: ParseStream<'_>, generation: Generation) -> syn::Result<ImageInfo> {
    let word = input.parse::<Ident>()?;

    macro_rules! image_info {
        ($artifact:ty) => {
            ImageInfo {
                image_artifact: quote!($artifact),
                arch: <$artifact>::ARCH,
                name_prefix: word.to_string(),
            }
        };
    }

    match &*word.to_string() {
        "freebsd_13_2_x64" => match generation {
            Generation::Gen1 => Ok(image_info!(
                ::petri_artifacts_vmm_test::artifacts::test_vhd::FREE_BSD_13_2_X64
            )),
            Generation::Gen2 => Err(Error::new(
                word.span(),
                "FreeBSD 13.2 is not available for UEFI",
            )),
        },
        "windows_datacenter_core_2022_x64" => match generation {
            Generation::Gen1 => Ok(image_info!(
                ::petri_artifacts_vmm_test::artifacts::test_vhd::GEN1_WINDOWS_DATA_CENTER_CORE2022_X64
            )),
            Generation::Gen2 => Ok(image_info!(
                ::petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64
            )),
        },
        "windows_datacenter_core_2025_x64" => match generation {
            Generation::Gen1 => Err(Error::new(
                word.span(),
                "Windows Server 2025 is not available for PCAT",
            )),
            Generation::Gen2 => Ok(image_info!(
                ::petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64
            )),
        },
        "windows_datacenter_core_2025_x64_prepped" => match generation {
            Generation::Gen1 => Err(Error::new(
                word.span(),
                "Windows Server 2025 is not available for PCAT",
            )),
            Generation::Gen2 => Ok(image_info!(
                ::petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2025_X64_PREPPED
            )),
        },
        "ubuntu_2404_server_x64" => Ok(image_info!(
            ::petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_X64
        )),
        "ubuntu_2504_server_x64" => Ok(image_info!(
            ::petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2504_SERVER_X64
        )),
        "alpine_3_23_x64" => Ok(image_info!(
            ::petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_X64
        )),
        "alpine_3_23_aarch64" => Ok(image_info!(
            ::petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_AARCH64
        )),
        "ubuntu_2404_server_aarch64" => Ok(image_info!(
            ::petri_artifacts_vmm_test::artifacts::test_vhd::UBUNTU_2404_SERVER_AARCH64
        )),
        "windows_11_enterprise_aarch64" => Ok(image_info!(
            ::petri_artifacts_vmm_test::artifacts::test_vhd::WINDOWS_11_ENTERPRISE_AARCH64
        )),
        _ => Err(Error::new(word.span(), "unrecognized vhd")),
    }
}

fn parse_iso(input: ParseStream<'_>) -> syn::Result<ImageInfo> {
    let word = input.parse::<Ident>()?;

    macro_rules! image_info {
        ($artifact:ty) => {
            ImageInfo {
                image_artifact: quote!($artifact),
                arch: <$artifact>::ARCH,
                name_prefix: word.to_string() + "_iso",
            }
        };
    }

    Ok(match &*word.to_string() {
        "freebsd_13_2_x64" => {
            image_info!(::petri_artifacts_vmm_test::artifacts::test_iso::FREE_BSD_13_2_X64)
        }
        _ => return Err(Error::new(word.span(), "unrecognized iso")),
    })
}

impl OpenhclUefiOptions {
    fn name_prefix(&self) -> Option<String> {
        let mut prefix = String::new();
        if let Some(isolation) = &self.isolation {
            prefix.push_str(match isolation {
                IsolationType::Vbs => "vbs",
                IsolationType::Snp => "snp",
                IsolationType::Tdx => "tdx",
            });
        }
        if prefix.is_empty() {
            None
        } else {
            Some(prefix)
        }
    }
}

impl Parse for OpenhclUefiOptions {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut options = Self::default();

        let words = input.parse_terminated(|stream| stream.parse::<Ident>(), Token![,])?;
        for word in words {
            match &*word.to_string() {
                "vbs" => {
                    if options.isolation.is_some() {
                        return Err(Error::new(word.span(), "isolation type already specified"));
                    }
                    options.isolation = Some(IsolationType::Vbs);
                }
                "snp" => {
                    if options.isolation.is_some() {
                        return Err(Error::new(word.span(), "isolation type already specified"));
                    }
                    options.isolation = Some(IsolationType::Snp);
                }
                "tdx" => {
                    if options.isolation.is_some() {
                        return Err(Error::new(word.span(), "isolation type already specified"));
                    }
                    options.isolation = Some(IsolationType::Tdx);
                }
                _ => return Err(Error::new(word.span(), "unrecognized openhcl uefi option")),
            }
        }
        Ok(options)
    }
}

impl ToTokens for IsolationType {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        tokens.extend(match self {
            IsolationType::Vbs => quote!(petri::IsolationType::Vbs),
            IsolationType::Snp => quote!(petri::IsolationType::Snp),
            IsolationType::Tdx => quote!(petri::IsolationType::Tdx),
        });
    }
}

fn parse_openhcl_uefi_options(input: ParseStream<'_>) -> syn::Result<OpenhclUefiOptions> {
    if input.peek(syn::token::Paren) {
        return Ok(Default::default());
    }

    let brackets;
    syn::bracketed!(brackets in input);
    brackets.parse()
}

fn parse_extra_deps(input: ParseStream<'_>) -> syn::Result<Vec<Path>> {
    if input.is_empty() || input.peek(Token![,]) {
        return Ok(vec![]);
    }

    let brackets;
    syn::bracketed!(brackets in input);
    let deps = brackets.parse_terminated(Path::parse, Token![,])?;
    Ok(deps.into_iter().collect())
}

/// Transform the function into VMM tests, one for each specified firmware configuration.
///
/// All options can be prefixed with "unstable_" to denote that this should
/// not block PRs if it fails.
///
/// Valid configuration options are:
/// - `{vmm}_linux_direct_{arch}`: Our provided Linux direct image
/// - `{vmm}_openhcl_linux_direct_{arch}`: Our provided Linux direct image with OpenHCL
/// - `{vmm}_pcat_{arch}(<PCAT guest>)`: A Gen 1 configuration
/// - `{vmm}_uefi_{arch}(<UEFI guest>)`: A Gen 2 configuration
/// - `{vmm}_openhcl_pcat_{arch}(<PCAT guest>)`: A Gen 1 configuration with OpenHCL
/// - `{vmm}_openhcl_uefi_{arch}[list,of,options](<UEFI guest>)`: A Gen 2 configuration with OpenHCL
///
/// Valid VMMs are:
/// - openvmm
/// - hyperv
///
/// Valid architectures are:
/// - x64
/// - aarch64
///
/// Valid PCAT guest options are:
/// - `vhd(<VHD>)`: One of our supported VHDs
/// - `iso(<ISO>)`: One of our supported ISOs
///
/// Valid UEFI guest options are:
/// - `vhd(<VHD>)`: One of our supported VHDs
/// - `guest_test_uefi_{arch}`: Our UEFI test application
/// - `none`: No guest
///
/// Valid x64 VHD options are:
/// - `alpine_3_23_x64`: Alpine Linux 3.23 cloud image
/// - `ubuntu_2404_server_x64`: Ubuntu Linux 24.04 cloudimg from Canonical
/// - `ubuntu_2504_server_x64`: Ubuntu Linux 25.04 cloudimg from Canonical
/// - `windows_datacenter_core_2022_x64`: Windows Server Datacenter Core 2022 from the Azure Marketplace
/// - `windows_datacenter_core_2025_x64`: Windows Server Datacenter Core 2025 from the Azure Marketplace
/// - `windows_datacenter_core_2025_x64_prepped`: Windows Server Datacenter Core 2025 from the Azure Marketplace,
///   pre-prepped with the pipette guest agent configured.
/// - `freebsd_13_2_x64`: FreeBSD 13.2 from the FreeBSD Project
///
/// Valid aarch64 VHD options are:
/// - `alpine_3_23_aarch64`: Alpine Linux 3.23 cloud image
/// - `ubuntu_2404_server_aarch64`: Ubuntu Linux 24.04 cloudimg from Canonical
/// - `windows_11_enterprise_aarch64`: Windows 11 Enterprise from the Azure Marketplace
///
/// Valid x64 ISO options are:
/// - `freebsd_13_2_x64`: FreeBSD 13.2 installer from the FreeBSD Project
///
/// Valid OpenHCL UEFI options are:
/// - `nvme`: Attach the boot drive via NVMe assigned to VTL2.
/// - `vbs`: Use VBS isolation.
/// - `snp`: Use SNP isolation.
/// - `tdx`: Use TDX isolation.
///
/// Each configuration can be optionally followed by a square-bracketed, comma-separated
/// list of additional artifacts required for that particular configuration.
#[proc_macro_attribute]
pub fn vmm_test(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = ArgsWithOverrides {
        args: parse_macro_input!(attr as Args),
        vmm: None,
        unstable: false,
        with_vtl0_pipette: true,
        requires_vpci: false,
    };
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but specify the following attributes to apply
/// to all tests, separated by underscores:
/// - unstable: all variants of this test are unstable
/// - noagent: don't use pipette in vtl0 for this test
/// - hyperv: use hyperv as the vmm
/// - openvmm: use openvmm as the vmm
///
/// example: #[vmm_test_with(unstable_noagent_openvmm(linux_direct_x64, ...))]
#[proc_macro_attribute]
pub fn vmm_test_with(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = parse_macro_input!(attr as ArgsWithOverrides);
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but only for OpenVMM tests
// TODO: remove this and replace occurrences with `vmm_test_with`
#[proc_macro_attribute]
pub fn openvmm_test(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = ArgsWithOverrides {
        args: parse_macro_input!(attr as Args),
        vmm: Some(Vmm::OpenVmm),
        unstable: false,
        with_vtl0_pipette: true,
        requires_vpci: false,
    };
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but only for OpenVMM tests and without using pipette in VTL0.
// TODO: remove this and replace occurrences with `vmm_test_with`
#[proc_macro_attribute]
pub fn openvmm_test_no_agent(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = ArgsWithOverrides {
        args: parse_macro_input!(attr as Args),
        vmm: Some(Vmm::OpenVmm),
        unstable: false,
        with_vtl0_pipette: false,
        requires_vpci: false,
    };
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn make_vmm_test(args: ArgsWithOverrides, item: ItemFn) -> syn::Result<TokenStream> {
    let args = args.resolve()?;

    let original_args = match item.sig.inputs.len() {
        1 => quote! {config},
        2 => quote! {config, extra_deps},
        3 => quote! {config, extra_deps, driver },
        _ => {
            return Err(Error::new(
                item.sig.inputs.span(),
                "expected 1, 2, or 3 arguments (the PetriVmConfig, ArtifactResolver, and Driver)",
            ));
        }
    };

    let with_vtl0_pipette = args.with_vtl0_pipette.to_token_stream();

    let original_name = &item.sig.ident;
    let mut tests = TokenStream::new();
    // FUTURE: compute all this in code instead of in the macro.
    for config in args.configs {
        let name = format!("{}_{original_name}", config.name_prefix());

        // Build requirements based on the configuration and resolved VMM
        let requirements = build_requirements(&config.firmware, config.vmm, config.requires_vpci);

        // Now move the values for the FirmwareAndArch and extra_deps
        let extra_deps = config.extra_deps;

        let firmware = FirmwareAndArch {
            firmware: config.firmware,
            arch: config.arch,
        };
        let arch = arch_to_tokens(config.arch);

        let (cfg_conditions, artifacts, petri_vm_config) = match config.vmm {
            Vmm::HyperV => (
                quote!(#[cfg(windows)]),
                quote!(::petri::PetriVmArtifacts::<::petri::hyperv::HyperVPetriBackend>),
                quote!(::petri::PetriVmBuilder::<::petri::hyperv::HyperVPetriBackend>),
            ),
            Vmm::OpenVmm => (
                quote!(),
                quote!(::petri::PetriVmArtifacts::<::petri::openvmm::OpenVmmPetriBackend>),
                quote!(::petri::PetriVmBuilder::<::petri::openvmm::OpenVmmPetriBackend>),
            ),
        };

        let remote_access = match config.vmm {
            Vmm::HyperV => quote!(::petri::RemoteAccess::LocalOnly),
            Vmm::OpenVmm => quote!(::petri::RemoteAccess::Allow),
        };

        let petri_vm_config = quote!(#petri_vm_config::new(params, artifacts, &driver)?);
        let unstable = config.unstable.to_token_stream();

        let test = quote! {
            #cfg_conditions
            ::petri::SimpleTest::new(
                #name,
                |resolver| {
                    let firmware = #firmware;
                    let arch = #arch;
                    let extra_deps = (#(resolver.require(#extra_deps),)*);
                    let artifacts = #artifacts::new(resolver, firmware, arch, #with_vtl0_pipette)?;
                    Some((artifacts, extra_deps))
                },
                |params, (artifacts, extra_deps)| {
                    ::pal_async::DefaultPool::run_with(async |driver| {
                        let config = #petri_vm_config;
                        #original_name(#original_args).await
                    })
                },
                Some(#requirements),
                #unstable,
                #remote_access,
            ).into(),
        };

        tests.extend(test);
    }

    Ok(quote! {
        ::petri::multitest!(vec![#tests]);
        #item
    })
}

// Helper to build requirements TokenStream for firmware and resolved VMM
fn build_requirements(firmware: &Firmware, resolved_vmm: Vmm, requires_vpci: bool) -> TokenStream {
    let mut requirement_expr: TokenStream = quote!(::petri::requirements::TestRequirement::Any);
    let mut is_vbs = false;
    // Add isolation requirement if specified
    if let Firmware::OpenhclUefi(
        OpenhclUefiOptions {
            isolation: Some(isolation),
        },
        _,
    ) = firmware
    {
        let isolation_requirement = match isolation {
            IsolationType::Vbs => {
                is_vbs = true;
                quote!(::petri::requirements::TestRequirement::Isolation(
                    ::petri::requirements::IsolationType::Vbs
                ))
            }
            IsolationType::Snp => quote!(::petri::requirements::TestRequirement::Isolation(
                ::petri::requirements::IsolationType::Snp
            )),
            IsolationType::Tdx => quote!(::petri::requirements::TestRequirement::Isolation(
                ::petri::requirements::IsolationType::Tdx
            )),
        };

        requirement_expr = quote!(#requirement_expr.and(#isolation_requirement));
    }

    let is_hyperv = resolved_vmm == Vmm::HyperV;

    if is_hyperv && is_vbs {
        requirement_expr = quote!(#requirement_expr.and(
            ::petri::requirements::TestRequirement::ExecutionEnvironment(
                ::petri::requirements::ExecutionEnvironment::Baremetal
            )
        ));
    }

    if requires_vpci {
        requirement_expr =
            quote!(#requirement_expr.and(::petri::requirements::TestRequirement::VpciSupport));
    }

    quote!(
        ::petri::requirements::TestCaseRequirements::new(#requirement_expr)
    )
}
