// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Derive macros for the `vmm_cli` crate. See that crate for documentation.

#![forbid(unsafe_code)]

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::format_ident;
use quote::quote;
use syn::Data;
use syn::DeriveInput;
use syn::Expr;
use syn::Fields;
use syn::LitStr;
use syn::Token;
use syn::Type;
use syn::Variant;
use syn::parse_macro_input;

/// Derive a `FromStr` impl that parses a comma-separated `key=value` option
/// string into this struct. Documented in the `vmm_cli` crate.
#[proc_macro_derive(KeyValueArgs, attributes(kv))]
pub fn derive_key_value_args(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_struct(input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Derive a `vmm_cli::KeyValueGroup` impl for an enum whose variants each
/// correspond to a mutually-exclusive key. Documented in the `vmm_cli` crate.
#[proc_macro_derive(KeyValueGroup, attributes(kv))]
pub fn derive_key_value_group(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_group(input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// How a scalar field is populated when its key is absent.
#[derive(Default)]
enum DefaultKind {
    /// No default: the key is required.
    #[default]
    None,
    /// `#[kv(default)]`: use `Default::default()`.
    Auto,
    /// `#[kv(default = expr)]`: use `expr`.
    Expr(Expr),
}

#[derive(Default)]
struct FieldAttr {
    key: Option<String>,
    flag: bool,
    flatten: bool,
    positional: bool,
    default: DefaultKind,
    present: Option<Expr>,
    absent: Option<Expr>,
}

fn parse_field_attr(field: &syn::Field) -> syn::Result<FieldAttr> {
    let mut fa = FieldAttr::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("kv") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("key") {
                fa.key = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("flag") {
                fa.flag = true;
            } else if meta.path.is_ident("flatten") {
                fa.flatten = true;
            } else if meta.path.is_ident("positional") {
                fa.positional = true;
            } else if meta.path.is_ident("default") {
                if meta.input.peek(Token![=]) {
                    fa.default = DefaultKind::Expr(meta.value()?.parse()?);
                } else {
                    fa.default = DefaultKind::Auto;
                }
            } else if meta.path.is_ident("present") {
                fa.present = Some(meta.value()?.parse()?);
            } else if meta.path.is_ident("absent") {
                fa.absent = Some(meta.value()?.parse()?);
            } else {
                return Err(meta.error("unknown `kv` attribute"));
            }
            Ok(())
        })?;
    }
    Ok(fa)
}

/// If `ty` is `Option<T>`, return `T`.
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(tp) = ty else { return None };
    if tp.qself.is_some() {
        return None;
    }
    let seg = tp.path.segments.last()?;
    if seg.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}

/// Whether `ty` is exactly `bool`.
fn is_bool(ty: &Type) -> bool {
    matches!(ty, Type::Path(tp) if tp.qself.is_none() && tp.path.is_ident("bool"))
}

fn validate_field_attr(field: &syn::Field, fa: &FieldAttr) -> syn::Result<()> {
    if !fa.flag && (fa.present.is_some() || fa.absent.is_some()) {
        return Err(syn::Error::new_spanned(
            field,
            "`present`/`absent` require `flag`",
        ));
    }
    if fa.flatten
        && (fa.key.is_some()
            || fa.flag
            || !matches!(fa.default, DefaultKind::None)
            || fa.present.is_some()
            || fa.absent.is_some())
    {
        return Err(syn::Error::new_spanned(
            field,
            "`flatten` cannot be combined with `key`, `flag`, `default`, `present`, or `absent`",
        ));
    }
    if fa.positional
        && (fa.key.is_some()
            || fa.flag
            || fa.flatten
            || !matches!(fa.default, DefaultKind::None)
            || fa.present.is_some()
            || fa.absent.is_some())
    {
        return Err(syn::Error::new_spanned(
            field,
            "`positional` cannot be combined with `key`, `flag`, `flatten`, `default`, `present`, or `absent`",
        ));
    }
    if fa.flag && !matches!(fa.default, DefaultKind::None) {
        return Err(syn::Error::new_spanned(
            field,
            "`default` is not allowed on a `flag` field",
        ));
    }
    if !fa.flag
        && !fa.flatten
        && !fa.positional
        && option_inner(&field.ty).is_some()
        && !matches!(fa.default, DefaultKind::None)
    {
        return Err(syn::Error::new_spanned(
            field,
            "`default` is not allowed on an `Option<T>` field",
        ));
    }
    Ok(())
}

fn expand_struct(input: DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let vis = &input.vis;
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input,
            "KeyValueArgs may only be derived for structs",
        ));
    };
    let Fields::Named(named) = &data.fields else {
        return Err(syn::Error::new_spanned(
            &input,
            "KeyValueArgs requires named fields",
        ));
    };

    let accum_name = format_ident!("__{}Accum", name);

    let mut accum_fields = Vec::new();
    let mut accept_arms = Vec::new();
    let mut flatten_accepts = Vec::new();
    let mut append_keys = Vec::new();
    let mut finish_fields = Vec::new();
    let mut positional: Option<(syn::Ident, Type, String)> = None;

    for field in &named.named {
        let fname = field.ident.as_ref().unwrap();
        let ty = &field.ty;
        let fa = parse_field_attr(field)?;
        validate_field_attr(field, &fa)?;
        let key = fa.key.clone().unwrap_or_else(|| fname.to_string());

        if fa.positional {
            if positional.is_some() {
                return Err(syn::Error::new_spanned(
                    field,
                    "only one `positional` field is allowed",
                ));
            }
            accum_fields.push(quote! { #fname: ::core::option::Option<#ty>, });
            let pname = fname.to_string();
            finish_fields.push(quote! {
                #fname: __a.#fname.ok_or_else(
                    || ::vmm_cli::private::error(::std::format!("missing required '{}'", #pname)),
                )?,
            });
            positional = Some((fname.clone(), ty.clone(), pname));
        } else if fa.flatten {
            accum_fields.push(quote! { #fname: <#ty as ::vmm_cli::KeyValueFields>::Accum, });
            append_keys.push(quote! {
                <#ty as ::vmm_cli::KeyValueFields>::append_keys(__keys);
            });
            flatten_accepts.push(quote! {
                if <#ty as ::vmm_cli::KeyValueFields>::accept(&mut __a.#fname, __key, __value)? {
                    return ::core::result::Result::Ok(true);
                }
            });
            finish_fields.push(quote! {
                #fname: <#ty as ::vmm_cli::KeyValueFields>::finish(__a.#fname)?,
            });
        } else if fa.flag {
            append_keys.push(quote! { __keys.push(#key); });
            accum_fields.push(quote! { #fname: ::core::option::Option<bool>, });
            accept_arms.push(quote! {
                #key => { ::vmm_cli::private::set_toggle(&mut __a.#fname, #key, __value)?; }
            });
            if option_inner(ty).is_some_and(is_bool) {
                if fa.present.is_some() || fa.absent.is_some() {
                    return Err(syn::Error::new_spanned(
                        field,
                        "`present`/`absent` are not allowed on an `Option<bool>` flag",
                    ));
                }
                finish_fields.push(quote! { #fname: __a.#fname, });
            } else {
                let (present, absent) = match (&fa.present, &fa.absent) {
                    (Some(p), Some(a)) => (quote!(#p), quote!(#a)),
                    (None, None) => (quote!(true), quote!(false)),
                    _ => {
                        return Err(syn::Error::new_spanned(
                            field,
                            "`flag` requires both `present` and `absent` (for non-bool fields), or neither (for bool)",
                        ));
                    }
                };
                finish_fields.push(quote! {
                    #fname: if __a.#fname.unwrap_or(false) { #present } else { #absent },
                });
            }
        } else if let Some(inner) = option_inner(ty) {
            append_keys.push(quote! { __keys.push(#key); });
            accum_fields.push(quote! { #fname: #ty, });
            accept_arms.push(quote! {
                #key => {
                    ::vmm_cli::private::set_once(&mut __a.#fname, #key, ::vmm_cli::private::parse_value::<#inner>(#key, __value)?)?;
                }
            });
            finish_fields.push(quote! { #fname: __a.#fname, });
        } else {
            append_keys.push(quote! { __keys.push(#key); });
            accum_fields.push(quote! { #fname: ::core::option::Option<#ty>, });
            accept_arms.push(quote! {
                #key => {
                    ::vmm_cli::private::set_once(&mut __a.#fname, #key, ::vmm_cli::private::parse_value::<#ty>(#key, __value)?)?;
                }
            });
            match &fa.default {
                DefaultKind::Expr(expr) => {
                    finish_fields.push(quote! { #fname: __a.#fname.unwrap_or_else(|| #expr), });
                }
                DefaultKind::Auto => {
                    finish_fields.push(quote! { #fname: __a.#fname.unwrap_or_default(), });
                }
                DefaultKind::None => finish_fields.push(quote! {
                    #fname: __a.#fname.ok_or_else(
                        || ::vmm_cli::private::error(::std::format!("missing required option '{}'", #key)),
                    )?,
                }),
            }
        }
    }

    // Body of `KeyValueFields::accept`.
    let accept_body = if accept_arms.is_empty() && flatten_accepts.is_empty() {
        quote! {
            let _ = (&mut *__a, __key, __value);
            ::core::result::Result::Ok(false)
        }
    } else {
        quote! {
            match __key {
                #(#accept_arms)*
                _ => {
                    #(#flatten_accepts)*
                    return ::core::result::Result::Ok(false);
                }
            }
            ::core::result::Result::Ok(true)
        }
    };

    // Body of `FromStr::from_str`: seed the accumulator, consume the leading
    // positional (if any), feed the rest through `accept`, then `finish`.
    let parse_loop = if let Some((pf, pty, pname)) = &positional {
        quote! {
            let mut __parts = ::vmm_cli::private::split_options(s)?.into_iter();
            __a.#pf = ::core::option::Option::Some(
                ::vmm_cli::private::parse_positional::<#pty>(#pname, __parts.next())?,
            );
            for __part in __parts {
                let (__key, __value) = ::vmm_cli::private::split_kv(__part)?;
                if !<#name as ::vmm_cli::KeyValueFields>::accept(&mut __a, __key, __value)? {
                    return ::core::result::Result::Err(::vmm_cli::private::unknown_option::<#name>(__key));
                }
            }
        }
    } else {
        quote! {
            for __part in ::vmm_cli::private::split_options(s)? {
                let (__key, __value) = ::vmm_cli::private::split_kv(__part)?;
                if !<#name as ::vmm_cli::KeyValueFields>::accept(&mut __a, __key, __value)? {
                    return ::core::result::Result::Err(::vmm_cli::private::unknown_option::<#name>(__key));
                }
            }
        }
    };

    Ok(quote! {
        // The accumulator inherits the parsed type's visibility (`#vis`) rather
        // than being private. It is the value of the public
        // `KeyValueFields::Accum` associated type, so for a `pub` parsed type it
        // must be at least as visible as that type; making it private trips
        // E0446 ("private type in public interface"). It also has to be nameable
        // by other types that `#[kv(flatten)]` this one, potentially across
        // crates. `#[doc(hidden)]` keeps it out of rustdoc regardless.
        #[doc(hidden)]
        #[derive(::core::default::Default)]
        #vis struct #accum_name {
            #(#accum_fields)*
        }

        impl ::vmm_cli::KeyValueFields for #name {
            type Accum = #accum_name;

            fn append_keys(__keys: &mut ::std::vec::Vec<&'static str>) {
                #(#append_keys)*
            }

            fn accept(
                __a: &mut Self::Accum,
                __key: &str,
                __value: ::core::option::Option<&str>,
            ) -> ::vmm_cli::Result<bool> {
                #accept_body
            }

            fn finish(__a: Self::Accum) -> ::vmm_cli::Result<Self> {
                ::core::result::Result::Ok(Self {
                    #(#finish_fields)*
                })
            }
        }

        impl ::core::str::FromStr for #name {
            type Err = ::vmm_cli::Error;

            fn from_str(s: &str) -> ::vmm_cli::Result<Self> {
                let mut __a = <#name as ::vmm_cli::KeyValueFields>::Accum::default();
                #parse_loop
                <#name as ::vmm_cli::KeyValueFields>::finish(__a)
            }
        }
    })
}

fn parse_variant_attr(variant: &Variant) -> syn::Result<(String, bool)> {
    let mut key = None;
    let mut is_default = false;
    for attr in &variant.attrs {
        if !attr.path().is_ident("kv") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("key") {
                key = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("default") {
                is_default = true;
            } else {
                return Err(meta.error("unknown `kv` attribute"));
            }
            Ok(())
        })?;
    }
    let key = key.unwrap_or_else(|| variant.ident.to_string().to_snake_case());
    Ok((key, is_default))
}

fn expand_group(input: DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input,
            "KeyValueGroup may only be derived for enums",
        ));
    };

    let mut arms = Vec::new();
    let mut keys = Vec::new();
    let mut append_keys = Vec::new();
    let mut default_variant = None;

    for variant in &data.variants {
        let vname = &variant.ident;
        let (key, is_default) = parse_variant_attr(variant)?;
        keys.push(format!("'{key}'"));
        append_keys.push(quote! { __keys.push(#key); });

        let build = match &variant.fields {
            Fields::Unit => quote! {
                {
                    if __value.is_some() {
                        return ::core::result::Result::Err(::vmm_cli::private::error(
                            ::std::format!("flag '{}' does not take a value", #key),
                        ));
                    }
                    #name::#vname
                }
            },
            Fields::Unnamed(f) if f.unnamed.len() == 1 => {
                let fty = &f.unnamed.first().unwrap().ty;
                if let Some(inner) = option_inner(fty) {
                    quote! { #name::#vname(::vmm_cli::private::parse_opt_value::<#inner>(#key, __value)?) }
                } else {
                    quote! { #name::#vname(::vmm_cli::private::parse_value::<#fty>(#key, __value)?) }
                }
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    variant,
                    "KeyValueGroup variants must be a unit or a single-field tuple",
                ));
            }
        };
        arms.push(quote! { #key => #build, });
        if is_default {
            if !matches!(variant.fields, Fields::Unit) {
                return Err(syn::Error::new_spanned(
                    variant,
                    "`#[kv(default)]` is only allowed on unit variants",
                ));
            }
            if default_variant.is_some() {
                return Err(syn::Error::new_spanned(
                    variant,
                    "only one variant may be marked `#[kv(default)]`",
                ));
            }
            default_variant = Some(quote! { #name::#vname });
        }
    }

    let keys_desc = keys.join(", ");
    let none_case = match &default_variant {
        Some(def) => quote! { ::core::result::Result::Ok(#def) },
        None => quote! {
            ::core::result::Result::Err(::vmm_cli::private::error(
                ::std::format!("one of {} is required", #keys_desc),
            ))
        },
    };

    Ok(quote! {
        impl ::vmm_cli::KeyValueFields for #name {
            type Accum = ::core::option::Option<#name>;

            fn append_keys(__keys: &mut ::std::vec::Vec<&'static str>) {
                #(#append_keys)*
            }

            fn accept(
                __accum: &mut Self::Accum,
                __key: &str,
                __value: ::core::option::Option<&str>,
            ) -> ::vmm_cli::Result<bool> {
                let __v = match __key {
                    #(#arms)*
                    _ => return ::core::result::Result::Ok(false),
                };
                if __accum.is_some() {
                    return ::core::result::Result::Err(::vmm_cli::private::error(
                        ::std::format!("only one of {} may be specified", #keys_desc),
                    ));
                }
                *__accum = ::core::option::Option::Some(__v);
                ::core::result::Result::Ok(true)
            }

            fn finish(__accum: Self::Accum) -> ::vmm_cli::Result<Self> {
                match __accum {
                    ::core::option::Option::Some(__v) => ::core::result::Result::Ok(__v),
                    ::core::option::Option::None => { #none_case }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn expansion_error(input: DeriveInput) -> String {
        expand_struct(input).unwrap_err().to_string()
    }

    #[test]
    fn rejects_attributes_without_meaning_for_field_kind() {
        assert_eq!(
            expansion_error(parse_quote! {
                struct Args {
                    #[kv(present = 1, absent = 0)]
                    value: u8,
                }
            }),
            "`present`/`absent` require `flag`"
        );
        assert_eq!(
            expansion_error(parse_quote! {
                struct Args {
                    #[kv(default)]
                    value: Option<u8>,
                }
            }),
            "`default` is not allowed on an `Option<T>` field"
        );
        assert_eq!(
            expansion_error(parse_quote! {
                struct Args {
                    #[kv(flag, default)]
                    value: bool,
                }
            }),
            "`default` is not allowed on a `flag` field"
        );
        assert_eq!(
            expansion_error(parse_quote! {
                struct Args {
                    #[kv(flatten, key = "value")]
                    value: Nested,
                }
            }),
            "`flatten` cannot be combined with `key`, `flag`, `default`, `present`, or `absent`"
        );
        assert_eq!(
            expansion_error(parse_quote! {
                struct Args {
                    #[kv(positional, key = "value")]
                    value: String,
                }
            }),
            "`positional` cannot be combined with `key`, `flag`, `flatten`, `default`, `present`, or `absent`"
        );
    }
}
