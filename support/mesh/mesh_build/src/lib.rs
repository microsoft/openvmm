// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A code generator for protobuf service definitions.
//!
//! Used with the prost protobuf code generator.

use heck::ToUpperCamelCase;
use proc_macro2::Span;
use proc_macro2::TokenStream;
use syn::Ident;

/// A service generator for mesh services.
pub struct MeshServiceGenerator;

impl prost_build::ServiceGenerator for MeshServiceGenerator {
    fn generate(&mut self, service: prost_build::Service, buf: &mut String) {
        let name = format!("{}.{}", service.package, service.proto_name);
        let ident = Ident::new(&service.name, Span::call_site());
        let method_names: Vec<_> = service.methods.iter().map(|m| &m.proto_name).collect();
        let method_idents: Vec<_> = service
            .methods
            .iter()
            .map(|m| Ident::new(&m.name.to_upper_camel_case(), Span::call_site()))
            .collect();
        let request_types: Vec<TokenStream> = service
            .methods
            .iter()
            .map(|m| m.input_type.parse().unwrap())
            .collect();
        let response_types: Vec<TokenStream> = service
            .methods
            .iter()
            .map(|m| m.output_type.parse().unwrap())
            .collect();

        *buf += &quote::quote! {
            #[derive(Debug)]
            pub enum #ident {
                #(
                    #method_idents(
                        #request_types,
                        ::mesh::OneshotSender<::core::result::Result<#response_types, ::mesh_rpc::service::Status>>,
                    ),
                )*
            }

            impl #ident {
                #[allow(dead_code)]
                pub fn fail(self, status: ::mesh_rpc::service::Status) {
                    match self {
                        #(
                            #ident::#method_idents(_, response) => response.send(Err(status)),
                        )*
                    }
                }
            }

            impl ::mesh_rpc::service::ServiceRpc for #ident {
                const NAME: &'static str = #name;

                fn method(&self) -> &'static str {
                    match self {
                        #(
                            #ident::#method_idents(_, _) => #method_names,
                        )*
                    }
                }

                fn encode(
                    self,
                    writer: ::mesh::payload::protobuf::FieldWriter<'_, '_, ::mesh::resource::Resource>,
                ) -> ::mesh::local_node::Port {
                    match self {
                        #(
                            #ident::#method_idents(req, port) => {
                                <<#request_types as ::mesh::payload::DefaultEncoding>::Encoding as ::mesh::payload::FieldEncode<_, _>>::write_field(req, writer);
                                port.into()
                            }
                        )*
                    }
                }

                fn compute_size(&mut self, sizer: ::mesh::payload::protobuf::FieldSizer<'_>) {
                    match self {
                        #(
                            #ident::#method_idents(req, _) => {
                                <<#request_types as ::mesh::payload::DefaultEncoding>::Encoding as ::mesh::payload::FieldEncode::<_, ::mesh::resource::Resource>>::compute_field_size(
                                    req,
                                    sizer);
                            }
                        )*
                    }
                }

                fn decode(
                    method: &str,
                    port: ::mesh::local_node::Port,
                    data: &[u8],
                ) -> Result<Self, (::mesh_rpc::service::ServiceRpcError, ::mesh::local_node::Port)> {
                    match method {
                        #(
                            #method_names => {
                                match mesh::payload::decode(data) {
                                    Ok(req) => Ok(#ident::#method_idents(req, port.into())),
                                    Err(e) => Err((::mesh_rpc::service::ServiceRpcError::InvalidInput(e), port)),
                                }
                            }
                        )*
                        _ => Err((::mesh_rpc::service::ServiceRpcError::UnknownMethod, port)),
                    }
                }
            }
        }
        .to_string();
    }
}
