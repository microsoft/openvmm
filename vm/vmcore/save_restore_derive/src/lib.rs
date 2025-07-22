// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

use quote::quote;
use syn::DeriveInput;
use syn::parse_macro_input;

// Documented in the save_restore module.
#[proc_macro_derive(SavedStateRoot)]
pub fn derive_saved_state_root(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = &input.ident;
    quote! {
        ::vmcore::declare_saved_state_root!(#ident);
    }
    .into()
}
