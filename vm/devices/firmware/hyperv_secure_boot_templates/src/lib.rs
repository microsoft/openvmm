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

use firmware_uefi_custom_vars::BaseTemplateIdentity;

// Keep these synchronized with the corresponding OS `TemplateRecipes.xml`
// entries. `ConvertVariables.ps1 -TemplateName ...` prints the GUID and version
// declarations when template data is regenerated.
pub const MICROSOFT_WINDOWS_IDENTITY: BaseTemplateIdentity = BaseTemplateIdentity {
    guid: guid::guid!("1734c6e8-3154-4dda-ba5f-a874cc483422"),
    version: 3,
};

pub const MICROSOFT_UEFI_CA_IDENTITY: BaseTemplateIdentity = BaseTemplateIdentity {
    guid: guid::guid!("272e7447-90a4-4563-a4b9-8e4ab00526ce"),
    version: 3,
};

macro_rules! include_templates {
    (
        $(($fn_name:ident, $path:literal, $identity:expr),)*
    ) => {
        $(
            pub fn $fn_name() -> firmware_uefi_custom_vars::BaseTemplate {
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
                firmware_uefi_custom_vars::BaseTemplate {
                    json: include_bytes!(concat!(env!("OUT_DIR"), "/", $path)).to_vec().into(),
                    identity: $identity,
                }
            }
        )*

        #[cfg(test)]
        mod test {
            $(
                #[test]
                fn $fn_name() {
                    let template = super::$fn_name();
                    assert_eq!(template.identity, $identity);
                    hyperv_uefi_custom_vars_json::parse_template_json(
                        template.json.as_bytes()
                    ).unwrap();
                }
            )*
        }

    };
}

pub mod aarch64 {
    include_templates! {
        (microsoft_windows, "aarch64/MicrosoftWindows_Template.json", crate::MICROSOFT_WINDOWS_IDENTITY),
        (microsoft_uefi_ca, "aarch64/MicrosoftUEFI_Template.json", crate::MICROSOFT_UEFI_CA_IDENTITY),
    }
}

pub mod x64 {
    include_templates! {
        (microsoft_windows, "x64/MicrosoftWindows_Template.json", crate::MICROSOFT_WINDOWS_IDENTITY),
        (microsoft_uefi_ca, "x64/MicrosoftUEFI_Template.json", crate::MICROSOFT_UEFI_CA_IDENTITY),
    }
}
