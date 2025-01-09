// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Protobuf service support for mesh types.

pub use grpc::Code;
pub use grpc::Status;
use mesh::local_node::Port;
use mesh::payload::encoding::MessageEncoding;
use mesh::payload::protobuf::FieldSizer;
use mesh::payload::protobuf::FieldWriter;
use mesh::payload::protobuf::MessageReader;
use mesh::payload::protobuf::MessageSizer;
use mesh::payload::protobuf::MessageWriter;
use mesh::payload::DefaultEncoding;
use mesh::payload::MessageDecode;
use mesh::payload::MessageEncode;
use mesh::payload::Result;
use mesh::resource::Resource;

mod grpc {
    // Generated types use these crates, reference them here to ensure they are
    // not removed by automated tooling.
    use prost as _;
    use prost_types as _;

    include!(concat!(env!("OUT_DIR"), "/google.rpc.rs"));

    impl std::fmt::Display for Code {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:?}", self)
        }
    }

    impl std::error::Error for Code {}
}

/// A generic RPC value.
///
/// This type is designed to have the same encoding as [`DecodedRpc`].
#[derive(mesh::MeshPayload)]
pub(crate) struct GenericRpc {
    #[mesh(1)]
    pub method: String,
    #[mesh(2)]
    pub data: Vec<u8>,
    #[mesh(3)]
    pub port: Port, // TODO: transparent mesh::OneshotSender<std::result::Result<Vec<u8>, Status>>,
}

impl GenericRpc {
    pub(crate) fn respond_status(self, status: Status) {
        let sender =
            mesh::OneshotSender::<std::result::Result<std::convert::Infallible, Status>>::from(
                self.port,
            );

        sender.send(Err(status));
    }
}

/// A generic RPC value, using borrows instead of owning types.
#[derive(mesh::MeshPayload)]
struct GenericRpcView<'a> {
    #[mesh(1)]
    method: &'a str,
    #[mesh(2)]
    data: &'a [u8],
    #[mesh(3)]
    port: Port,
}

/// Trait for service-specific RPC requests.
pub trait ServiceRpc: 'static + Send + Sized {
    /// The service name.
    const NAME: &'static str;

    /// The method name.
    fn method(&self) -> &'static str;

    /// Encode the request into a field.
    fn encode(self, writer: FieldWriter<'_, '_, Resource>) -> Port;

    /// Compute the field size of the request.
    fn compute_size(&mut self, sizer: FieldSizer<'_>);

    /// Decode the request from a field.
    fn decode(
        method: &str,
        port: Port,
        data: &[u8],
    ) -> std::result::Result<Self, (ServiceRpcError, Port)>;
}

/// An error returned while decoding a method call.
pub enum ServiceRpcError {
    /// The method is unknown.
    UnknownMethod,
    /// The input could not be decoded.
    InvalidInput(mesh::payload::Error),
}

#[doc(hidden)]
pub(crate) enum DecodedRpc<T> {
    Rpc(T),
    Err {
        rpc: GenericRpc,
        err: ServiceRpcError,
    },
}

pub(crate) struct DecodedRpcEncoder;

impl<T: ServiceRpc> DefaultEncoding for DecodedRpc<T> {
    type Encoding = MessageEncoding<DecodedRpcEncoder>;
}

impl<T: ServiceRpc> MessageEncode<DecodedRpc<T>, Resource> for DecodedRpcEncoder {
    fn write_message(item: DecodedRpc<T>, mut writer: MessageWriter<'_, '_, Resource>) {
        match item {
            DecodedRpc::Rpc(rpc) => {
                writer.field(1).bytes(rpc.method().as_bytes());
                let port = rpc.encode(writer.field(2));
                writer.field(3).resource(Resource::Port(port));
            }
            DecodedRpc::Err { rpc, err: _ } => {
                <GenericRpc as DefaultEncoding>::Encoding::write_message(rpc, writer)
            }
        }
    }

    fn compute_message_size(item: &mut DecodedRpc<T>, mut sizer: MessageSizer<'_>) {
        match item {
            DecodedRpc::Rpc(rpc) => {
                sizer.field(1).bytes(rpc.method().len());
                rpc.compute_size(sizer.field(2));
                sizer.field(3).resource();
            }
            DecodedRpc::Err { rpc, err: _ } => {
                <GenericRpc as DefaultEncoding>::Encoding::compute_message_size(rpc, sizer)
            }
        }
    }
}

impl<'a, T: ServiceRpc> MessageDecode<'a, DecodedRpc<T>, Resource> for DecodedRpcEncoder {
    fn read_message(
        item: &mut mesh::payload::inplace::InplaceOption<'_, DecodedRpc<T>>,
        reader: MessageReader<'a, '_, Resource>,
    ) -> Result<()> {
        mesh::payload::inplace_none!(v: GenericRpcView<'_>);
        <GenericRpcView<'_> as DefaultEncoding>::Encoding::read_message(&mut v, reader)?;
        let v = v.take().expect("should be constructed");
        let rpc = match T::decode(v.method, v.port, v.data) {
            Ok(rpc) => DecodedRpc::Rpc(rpc),
            Err((err, port)) => {
                let rpc = GenericRpc {
                    method: v.method.to_string(),
                    data: v.data.to_vec(),
                    port,
                };
                DecodedRpc::Err { rpc, err }
            }
        };
        item.set(rpc);
        Ok(())
    }
}
