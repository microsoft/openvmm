// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Nvram types used by linux distros

pub mod vars {
    use guid::Guid;

    const EFI_IMAGE_SECURITY_MOK_DATABASE_GUID: Guid =
        Guid::from_static_str("605dab50-e046-4300-abb6-3dd810dd8b23");

    defn_nvram_var!(MOK_LIST = (EFI_IMAGE_SECURITY_MOK_DATABASE_GUID, "MokList"));
    defn_nvram_var!(MOK_LISTX = (EFI_IMAGE_SECURITY_MOK_DATABASE_GUID, "MokListX"));
}
