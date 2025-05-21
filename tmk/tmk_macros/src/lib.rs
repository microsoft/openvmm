// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Procedural macros for TMK tests.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use quote::ToTokens;
use quote::quote;

/// `tmk_test` procedural attribute macro.
///
/// This macro is used to define a test in the TMK.
#[proc_macro_attribute]
pub fn tmk_test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = syn::parse_macro_input!(item as syn::ItemFn);
    let name = item.sig.ident.to_string();
    let func = &item.sig.ident;
    quote! {
        ::tmk_core::define_tmk_test!(#name, #func);
        #item
    }
    .into_token_stream()
    .into()
}
