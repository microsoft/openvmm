// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

//! A basic "resource" crate which contains hard-coded Hyper-V Secure Boot
//! Template JSON files which can be embedded directly into a final binary.
//!
//! This crate should not include any `cfg(target_arch)` or `cfg(guest_arch)`
//! gates! Unused templates should be stripped from the final binary by the
//! linker.

use firmware_uefi_custom_vars::BaseTemplateVars;
use mesh_protobuf::Protobuf;

/// A concrete built-in UEFI Secure Boot template.
#[derive(Clone, Copy, Debug, Protobuf)]
pub enum UefiSecureBootTemplate {
    MicrosoftWindowsX64,
    MicrosoftWindowsAarch64,
    MicrosoftUefiCaX64,
    MicrosoftUefiCaAarch64,
}

#[derive(Clone, Copy, Debug)]
pub enum UefiTemplateGuest {
    None,
    MicrosoftWindows,
    MicrosoftUefiCa,
}

#[derive(Clone, Copy, Debug)]
pub enum UefiTemplateArch {
    X64,
    Aarch64,
}

impl UefiSecureBootTemplate {
    /// Load the selected built-in template.
    pub fn load(self) -> BaseTemplateVars {
        match self {
            Self::MicrosoftWindowsX64 => x64::microsoft_windows(),
            Self::MicrosoftWindowsAarch64 => aarch64::microsoft_windows(),
            Self::MicrosoftUefiCaX64 => x64::microsoft_uefi_ca(),
            Self::MicrosoftUefiCaAarch64 => aarch64::microsoft_uefi_ca(),
        }
    }

    /// Select a built-in template for a guest and architecture.
    pub fn pick(guest: UefiTemplateGuest, arch: UefiTemplateArch) -> Option<Self> {
        match (guest, arch) {
            (UefiTemplateGuest::None, _) => None,
            (UefiTemplateGuest::MicrosoftWindows, UefiTemplateArch::X64) => {
                Some(Self::MicrosoftWindowsX64)
            }
            (UefiTemplateGuest::MicrosoftWindows, UefiTemplateArch::Aarch64) => {
                Some(Self::MicrosoftWindowsAarch64)
            }
            (UefiTemplateGuest::MicrosoftUefiCa, UefiTemplateArch::X64) => {
                Some(Self::MicrosoftUefiCaX64)
            }
            (UefiTemplateGuest::MicrosoftUefiCa, UefiTemplateArch::Aarch64) => {
                Some(Self::MicrosoftUefiCaAarch64)
            }
        }
    }
}

#[cfg(test)]
mod picker_tests {
    use super::UefiSecureBootTemplate;
    use super::UefiTemplateArch;
    use super::UefiTemplateGuest;

    #[test]
    fn picks_supported_templates() {
        assert!(matches!(
            UefiSecureBootTemplate::pick(
                UefiTemplateGuest::MicrosoftWindows,
                UefiTemplateArch::X64,
            ),
            Some(UefiSecureBootTemplate::MicrosoftWindowsX64)
        ));
        assert!(matches!(
            UefiSecureBootTemplate::pick(
                UefiTemplateGuest::MicrosoftWindows,
                UefiTemplateArch::Aarch64,
            ),
            Some(UefiSecureBootTemplate::MicrosoftWindowsAarch64)
        ));
        assert!(matches!(
            UefiSecureBootTemplate::pick(UefiTemplateGuest::MicrosoftUefiCa, UefiTemplateArch::X64,),
            Some(UefiSecureBootTemplate::MicrosoftUefiCaX64)
        ));
        assert!(matches!(
            UefiSecureBootTemplate::pick(
                UefiTemplateGuest::MicrosoftUefiCa,
                UefiTemplateArch::Aarch64,
            ),
            Some(UefiSecureBootTemplate::MicrosoftUefiCaAarch64)
        ));
    }

    #[test]
    fn no_guest_has_no_template() {
        assert!(
            UefiSecureBootTemplate::pick(UefiTemplateGuest::None, UefiTemplateArch::X64).is_none()
        );
    }
}

macro_rules! include_templates {
    (
        $(($fn_name:ident, $path:literal),)*
    ) => {
        $(
            pub fn $fn_name() -> firmware_uefi_custom_vars::BaseTemplateVars {
                // DEVNOTE: in the future, it may be interesting to explore
                // parsing the JSON at compile time, and then "baking" the
                // parsed templates into the binary as a `const` value, instead
                // of baking in the JSON and doing this extra "useless" parsing
                // + validation at runtime.
                //
                // While it's unlikely this would save all that much code space
                // in the final bin (given that much of the parsing + validation
                // code is shared between both templates and user custom uefi
                // JSON files), it may result in a nice .rodata size decrease.
                hyperv_uefi_custom_vars_json::load_template_from_json(include_bytes!(concat!(env!("OUT_DIR"), "/", $path))).unwrap().into()
            }
        )*

        #[cfg(test)]
        mod test {
            $(
                #[test]
                fn $fn_name() {
                    super::$fn_name();
                }
            )*
        }

    };
}

pub mod aarch64 {
    include_templates! {
        (microsoft_windows, "aarch64/MicrosoftWindows_Template.json"),
        (microsoft_uefi_ca, "aarch64/MicrosoftUEFI_Template.json"),
    }
}

pub mod x64 {
    include_templates! {
        (microsoft_windows, "x64/MicrosoftWindows_Template.json"),
        (microsoft_uefi_ca, "x64/MicrosoftUEFI_Template.json"),
    }
}
