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
pub fn tmk_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = syn::parse_macro_input!(item as syn::ItemFn);
    let name = item.sig.ident.to_string();
    let func = &item.sig.ident;

    let flags = match attr.to_string().as_str() {
        "" => quote! { ::tmk_protocol::TestFlags64::new() },
        "expected_failure" => {
            quote! { ::tmk_protocol::TestFlags64::new().with_expected_failure(true) }
        }
        attr => {
            let msg = format!("unsupported tmk_test option: {attr}");
            return quote! {
                compile_error!(#msg);
                #item
            }
            .into_token_stream()
            .into();
        }
    };

    quote! {
        ::tmk_core::define_tmk_test!(#name, #func, #flags);
        #item
    }
    .into_token_stream()
    .into()
}
