// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for deferring inspection requests.

use super::InspectMut;
use super::InternalNode;
use super::Request;
use super::Response;
use super::SensitivityLevel;
use super::UpdateRequest;
use crate::Inspect;
use crate::NumberFormat;
use crate::RequestParams;
use crate::RootParams;
use crate::ValueKind;
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::string::String;
use mesh::MeshPayload;

/// An [`Inspect`] implementation that defers the inspection to another
/// thread or context by sending the request on a [`mesh`] channel.
///
/// # Usage
///
/// ```rust
/// enum MyRpc {
///     Inspect(inspect::Deferred),
///     SomeWork(mesh::rpc::Rpc<u32, u32>),
/// }
///
/// fn inspect_remote(sender: &mesh::Sender<MyRpc>, req: inspect::Request<'_>) {
///     req.respond().merge(inspect::send(sender, MyRpc::Inspect));
/// }
/// ```
pub fn send<'a, S: 'a + mesh::rpc::RpcSend + Copy, F: 'a + Fn(Deferred) -> S::Message>(
    sender: S,
    map: F,
) -> AsDeferred<S, F> {
    AsDeferred(sender, map)
}

/// The return type of [`send`].
pub struct AsDeferred<S, F>(S, F);

impl<S: mesh::rpc::RpcSend + Copy, F: Fn(Deferred) -> S::Message> Inspect for AsDeferred<S, F> {
    fn inspect(&self, req: Request<'_>) {
        self.0.send_rpc(self.1(req.defer()));
    }
}

impl<S: mesh::rpc::RpcSend + Copy, F: Fn(Deferred) -> S::Message> InspectMut for AsDeferred<S, F> {
    fn inspect_mut(&mut self, req: Request<'_>) {
        self.0.send_rpc(self.1(req.defer()));
    }
}

impl Request<'_> {
    /// Defers the inspection request, producing a value that can be sent to
    /// another thread or context to continue the inspection asynchronously.
    pub fn defer(self) -> Deferred {
        let (send, recv) = mesh::oneshot();
        *self.node = InternalNode::Deferred(recv);
        Deferred(Box::new(DeferredInner {
            path: self.params.path().to_owned(),
            value: self.params.root.value.map(|x| x.to_owned()),
            depth: self.params.depth,
            node: send,
            sensitivity: self.params.root.sensitivity,
            number_format: self.params.number_format,
        }))
    }
}

impl UpdateRequest<'_> {
    /// Defers this update request, returning an object that can be sent across
    /// threads or processes and then used to report the update result at a
    /// later time.
    pub fn defer(self) -> DeferredUpdate {
        let (send, recv) = mesh::oneshot();
        *self.node = InternalNode::Deferred(recv);
        DeferredUpdate {
            value: self.value.to_owned(),
            node: send,
            number_format: self.number_format,
        }
    }
}

/// A deferred inspection, which can provide inspection results asynchronously
/// from a call to [`inspect`](crate::Inspect::inspect).
#[derive(Debug, MeshPayload)]
pub struct Deferred(Box<DeferredInner>);

#[derive(Debug, MeshPayload)]
struct DeferredInner {
    path: String,
    value: Option<String>,
    depth: usize,
    node: mesh::OneshotSender<InternalNode>,
    sensitivity: SensitivityLevel,
    number_format: NumberFormat,
}

impl Deferred {
    /// Inspect an object as part of a deferred inspection.
    pub fn inspect(self, obj: impl InspectMut) {
        let node = self.params(&self.root()).inspect(obj);
        self.0.node.send(node);
    }

    /// Responds to the deferred inspection, calling `f` with a [`Response`].
    pub fn respond<F: FnOnce(&mut Response<'_>)>(self, f: F) {
        let node = self.params(&self.root()).with(|req| f(&mut req.respond()));
        self.0.node.send(node);
    }

    /// Responds to the deferred request with a value.
    pub fn value(self, value: impl Into<ValueKind>) {
        self.value_(value.into())
    }
    fn value_(self, value: ValueKind) {
        let node = self.params(&self.root()).with(|req| req.value(value));
        self.0.node.send(node);
    }

    /// Returns an object used for handling an update request.
    ///
    /// If this is not an update request, returns `Err(self)`.
    pub fn update(self) -> Result<DeferredUpdate, Self> {
        if self.0.path.is_empty()
            && let Some(value) = self.0.value
        {
            Ok(DeferredUpdate {
                value,
                node: self.0.node,
                number_format: self.0.number_format,
            })
        } else {
            Err(self)
        }
    }

    fn root(&self) -> RootParams<'_> {
        RootParams {
            full_path: &self.0.path,
            sensitivity: self.0.sensitivity,
            value: self.0.value.as_deref(),
        }
    }

    fn params<'a>(&'a self, root: &'a RootParams<'a>) -> RequestParams<'a> {
        RequestParams {
            root,
            path_start: 0,
            depth: self.0.depth,
            number_format: self.0.number_format,
        }
    }

    /// Removes this node from the inspection output.
    pub fn ignore(self) {
        self.0.node.send(InternalNode::Ignored);
    }

    /// Gets the request information for sending to a remote node via a non-mesh
    /// communication mechanism, for use with [`complete_external`][].
    ///
    /// You don't need this if you are communicating with the remote object via
    /// mesh. In that case, just send this object to the remote object over a
    /// mesh channel, and then use [`respond`][] or similar methods to
    /// handle the request.
    ///
    /// Use this when the remote object is across some other communication
    /// boundary, such as gRPC. In that case, you will be responsible for using
    /// [`inspect`][] on the remote node to handle the request, and to serializing
    /// and deserializing the [`Node`][] structure across the communications
    /// boundary.
    ///
    /// [`inspect`]: crate::inspect
    /// [`Node`]: crate::Node
    /// [`respond`]: Self::respond
    /// [`complete_external`]: Self::complete_external
    #[cfg(feature = "initiate")]
    pub fn external_request(&self) -> ExternalRequest<'_> {
        ExternalRequest {
            path: &self.0.path,
            sensitivity: self.0.sensitivity,
            request_type: match &self.0.value {
                None => ExternalRequestType::Inspect {
                    depth: self.0.depth,
                },
                Some(value) => ExternalRequestType::Update { value },
            },
        }
    }

    /// Complete the request with a [`Node`][] and a [`SensitivityLevel`].
    ///
    /// See [`external_request`][] for details on how to use this.
    ///
    /// [`Node`]: crate::Node
    /// [`external_request`]: Self::external_request
    #[cfg(feature = "initiate")]
    pub fn complete_external(self, node: super::Node, sensitivity: SensitivityLevel) {
        // If the returned sensitivity level is not allowed for this request, drop it.
        if sensitivity > self.0.sensitivity {
            return;
        }
        if let Some(node) = InternalNode::from_node(node, self.0.sensitivity) {
            // Add the prefixed path back on as a sequence of directory nodes. This
            // is necessary so that they can be skipped in post-processing.
            let node =
                self.0
                    .path
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .rev()
                    .fold(node, |node, name| {
                        InternalNode::DirResolved(alloc::vec![crate::InternalEntry {
                            name: name.to_owned(),
                            node,
                            sensitivity,
                        }])
                    });

            self.0.node.send(node);
        }
    }

    /// Gets the sensitivity level for this request.
    pub fn sensitivity(&self) -> SensitivityLevel {
        self.0.sensitivity
    }
}

impl InternalNode {
    #[cfg(feature = "initiate")]
    pub(crate) fn from_node(
        value: crate::Node,
        request_sensitivity: SensitivityLevel,
    ) -> Option<Self> {
        use crate::Error;
        use crate::InternalError;
        use crate::Node;

        let node = match value {
            Node::Unevaluated => Self::Unevaluated,
            Node::Failed(err) => Self::Failed(match err {
                Error::NotFound => return None,
                Error::Unresolved => InternalError::Unresolved,
                Error::Mesh(err) => InternalError::Mesh(err),
                Error::Immutable => InternalError::Immutable,
                Error::Update(err) => InternalError::Update(err),
                Error::NotADirectory => InternalError::NotADirectory,
                Error::Internal => return None,
            }),
            Node::Value(v) => Self::Value(v),
            Node::Dir(children) => Self::DirResolved(
                children
                    .into_iter()
                    .filter_map(|e| {
                        // If the returned sensitivity level is not allowed for this request, drop it.
                        if e.sensitivity > request_sensitivity {
                            return None;
                        }
                        InternalNode::from_node(e.node, request_sensitivity).map(|v| {
                            crate::InternalEntry {
                                name: e.name,
                                node: v,
                                sensitivity: e.sensitivity,
                            }
                        })
                    })
                    .collect(),
            ),
        };
        Some(node)
    }
}

/// Return value from [`Deferred::external_request`], specifying parameters for
/// a remote inspection.
#[cfg(feature = "initiate")]
pub struct ExternalRequest<'a> {
    /// The remaining path of the request.
    pub path: &'a str,
    /// The request type and associated data.
    pub request_type: ExternalRequestType<'a>,
    /// The sensitivity level of the request.
    pub sensitivity: SensitivityLevel,
}

/// The request type associated with [`ExternalRequest`].
#[cfg(feature = "initiate")]
pub enum ExternalRequestType<'a> {
    /// An inspection request.
    Inspect {
        /// The depth to which to recurse.
        depth: usize,
    },
    /// An update request.
    Update {
        /// The value to update to.
        value: &'a str,
    },
}

/// A deferred inspection, which can provide inspection results asynchronously
/// from a call to [`inspect`](crate::Inspect::inspect).
#[derive(Debug, MeshPayload)]
pub struct DeferredUpdate {
    value: String,
    node: mesh::OneshotSender<InternalNode>,
    number_format: NumberFormat,
}

impl DeferredUpdate {
    /// Gets the requested new value.
    pub fn new_value(&self) -> &str {
        &self.value
    }

    /// Report that the update succeeded, with a new value of `value`.
    pub fn succeed(self, value: impl Into<ValueKind>) {
        self.succeed_(value.into())
    }
    fn succeed_(self, value: ValueKind) {
        self.node
            .send(InternalNode::Value(value.with_format(self.number_format)));
    }

    /// Report that the update failed, with the reason in `err`.
    pub fn fail<E: Into<Box<dyn core::error::Error + Send + Sync>>>(self, err: E) {
        self.node.send(InternalNode::failed(err.into()));
    }
}
