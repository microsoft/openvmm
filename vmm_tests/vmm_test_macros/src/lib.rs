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
    OpenhclUefi(OpenhclUefiOptions, UefiGuest),
}

#[derive(Default)]
struct OpenhclUefiOptions {
    nvme: bool,
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

impl Config {
    fn name_prefix(&self, resolved_vmm: Vmm) -> String {
        let arch_prefix = arch_to_str(self.arch);

        let vmm_prefix = match resolved_vmm {
            Vmm::OpenVmm => "openvmm",
            Vmm::HyperV => "hyperv",
        };

        let firmware_prefix = match &self.firmware {
            Firmware::LinuxDirect => "linux",
            Firmware::Pcat(_) => "pcat",
            Firmware::Uefi(_) => "uefi",
            Firmware::OpenhclLinuxDirect => "openhcl_linux",
            Firmware::OpenhclUefi(..) => "openhcl_uefi",
        };

        let guest_prefix = match &self.firmware {
            Firmware::LinuxDirect | Firmware::OpenhclLinuxDirect => None,
            Firmware::Pcat(guest) => Some(guest.name_prefix()),
            Firmware::Uefi(guest) | Firmware::OpenhclUefi(_, guest) => guest.name_prefix(),
        };

        let options_prefix = match &self.firmware {
            Firmware::LinuxDirect
            | Firmware::Pcat(_)
            | Firmware::Uefi(_)
            | Firmware::OpenhclLinuxDirect => None,
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
                quote!(::petri::PcatGuest::Vhd(petri::BootImageConfig::from_vhd(resolver.require(#vhd))))
            }
            PcatGuest::Iso(known_iso) => {
                let iso = known_iso.image_artifact.clone();
                quote!(::petri::PcatGuest::Iso(petri::BootImageConfig::from_iso(resolver.require(#iso))))
            }
        });
    }
}

impl UefiGuest {
    fn name_prefix(&self) -> Option<String> {
        match self {
            UefiGuest::Vhd(known_vhd) => Some(known_vhd.name_prefix.to_owned()),
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
                quote!(::petri::UefiGuest::Vhd(petri::BootImageConfig::from_vhd(resolver.require(#v))))
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
            Firmware::OpenhclUefi(OpenhclUefiOptions { nvme, isolation }, guest) => {
                let isolation = match isolation {
                    Some(i) => quote!(Some(#i)),
                    None => quote!(None),
                };
                quote!(::petri::Firmware::openhcl_uefi(resolver, #arch, #guest, #isolation, #nvme))
            }
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

        let (vmm, remainder) = if let Some(remainder) = word_string.strip_prefix("hyperv_") {
            (Some(Vmm::HyperV), remainder)
        } else if let Some(remainder) = word_string.strip_prefix("openvmm_") {
            (Some(Vmm::OpenVmm), remainder)
        } else {
            (None, word_string.as_str())
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
        if self.nvme {
            if !prefix.is_empty() {
                prefix.push('_');
            }
            prefix.push_str("nvme");
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
                "nvme" => {
                    options.nvme = true;
                }
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
/// Valid configuration options are:
/// - `{vmm}_linux_direct_{arch}`: Our provided Linux direct image
/// - `{vmm}_openhcl_linux_direct_{arch}`: Our provided Linux direct image with OpenHCL
/// - `{vmm}_pcat_{arch}(<PCAT guest>)`: A Gen 1 configuration
/// - `{vmm}_uefi_{arch}(<UEFI guest>)`: A Gen 2 configuration
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
/// - `ubuntu_2404_server_x64`: Ubuntu Linux 24.04 cloudimg from Canonical
/// - `ubuntu_2504_server_x64`: Ubuntu Linux 25.04 cloudimg from Canonical
/// - `windows_datacenter_core_2022_x64`: Windows Server Datacenter Core 2022 from the Azure Marketplace
/// - `windows_datacenter_core_2025_x64`: Windows Server Datacenter Core 2025 from the Azure Marketplace
/// - `windows_datacenter_core_2025_x64_prepped`: Windows Server Datacenter Core 2025 from the Azure Marketplace,
///   pre-prepped with the pipette guest agent configured.
/// - `freebsd_13_2_x64`: FreeBSD 13.2 from the FreeBSD Project
///
/// Valid aarch64 VHD options are:
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
    let args = parse_macro_input!(attr as Args);
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item, None, true)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but without using pipette in VTL0.
#[proc_macro_attribute]
pub fn vmm_test_no_agent(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = parse_macro_input!(attr as Args);
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item, None, false)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but only for OpenVMM tests
#[proc_macro_attribute]
pub fn openvmm_test(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = parse_macro_input!(attr as Args);
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item, Some(Vmm::OpenVmm), true)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but only for OpenVMM tests and without using pipette in VTL0.
#[proc_macro_attribute]
pub fn openvmm_test_no_agent(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = parse_macro_input!(attr as Args);
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item, Some(Vmm::OpenVmm), false)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but only for Hyper-V tests
#[proc_macro_attribute]
pub fn hyperv_test(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = parse_macro_input!(attr as Args);
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item, Some(Vmm::HyperV), true)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn make_vmm_test(
    args: Args,
    item: ItemFn,
    specific_vmm: Option<Vmm>,
    with_vtl0_pipette: bool,
) -> syn::Result<TokenStream> {
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

    let original_name = &item.sig.ident;
    let mut tests = TokenStream::new();
    // FUTURE: compute all this in code instead of in the macro.
    for config in args.configs {
        // Resolve the VMM backend early by combining specific_vmm and config.vmm
        let resolved_vmm = match (specific_vmm, config.vmm) {
            (Some(Vmm::HyperV), Some(Vmm::HyperV))
            | (Some(Vmm::HyperV), None)
            | (None, Some(Vmm::HyperV)) => Vmm::HyperV,
            (Some(Vmm::OpenVmm), Some(Vmm::OpenVmm))
            | (Some(Vmm::OpenVmm), None)
            | (None, Some(Vmm::OpenVmm)) => Vmm::OpenVmm,
            (None, None) => return Err(Error::new(config.span, "vmm must be specified")),
            _ => return Err(Error::new(config.span, "vmm mismatch")),
        };

        let name = format!("{}_{original_name}", config.name_prefix(resolved_vmm));

        // Build requirements based on the configuration and resolved VMM
        let requirements = build_requirements(&config.firmware, &name, resolved_vmm);
        let requirements = if let Some(req) = requirements {
            quote! { Some(#req) }
        } else {
            quote! { None }
        };

        // Now move the values for the FirmwareAndArch and extra_deps
        let extra_deps = config.extra_deps;

        let firmware = FirmwareAndArch {
            firmware: config.firmware,
            arch: config.arch,
        };
        let arch = arch_to_tokens(config.arch);

        let (cfg_conditions, artifacts, petri_vm_config) = match resolved_vmm {
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

        let petri_vm_config = quote!(#petri_vm_config::new(params, artifacts, &driver)?);

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
                #requirements
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
fn build_requirements(firmware: &Firmware, name: &str, resolved_vmm: Vmm) -> Option<TokenStream> {
    let mut requirement_expr: Option<TokenStream> = None;
    let mut is_vbs = false;
    // Add isolation requirement if specified
    if let Firmware::OpenhclUefi(
        OpenhclUefiOptions {
            isolation: Some(isolation),
            ..
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

        requirement_expr = Some(isolation_requirement);
    }

    // Special case for "servicing" tests
    if name.contains("servicing") {
        let servicing_expr = quote!(::petri::requirements::TestRequirement::Not(Box::new(
            ::petri::requirements::TestRequirement::And(
                Box::new(::petri::requirements::TestRequirement::Vendor(
                    ::petri::requirements::Vendor::Amd
                )),
                Box::new(
                    ::petri::requirements::TestRequirement::ExecutionEnvironment(
                        ::petri::requirements::ExecutionEnvironment::Nested
                    )
                )
            )
        )));

        requirement_expr = match requirement_expr {
            Some(existing) => Some(quote!(
                ::petri::requirements::TestRequirement::And(
                    Box::new(#existing),
                    Box::new(#servicing_expr)
                )
            )),
            None => Some(servicing_expr),
        };
    }

    let is_hyperv = resolved_vmm == Vmm::HyperV;

    if is_hyperv && is_vbs {
        let hyperv_vbs_requirement_expr = quote!(
            ::petri::requirements::TestRequirement::ExecutionEnvironment(
                ::petri::requirements::ExecutionEnvironment::Baremetal
            )
        );
        requirement_expr = match requirement_expr {
            Some(existing) => Some(quote!(
                ::petri::requirements::TestRequirement::And(
                    Box::new(#existing),
                    Box::new(#hyperv_vbs_requirement_expr)
                )
            )),
            None => Some(hyperv_vbs_requirement_expr),
        };
    }

    if requirement_expr.is_some() {
        Some(quote!(::petri::requirements::TestCaseRequirements::new(#requirement_expr)))
    } else {
        None
    }
}
