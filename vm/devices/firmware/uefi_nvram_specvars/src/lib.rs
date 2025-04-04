// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//!  UEFI NVRAM structures.

#![expect(missing_docs)]

use thiserror::Error;

pub mod boot_order;
pub mod signature_list;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("parsing boot order")]
    BootOrder(#[from] boot_order::Error),
    #[error("parsing signature list")]
    SignatureList(#[from] signature_list::ParseError),
}

#[derive(Debug)]
pub enum ParsedNvramEntry<'a> {
    BootOrder(Vec<u16>),
    Boot(boot_order::EfiLoadOption<'a>),
    SignatureList(Vec<signature_list::SignatureList<'a>>),
    Unknown(&'a [u8]),
}

pub fn parse_nvram_entry<'a>(
    name: &'a str,
    data: &'a [u8],
) -> Result<ParsedNvramEntry<'a>, ParseError> {
    Ok(match name {
        "BootOrder" => ParsedNvramEntry::BootOrder(boot_order::parse_boot_order(data)?.collect()),
        _ if name
            .strip_prefix("Boot")
            .map(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false) =>
        {
            ParsedNvramEntry::Boot(boot_order::EfiLoadOption::parse(data)?)
        }
        "KEK" | "db" | "dbx" | "PK" | "dbDefault" | "MokList" | "MokListX" => {
            ParsedNvramEntry::SignatureList(
                signature_list::ParseSignatureLists::new(data)
                    .collect_signature_lists(|_, _| true)?,
            )
        }
        _ => ParsedNvramEntry::Unknown(data),
    })
}
