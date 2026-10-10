// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use petri_artifacts_common::capabilities;
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
    /// If unstable, the reason why.
    unstable: Option<String>,
    /// If ignored, the reason why (compile-time documentation only).
    ignored: Option<String>,
}

struct ResolvedConfig {
    vmm: Vmm,
    firmware: Firmware,
    arch: MachineArch,
    extra_deps: Vec<Path>,
    unstable: Option<String>,
    ignored: Option<String>,
    requires_host_vendor: Option<HostVendor>,
    requires_capabilities: Vec<&'static str>,
}

struct MaybeNestedResolvedConfig {
    l1_config: ResolvedConfig,
    nested_config: Option<NestedResolvedConfig>,
}

struct NestedResolvedConfig {
    archive_artifact: Path,
    test_module: syn::LitStr,
    inner_module: syn::LitStr,
    test_fn: Ident,
    l2_config: ResolvedConfig,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Vmm {
    #[expect(clippy::enum_variant_names)]
    OpenVmm,
    HyperV,
    Qemu,
}

/// Host CPU vendor that a test can be restricted to via macro overrides.
///
/// When set, the test is marked ignored at runtime on hosts whose vendor
/// does not match (e.g. an `amd`-gated test is skipped on Intel hosts).
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostVendor {
    Amd,
    Intel,
}

enum Firmware {
    LinuxDirect,
    LinuxDirectBzImage,
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
    configs: Vec<MaybeNestedConfig>,
}

struct ArgsWithOverrides {
    args: Args,
    overrides: ParsedOverrides,
}

struct ResolvedArgs {
    configs: Vec<MaybeNestedResolvedConfig>,
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
            Vmm::Qemu => "qemu",
        };

        let firmware_prefix = match &self.firmware {
            Firmware::LinuxDirect => "linux",
            Firmware::LinuxDirectBzImage => "linux_bzimage",
            Firmware::Pcat(_) => "pcat",
            Firmware::Uefi(_) => "uefi",
            Firmware::OpenhclLinuxDirect => "openhcl_linux",
            Firmware::OpenhclPcat(..) => "openhcl_pcat",
            Firmware::OpenhclUefi(..) => "openhcl_uefi",
        };

        let guest_prefix = match &self.firmware {
            Firmware::LinuxDirect | Firmware::LinuxDirectBzImage | Firmware::OpenhclLinuxDirect => {
                None
            }
            Firmware::Pcat(guest) | Firmware::OpenhclPcat(guest) => Some(guest.name_prefix()),
            Firmware::Uefi(guest) | Firmware::OpenhclUefi(_, guest) => guest.name_prefix(),
        };

        let options_prefix = match &self.firmware {
            Firmware::LinuxDirect
            | Firmware::LinuxDirectBzImage
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
            Firmware::LinuxDirectBzImage => {
                quote!(::petri::Firmware::linux_direct_bzimage(resolver))
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
        // Syntax: a comma-separated list of attributes followed by a single
        // `configs(...)` group of firmware entries, e.g.
        //   #[vmm_test_with(openvmm, amd, configs(linux_direct_x64, ...))]
        //   #[vmm_test_with(requires(vpci), configs(...))]
        //
        // By convention the vmm (if specified) comes first and `configs(...)`
        // comes last; both are enforced below. Floating the attributes as
        // plain idents (rather than wrapping them in a tuple) keeps the whole
        // attribute a valid meta item, which is what lets rustfmt format it.
        let mut overrides = ParsedOverrides::new();
        let mut args = None;
        let mut position = 0usize;

        while !input.is_empty() {
            let ident = input.parse::<Ident>()?;
            let ident_string = ident.to_string();
            match ident_string.as_str() {
                "configs" => {
                    let configs;
                    syn::parenthesized!(configs in input);
                    args = Some(configs.parse::<Args>()?);
                    // Tolerate an optional trailing comma after `configs(...)`.
                    if input.peek(Token![,]) {
                        input.parse::<Token![,]>()?;
                    }
                    if !input.is_empty() {
                        return Err(input.error("`configs(...)` must be the last argument"));
                    }
                    break;
                }
                "requires" => {
                    let capabilities;
                    syn::parenthesized!(capabilities in input);
                    for capability in parse_required_capabilities(&capabilities)? {
                        overrides.add_capability(capability.span, capability.name)?;
                    }
                }
                "openvmm" | "hyperv" | "qemu" => {
                    if position != 0 {
                        return Err(Error::new(
                            ident.span(),
                            "the vmm must be the first argument",
                        ));
                    }
                    overrides.vmm = match ident_string.as_str() {
                        "openvmm" => Some(Vmm::OpenVmm),
                        "hyperv" => Some(Vmm::HyperV),
                        "qemu" => Some(Vmm::Qemu),
                        _ => unreachable!(),
                    };
                }
                "unstable" | "ignore" => {
                    let inner;
                    syn::parenthesized!(inner in input);
                    let reason = parse_reason(&inner)?;
                    // Tolerate a trailing comma (e.g. from rustfmt).
                    let _: Option<Token![,]> = inner.parse()?;
                    if !inner.is_empty() {
                        return Err(inner.error(
                            "whole-list `ignore`/`unstable` takes only `reason = \"...\"`; \
                             wrap an individual config to scope it",
                        ));
                    }
                    overrides.set_flag_reason(&ident, reason)?;
                }
                _ => overrides.apply_ident(&ident)?,
            }

            position += 1;
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }

        let args =
            args.ok_or_else(|| input.error("expected a `configs(...)` group of firmware entries"))?;

        Ok(overrides.finish(args))
    }
}

struct ParsedOverrides {
    vmm: Option<Vmm>,
    unstable: Option<String>,
    ignored: Option<String>,
    with_vtl0_pipette: bool,
    requires_host_vendor: Option<HostVendor>,
    requires_capabilities: Vec<&'static str>,
}

impl ParsedOverrides {
    fn new() -> Self {
        Self {
            vmm: None,
            unstable: None,
            ignored: None,
            with_vtl0_pipette: true,
            requires_host_vendor: None,
            requires_capabilities: Vec::new(),
        }
    }

    fn apply_ident(&mut self, ident: &Ident) -> syn::Result<()> {
        let ident_string = ident.to_string();
        let conflict_err = || Err(Error::new(ident.span(), "conflicting override"));
        match ident_string.as_str() {
            "noagent" => {
                if !self.with_vtl0_pipette {
                    return conflict_err();
                }
                self.with_vtl0_pipette = false;
            }
            "amd" => {
                if self.requires_host_vendor.is_some() {
                    return conflict_err();
                }
                self.requires_host_vendor = Some(HostVendor::Amd);
            }
            "intel" => {
                if self.requires_host_vendor.is_some() {
                    return conflict_err();
                }
                self.requires_host_vendor = Some(HostVendor::Intel);
            }
            _ => return Err(Error::new(ident.span(), "unrecognized vmm test override")),
        }
        Ok(())
    }

    fn set_flag_reason(&mut self, ident: &Ident, reason: String) -> syn::Result<()> {
        let slot = match ident.to_string().as_str() {
            "unstable" => &mut self.unstable,
            "ignore" => &mut self.ignored,
            _ => unreachable!(),
        };
        if slot.is_some() {
            return Err(Error::new(ident.span(), "conflicting override"));
        }
        *slot = Some(reason);
        Ok(())
    }

    fn add_capability(&mut self, span: Span, capability: &'static str) -> syn::Result<()> {
        if self.requires_capabilities.contains(&capability) {
            return Err(Error::new(span, "duplicate required capability"));
        }
        self.requires_capabilities.push(capability);
        Ok(())
    }

    fn finish(self, args: Args) -> ArgsWithOverrides {
        ArgsWithOverrides {
            args,
            overrides: self,
        }
    }
}

struct RequiredCapability {
    span: Span,
    name: &'static str,
}

fn parse_required_capabilities(input: ParseStream<'_>) -> syn::Result<Vec<RequiredCapability>> {
    let idents = input.parse_terminated(Ident::parse, Token![,])?;
    if idents.is_empty() {
        return Err(input.error("requires expects at least one capability"));
    }

    idents
        .into_iter()
        .map(|ident| {
            let name = ident.to_string();
            let name = capabilities::known(&name).ok_or_else(|| {
                Error::new(ident.span(), format!("unknown test capability `{name}`"))
            })?;
            Ok(RequiredCapability {
                span: ident.span(),
                name,
            })
        })
        .collect()
}

impl Config {
    fn resolve(self, overrides: &ParsedOverrides) -> syn::Result<ResolvedConfig> {
        Ok(ResolvedConfig {
            vmm: match (overrides.vmm, self.vmm) {
                (Some(Vmm::HyperV), Some(Vmm::HyperV))
                | (Some(Vmm::HyperV), None)
                | (None, Some(Vmm::HyperV)) => Vmm::HyperV,
                (Some(Vmm::OpenVmm), Some(Vmm::OpenVmm))
                | (Some(Vmm::OpenVmm), None)
                | (None, Some(Vmm::OpenVmm)) => Vmm::OpenVmm,
                (Some(Vmm::Qemu), Some(Vmm::Qemu))
                | (Some(Vmm::Qemu), None)
                | (None, Some(Vmm::Qemu)) => Vmm::Qemu,
                (None, None) => {
                    return Err(Error::new(self.span, "vmm must be specified"));
                }
                _ => return Err(Error::new(self.span, "vmm mismatch")),
            },
            firmware: self.firmware,
            arch: self.arch,
            extra_deps: self.extra_deps,
            // A per-config wrapper reason wins over the whole-list override.
            // A config may carry both `unstable` and `ignored`; `ignore`
            // dominates at runtime (the test is skipped).
            unstable: self.unstable.or_else(|| overrides.unstable.clone()),
            ignored: self.ignored.or_else(|| overrides.ignored.clone()),
            requires_host_vendor: overrides.requires_host_vendor,
            requires_capabilities: overrides.requires_capabilities.clone(),
        })
    }
}

impl NestedConfig {
    fn resolve(self, overrides: &ParsedOverrides) -> syn::Result<NestedResolvedConfig> {
        Ok(NestedResolvedConfig {
            archive_artifact: self.archive_artifact,
            test_module: self.test_module,
            inner_module: self.inner_module,
            test_fn: self.test_fn,
            l2_config: self.l2_config.resolve(overrides)?,
        })
    }
}

impl MaybeNestedConfig {
    fn resolve(self, overrides: &ParsedOverrides) -> syn::Result<MaybeNestedResolvedConfig> {
        let mut default_overrides = ParsedOverrides::new();
        let (l1_overrides, l2_overrides) = if self.nested_config.is_some() {
            default_overrides.add_capability(self.l1_config.span, "nested_virt")?;
            (&default_overrides, overrides)
        } else {
            (overrides, &default_overrides)
        };
        Ok(MaybeNestedResolvedConfig {
            l1_config: self.l1_config.resolve(l1_overrides)?,
            nested_config: self
                .nested_config
                .map(|c| c.resolve(l2_overrides))
                .transpose()?,
        })
    }
}

impl ArgsWithOverrides {
    fn resolve(self) -> syn::Result<ResolvedArgs> {
        let ArgsWithOverrides {
            args: Args { configs },
            overrides,
        } = self;

        let mut resolved_configs = Vec::new();

        for config in configs.into_iter() {
            resolved_configs.push(config.resolve(&overrides)?);
        }

        Ok(ResolvedArgs {
            configs: resolved_configs,
            with_vtl0_pipette: overrides.with_vtl0_pipette,
        })
    }
}

impl Parse for Args {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        if input.is_empty() {
            return Err(input.error("expected at least one firmware entry"));
        }

        let configs: Vec<_> = input
            .parse_terminated(MaybeNestedConfig::parse, Token![,])?
            .into_iter()
            .collect();

        if configs.is_empty() {
            return Err(input.error("expected at least one firmware entry"));
        }

        for config in &configs {
            validate_firmware(&config.l1_config)?;
            if let Some(nested_config) = config.nested_config.as_ref() {
                validate_firmware(&nested_config.l2_config)?;
            }
        }

        Ok(Args { configs })
    }
}

fn validate_firmware(config: &Config) -> syn::Result<()> {
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
    Ok(())
}

impl Parse for MaybeNestedConfig {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let word = input.parse::<Ident>()?;
        let word_string = word.to_string();

        if word_string == "nested" {
            let inner;
            syn::parenthesized!(inner in input);
            parse_nested(&inner)
        } else {
            Ok(parse_config(input, word)?.into())
        }
    }
}

impl Parse for Config {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let word = input.parse::<Ident>()?;
        parse_config(input, word)
    }
}

fn parse_config(input: ParseStream<'_>, word: Ident) -> syn::Result<Config> {
    let word_string = word.to_string();

    // Per-config `ignore(reason = "...", <config>)` /
    // `unstable(reason = "...", <config>)` wrappers.
    if word_string == "ignore" || word_string == "unstable" {
        let inner;
        syn::parenthesized!(inner in input);
        let reason = parse_reason(&inner)?;
        inner.parse::<Token![,]>().map_err(|_| {
            Error::new(
                word.span(),
                "per-config `ignore`/`unstable` requires a config, e.g. \
                     `ignore(reason = \"...\", linux_direct_x64)`",
            )
        })?;
        let mut config = inner.parse::<Config>()?;
        // Tolerate a trailing comma (e.g. from rustfmt).
        let _: Option<Token![,]> = inner.parse()?;
        if !inner.is_empty() {
            return Err(inner.error("expected a single config after the reason"));
        }
        if config.unstable.is_some() || config.ignored.is_some() {
            return Err(Error::new(
                word.span(),
                "cannot nest `ignore`/`unstable` wrappers",
            ));
        }
        if word_string == "ignore" {
            config.ignored = Some(reason);
        } else {
            config.unstable = Some(reason);
        }
        return Ok(config);
    }

    let (vmm, remainder) = if let Some(remainder) = word_string.strip_prefix("hyperv_") {
        (Some(Vmm::HyperV), remainder)
    } else if let Some(remainder) = word_string.strip_prefix("openvmm_") {
        (Some(Vmm::OpenVmm), remainder)
    } else if let Some(remainder) = word_string.strip_prefix("qemu_") {
        (Some(Vmm::Qemu), remainder)
    } else {
        (None, word_string.as_str())
    };

    let (arch, firmware) = match remainder {
        "linux_direct_x64" => (MachineArch::X86_64, Firmware::LinuxDirect),
        "linux_direct_bzimage_x64" => (MachineArch::X86_64, Firmware::LinuxDirectBzImage),
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
        unstable: None,
        ignored: None,
    })
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
        "windows_datacenter_core_2022_x64_no_vmbus_prepped" => match generation {
            Generation::Gen1 => Err(Error::new(
                word.span(),
                "Windows Server 2022 no-vmbus prepped is not available for PCAT",
            )),
            Generation::Gen2 => Ok(image_info!(
                ::petri_artifacts_vmm_test::artifacts::test_vhd::GEN2_WINDOWS_DATA_CENTER_CORE2022_X64_NO_VMBUS_PREPPED
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

/// Parses a mandatory `reason = "..."` argument, returning the reason string.
fn parse_reason(input: ParseStream<'_>) -> syn::Result<String> {
    let ident = input.parse::<Ident>()?;
    if ident != "reason" {
        return Err(Error::new(ident.span(), "expected `reason = \"...\"`"));
    }
    input.parse::<Token![=]>()?;
    let reason = input.parse::<syn::LitStr>()?;
    Ok(reason.value())
}

struct MaybeNestedConfig {
    l1_config: Config,
    nested_config: Option<NestedConfig>,
}

impl From<Config> for MaybeNestedConfig {
    fn from(value: Config) -> Self {
        Self {
            l1_config: value,
            nested_config: None,
        }
    }
}

struct NestedConfig {
    archive_artifact: Path,
    test_module: syn::LitStr,
    inner_module: syn::LitStr,
    test_fn: Ident,
    l2_config: Config,
}

/// Parses a nested config
fn parse_nested(input: ParseStream<'_>) -> syn::Result<MaybeNestedConfig> {
    let parens;
    syn::parenthesized!(parens in input);
    let test_fn = parens.parse::<Ident>()?;
    parens.parse::<Token![,]>()?;
    let l1_config = parens.parse::<Config>()?;
    if l1_config.ignored.is_some() || l1_config.unstable.is_some() {
        return Err(Error::new(
            l1_config.span,
            "ignored/unstable should be applied to the l2 config",
        ));
    }
    let _: Option<Token![,]> = parens.parse()?;
    if !parens.is_empty() {
        return Err(parens.error("unexpected tokens after the L1 config"));
    }
    input.parse::<Token![,]>()?;

    let parens;
    syn::parenthesized!(parens in input);
    let archive_artifact = parens.parse::<Path>()?;
    parens.parse::<Token![,]>()?;
    let test_module = parens.parse::<syn::LitStr>()?;
    parens.parse::<Token![,]>()?;
    let inner_module = parens.parse::<syn::LitStr>()?;
    parens.parse::<Token![,]>()?;
    let l2_config = parens.parse::<Config>()?;
    let _: Option<Token![,]> = parens.parse()?;
    if !parens.is_empty() {
        return Err(parens.error("unexpected tokens after the L2 config"));
    }
    let _: Option<Token![,]> = input.parse()?;
    if !input.is_empty() {
        return Err(parens.error("unexpected tokens after nested config"));
    }

    Ok(MaybeNestedConfig {
        l1_config,
        nested_config: Some(NestedConfig {
            archive_artifact,
            test_module,
            inner_module,
            test_fn,
            l2_config,
        }),
    })
}

/// Transform the function into VMM tests, one for each specified firmware configuration.
///
/// An individual config can be marked unstable (runs, but failures don't block
/// PRs) or ignored (skipped by default, like a libtest `#[ignore]` test) by
/// wrapping it in `unstable(reason = "...", <config>)` or
/// `ignore(reason = "...", <config>)`. The `reason` is mandatory. Use
/// `vmm_test_with` to apply either to the whole list of configs.
///
/// Valid configuration options are:
/// - `{vmm}_linux_direct_{arch}`: Our provided Linux direct image
/// - `{vmm}_linux_direct_bzimage_x64`: Our provided Linux direct bzImage (compressed kernel, x86_64 only)
/// - `{vmm}_openhcl_linux_direct_{arch}`: Our provided Linux direct image with OpenHCL
/// - `{vmm}_pcat_{arch}(<PCAT guest>)`: A Gen 1 configuration
/// - `{vmm}_uefi_{arch}(<UEFI guest>)`: A Gen 2 configuration
/// - `{vmm}_openhcl_pcat_{arch}(<PCAT guest>)`: A Gen 1 configuration with OpenHCL
/// - `{vmm}_openhcl_uefi_{arch}[list,of,options](<UEFI guest>)`: A Gen 2 configuration with OpenHCL
///
/// Valid VMMs are:
/// - openvmm
/// - hyperv
/// - qemu
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
/// - `windows_datacenter_core_2022_x64_no_vmbus_prepped`: Windows Server Datacenter Core 2022,
///   pre-prepped with NetKVM driver and TCP pipette transport for no-vmbus testing.
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
///
/// Nested tests can be defined by wrapping the necessary information with `nested(...)`:
/// - The L1 and L2 configs, as defined above.
/// - The function that creates the L1 VM host.
/// - The information necessary to run the L2 test inside of the L1.
///
/// ```rust,ignore
/// nested(
///     (my_vm_host, l1_config),
///     (NEXTEST_ARCHIVE_WITH_NESTED_TEST, "test_binary", "test::module::path", l2_config),
/// ),
/// ```
///
#[proc_macro_attribute]
pub fn vmm_test(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let args = ArgsWithOverrides {
        args: parse_macro_input!(attr as Args),
        overrides: ParsedOverrides {
            vmm: None,
            unstable: None,
            ignored: None,
            with_vtl0_pipette: true,
            requires_host_vendor: None,
            requires_capabilities: Vec::new(),
        },
    };
    let item = parse_macro_input!(item as ItemFn);
    make_vmm_test(args, item)
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// Same options as `vmm_test`, but accepts test-wide attributes in addition to
/// the firmware entries. The attributes are listed first, as plain comma-separated
/// idents, and the firmware entries follow in a trailing `configs(...)` group:
///
/// ```ignore
/// #[vmm_test_with(<attribute>, ..., configs(<firmware entry>, ...))]
/// ```
///
/// The available attributes are:
/// - unstable(reason = "..."): all variants are unstable (failures don't block PRs)
/// - ignore(reason = "..."): all variants are ignored (skipped by default)
/// - noagent: don't use pipette in vtl0 for this test
/// - amd: this test only runs on AMD-vendor hosts (skipped otherwise)
/// - intel: this test only runs on Intel-vendor hosts (skipped otherwise)
/// - hyperv: use hyperv as the vmm
/// - openvmm: use openvmm as the vmm
/// - requires(...): required capabilities (see below)
///
/// `unstable` and `ignore` can also be applied to a single config by wrapping
/// it: `unstable(reason = "...", <config>)` / `ignore(reason = "...", <config>)`.
/// A per-config wrapper cannot itself be nested inside another.
///
/// `requires(...)` lists capabilities the test needs. Petri auto-detects some
/// capabilities, such as `vpci`, and execution environments can advertise
/// capabilities via the `PETRI_CAPABILITIES` environment variable. When a
/// required capability is not available, the test is skipped, so it
/// self-excludes on any host that cannot provide it. Capability availability
/// is evaluated in the context of each config's resolved VMM.
///
/// By convention the vmm (if specified) comes first and `configs(...)` comes
/// last; both are enforced.
///
/// example: #[vmm_test_with(noagent, configs(linux_direct_x64, ...))]
/// example: #[vmm_test_with(openvmm, requires(test_disk), configs(linux_direct_aarch64))]
/// example: #[vmm_test_with(openvmm, amd, configs(linux_direct_x64))]
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
        overrides: ParsedOverrides {
            vmm: Some(Vmm::OpenVmm),
            unstable: None,
            ignored: None,
            with_vtl0_pipette: true,
            requires_host_vendor: None,
            requires_capabilities: Vec::new(),
        },
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
        overrides: ParsedOverrides {
            vmm: Some(Vmm::OpenVmm),
            unstable: None,
            ignored: None,
            with_vtl0_pipette: false,
            requires_host_vendor: None,
            requires_capabilities: Vec::new(),
        },
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
    for mut config in args.configs {
        let no_nested_test = quote!(None);
        if let Some(mut nested_config) = config.nested_config {
            let name = format!(
                "{}_{}_on_{}_{}",
                nested_config.l2_config.name_prefix(),
                original_name,
                config.l1_config.name_prefix(),
                nested_config.test_fn,
            );
            let archive = nested_config.archive_artifact.to_token_stream();
            let inner_name = format!(
                "nested_{}_{}",
                nested_config.l2_config.name_prefix(),
                original_name,
            );
            let test_module = nested_config.test_module;
            let inner_module = nested_config.inner_module;
            let nested_test = quote! {{
                const NAME: &'static str = concat!(#inner_module, "::", #inner_name);
                resolver.require_nested(#archive, NAME);
                Some(::petri::NestedTestDeps::new(
                    #archive,
                    #test_module,
                    NAME,
                ))
            }};
            let l1_args = if config.l1_config.extra_deps.is_empty() {
                quote! {config}
            } else {
                quote! {config, extra_deps}
            };
            config.l1_config.ignored = nested_config.l2_config.ignored.take();
            config.l1_config.unstable = nested_config.l2_config.unstable.take();
            tests.extend(make_vmm_test_config(
                &name,
                &nested_config.test_fn.to_token_stream(),
                &l1_args,
                config.l1_config,
                &true.to_token_stream(),
                &nested_test,
            ));
            nested_config.l2_config.ignored = Some("always ignore l2 tests".into());
            tests.extend(make_vmm_test_config(
                &inner_name,
                &original_name.to_token_stream(),
                &original_args,
                nested_config.l2_config,
                &with_vtl0_pipette,
                &no_nested_test,
            ));
        } else {
            let name = format!("{}_{original_name}", config.l1_config.name_prefix());
            tests.extend(make_vmm_test_config(
                &name,
                &original_name.to_token_stream(),
                &original_args,
                config.l1_config,
                &with_vtl0_pipette,
                &no_nested_test,
            ));
        }
    }

    Ok(quote! {
        ::petri::multitest!(vec![#tests]);
        #item
    })
}

fn make_vmm_test_config(
    name: &String,
    original_name: &TokenStream,
    original_args: &TokenStream,
    config: ResolvedConfig,
    with_vtl0_pipette: &TokenStream,
    nested_test: &TokenStream,
) -> TokenStream {
    // Build requirements based on the configuration and resolved VMM
    let requirements = build_requirements(
        &config.firmware,
        config.vmm,
        config.requires_host_vendor,
        &config.requires_capabilities,
    );

    // Now move the values for the FirmwareAndArch and extra_deps
    let extra_deps = config.extra_deps;

    let firmware = FirmwareAndArch {
        firmware: config.firmware,
        arch: config.arch,
    };
    let arch = arch_to_tokens(config.arch);

    let (cfg_conditions, backend) = match config.vmm {
        Vmm::HyperV => (
            quote!(#[cfg(windows)]),
            quote!(::petri::hyperv::HyperVPetriBackend),
        ),
        Vmm::OpenVmm => (quote!(), quote!(::petri::openvmm::OpenVmmPetriBackend)),
        Vmm::Qemu => (quote!(), quote!(::petri::qemu::QemuPetriBackend)),
    };

    let remote_access = match config.vmm {
        Vmm::HyperV => quote!(::petri::RemoteAccess::LocalOnly),
        Vmm::OpenVmm => quote!(::petri::RemoteAccess::Allow),
        Vmm::Qemu => quote!(::petri::RemoteAccess::LocalOnly),
    };

    let petri_vm_config =
        quote!(::petri::PetriVmBuilder::<#backend>::new(params, artifacts, &driver)?);
    let unstable = match &config.unstable {
        Some(reason) => quote!(.unstable(#reason)),
        None => quote!(),
    };
    let ignore = if config.ignored.is_some() {
        quote!(.ignore())
    } else {
        quote!()
    };

    quote! {
        #cfg_conditions
        ::petri::SimpleTest::new_async(
            #name,
            |resolver| {
                let firmware = #firmware;
                let arch = #arch;
                let extra_deps = (#(resolver.require(#extra_deps),)*);
                let artifacts = ::petri::PetriVmArtifacts::<#backend>::new_maybe_nested(
                    resolver,
                    firmware,
                    arch,
                    #with_vtl0_pipette,
                    #nested_test,
                )?;
                Some((artifacts, extra_deps))
            },
            async |params, driver, (artifacts, extra_deps)| {
                let config = #petri_vm_config;
                #original_name(#original_args).await
            },
        )
        .requirements(#requirements)
        .remote_access(#remote_access)
        #unstable
        #ignore
        .into(),
    }
}

// Helper to build requirements TokenStream for firmware and resolved VMM
fn build_requirements(
    firmware: &Firmware,
    resolved_vmm: Vmm,
    requires_host_vendor: Option<HostVendor>,
    requires_capabilities: &[&'static str],
) -> TokenStream {
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

    if let Some(vendor) = requires_host_vendor {
        let vendor_tokens = match vendor {
            HostVendor::Amd => quote!(::petri::requirements::Vendor::Amd),
            HostVendor::Intel => quote!(::petri::requirements::Vendor::Intel),
        };
        requirement_expr = quote!(#requirement_expr.and(
            ::petri::requirements::TestRequirement::Vendor(#vendor_tokens)
        ));
    }

    for capability in requires_capabilities {
        let vmm = match resolved_vmm {
            Vmm::OpenVmm => quote!(::petri::requirements::VmmType::OpenVmm),
            Vmm::HyperV => quote!(::petri::requirements::VmmType::HyperV),
            Vmm::Qemu => quote!(::petri::requirements::VmmType::Qemu),
        };
        requirement_expr = quote!(#requirement_expr.and(
            ::petri::requirements::TestRequirement::RequiresCapability {
                name: #capability,
                vmm: #vmm,
            }
        ));
    }

    quote!(
        ::petri::requirements::TestCaseRequirements::new(#requirement_expr)
    )
}
