// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for deferring inspection requests.

use super::InspectMut;
use super::InternalNode;
use super::Request;
use super::RequestRoot;
use super::Response;
use super::SensitivityLevel;
use super::UpdateRequest;
use super::Value;
use alloc::borrow::ToOwned;
use alloc::string::String;
use mesh::MeshPayload;

impl Request<'_> {
    /// Defers the inspection request, producing a value that can be sent to
    /// another thread or context to continue the inspection asynchronously.
    pub fn defer(self) -> Deferred {
        let (send, recv) = mesh::oneshot();
        *self.node = InternalNode::Deferred(recv);
        Deferred {
            path: self.path.to_owned(),
            value: self.value.map(|x| x.to_owned()),
            depth: self.depth,
            node: send,
            sensitivity: self.sensitivity,
        }
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
        }
    }
}

/// A deferred inspection, which can provide inspection results asynchronously
/// from a call to [`inspect`](crate::Inspect::inspect).
#[derive(Debug, MeshPayload)]
pub struct Deferred {
    path: String,
    value: Option<String>,
    depth: usize,
    node: mesh::OneshotSender<InternalNode>,
    sensitivity: SensitivityLevel,
}

impl Deferred {
    /// Inspect an object as part of a deferred inspection.
    pub fn inspect(self, mut obj: impl InspectMut) {
        let mut root = self.root();
        obj.inspect_mut(root.request());
        let node = root.node;
        self.node.send(node);
    }

    /// Responds to the deferred inspection, calling `f` with a [`Response`].
    pub fn respond<F: FnOnce(&mut Response<'_>)>(self, f: F) {
        let mut root = self.root();
        f(&mut root.request().respond());
        let node = root.node;
        self.node.send(node);
    }

    /// Responds to the deferred request with a value.
    pub fn value(self, value: Value) {
        let mut root = self.root();
        root.request().value(value);
        let node = root.node;
        self.node.send(node);
    }

    /// Returns an object used for handling an update request.
    ///
    /// If this is not an update request, returns `Err(self)`.
    pub fn update(self) -> Result<DeferredUpdate, Self> {
        if self.value.is_some() && self.path.is_empty() {
            Ok(DeferredUpdate {
                value: self.value.unwrap(),
                node: self.node,
            })
        } else {
            Err(self)
        }
    }

    fn root(&self) -> RequestRoot<'_> {
        RequestRoot::new(
            &self.path,
            self.depth,
            self.value.as_deref(),
            self.sensitivity,
        )
    }

    /// Removes this node from the inspection output.
    pub fn ignore(self) {
        self.node.send(InternalNode::Ignored);
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
            path: &self.path,
            sensitivity: self.sensitivity,
            request_type: match &self.value {
                None => ExternalRequestType::Inspect { depth: self.depth },
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
        if sensitivity > self.sensitivity {
            return;
        }
        if let Some(node) = InternalNode::from_node(node, self.sensitivity) {
            // Add the prefixed path back on as a sequence of directory nodes. This
            // is necessary so that they can be skipped in post-processing.
            let node =
                self.path
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

            self.node.send(node);
        }
    }

    /// Gets the sensitivity level for this request.
    pub fn sensitivity(&self) -> SensitivityLevel {
        self.sensitivity
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
}

impl DeferredUpdate {
    /// Gets the requested new value.
    pub fn new_value(&self) -> &str {
        &self.value
    }

    /// Report that the update succeeded, with a new value of `value`.
    pub fn succeed(self, value: Value) {
        self.node.send(InternalNode::Value(value));
    }

    /// Report that the update failed, with the reason in `err`.
    pub fn fail<E: Into<alloc::boxed::Box<dyn core::error::Error + Send + Sync>>>(self, err: E) {
        self.node.send(InternalNode::failed(err.into()));
    }
}
