// Copyright (C) Microsoft Corporation. All rights reserved.

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

            impl ::mesh::payload::MessageEncode<#ident, ::mesh::resource::Resource> for ::mesh::payload::encoding::DerivedEncoding<#ident> {
                fn write_message(item: #ident, mut writer: ::mesh::payload::protobuf::MessageWriter<'_, '_, ::mesh::resource::Resource>) {
                    let (method, port) = match item {
                        #(
                            #ident::#method_idents(req, port) => {
                                writer.field(2).message(|message| {
                                    <#request_types as ::mesh::payload::DefaultEncoding>::Encoding::write_message(req, message);
                                });
                                (#method_names, port.force_downcast())
                            }
                        )*
                    };
                    ::mesh_rpc::service::write_rpc_message(writer, method, port);
                }

                fn compute_message_size(item: &mut #ident, mut sizer: ::mesh::payload::protobuf::MessageSizer<'_>) {
                    let method = match item {
                        #(
                            #ident::#method_idents(req, _) => {
                                sizer.field(2).message(|message| {
                                    <<#request_types as ::mesh::payload::DefaultEncoding>::Encoding as ::mesh::payload::MessageEncode::<_, ::mesh::resource::Resource>>::compute_message_size(
                                        req,
                                        message);
                                });
                                #method_names
                            }
                        )*
                    };
                    ::mesh_rpc::service::compute_size_rpc_message(sizer, method);
                }
            }

            impl<'encoding> ::mesh::payload::MessageDecode<'encoding, #ident, ::mesh::resource::Resource> for ::mesh::payload::encoding::DerivedEncoding<#ident> {
                fn read_message(
                    item: &mut ::mesh::payload::inplace::InplaceOption<'_, #ident>,
                    reader: ::mesh::payload::protobuf::MessageReader<'encoding, '_, ::mesh::resource::Resource>,
                ) -> ::mesh::payload::Result<()> {
                    use ::mesh::payload::ResultExt;
                    let (method, data, port) = ::mesh_rpc::service::read_rpc_message(reader).typed::<#ident>()?;
                    item.set(match method {
                        #(
                            #method_names => {
                                #ident::#method_idents(mesh::payload::decode(data)?, port.upcast())
                            }
                        )*
                        _ => return Err(mesh::payload::Error::new(mesh_rpc::service::UnknownMethod(method.to_string())).typed::<#ident>()),
                    });
                    Ok(())
                }
            }

            impl ::mesh::payload::DefaultEncoding for #ident {
                type Encoding = ::mesh::payload::encoding::MessageEncoding<mesh::payload::encoding::DerivedEncoding<Self>>;
            }

            impl ::mesh_rpc::service::ServiceRpc for #ident {
                const NAME: &'static str = #name;
            }

            impl ::mesh::payload::Downcast<#ident> for #ident {}
            impl ::mesh::payload::Downcast<#ident> for ::mesh_rpc::service::GenericRpc {}
        }
        .to_string();
    }
}
