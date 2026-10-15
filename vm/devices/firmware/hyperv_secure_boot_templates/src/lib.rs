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

macro_rules! include_templates {
    (
        $(($fn_name:ident, $path:literal),)*
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
                }
            }
        )*

        #[cfg(test)]
        mod test {
            $(
                #[test]
                fn $fn_name() {
                    let template = super::$fn_name();
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
