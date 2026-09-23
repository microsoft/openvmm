// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Procedural macros for TMK tests.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use quote::ToTokens;
use quote::quote;
use syn::parse::Parser;

/// `tmk_test` procedural attribute macro.
///
/// This macro is used to define a test in the TMK.
#[proc_macro_attribute]
pub fn tmk_test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = syn::parse_macro_input!(item as syn::ItemFn);
    let name = item.sig.ident.to_string();
    let func = &item.sig.ident;

    let mut expected_failure = false;
    let mut linux_only = false;

    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("expected_failure") {
            expected_failure = true;
            Ok(())
        } else if meta.path.is_ident("linux_only") {
            linux_only = true;
            Ok(())
        } else {
            let ident = meta
                .path
                .get_ident()
                .map_or_else(|| "<unknown>".to_string(), |exp| exp.to_string());

            Err(meta.error(format!("unsupported tmk_test option: {ident}")))
        }
    });

    if let Err(err) = parser.parse(attr) {
        let compile_err = err.to_compile_error();
        return quote! {
            #compile_err
            #item
        }
        .into();
    }

    let flags = quote! {
        ::tmk_protocol::TestFlags64::new()
            .with_expected_failure(#expected_failure)
            .with_linux_only(#linux_only)
    };

    quote! {
        ::tmk_core::define_tmk_test!(#name, #func, #flags);
        #item
    }
    .into_token_stream()
    .into()
}
