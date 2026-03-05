// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for initiating inspection requests.

mod natural_sort;

use super::InspectMut;
use super::InternalError;
use super::InternalNode;
use super::SensitivityLevel;
use super::Value;
use super::ValueKind;
use crate::NumberFormat;
use crate::RequestParams;
use crate::RootParams;
use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use base64::display::Base64Display;
use core::cmp::Ordering;
use core::fmt;
use core::fmt::Write;
use core::future::Future;
use core::future::poll_fn;
use core::pin::Pin;
use core::task::Context;
use core::task::Poll;
use core::time::Duration;
use mesh::MeshPayload;
use thiserror::Error;

/// A node of an inspect result.
#[derive(Debug, Clone, PartialEq, MeshPayload)]
#[mesh(package = "inspect")]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub enum Node {
    /// The node is known to exist in the inspect tree but was not evaluated.
    #[mesh(1)]
    Unevaluated,
    /// Evaluation of this node did not complete successfully.
    #[mesh(2)]
    Failed(Error),
    /// A value.
    #[mesh(3)]
    Value(Value),
    /// An interior node, with zero or more children.
    #[mesh(4)]
    Dir(Vec<Entry>),
}

#[derive(Debug, Clone, PartialEq, MeshPayload)]
#[mesh(package = "inspect")]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
/// A directory entry.
pub struct Entry {
    /// The name of the entry.
    #[mesh(1)]
    pub name: String,
    /// The node at this entry.
    #[mesh(2)]
    pub node: Node,
    /// The sensitivity level of this entry.
    #[mesh(3)]
    pub sensitivity: SensitivityLevel,
}

/// A node resolution error.
#[derive(Debug, Clone, PartialEq, Eq, Error, MeshPayload)]
#[mesh(package = "inspect")]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
pub enum Error {
    /// Deferred request never resolved.
    #[error("unresolved")]
    #[mesh(1)]
    Unresolved,
    /// Mesh channel error.
    #[error("channel error: {0}")]
    #[mesh(2)]
    Mesh(String),
    /// The node is immutable.
    #[error("immutable node")]
    #[mesh(3)]
    Immutable,
    /// The update value could not be applied.
    #[error("update error: {0}")]
    #[mesh(4)]
    Update(String),
    /// A requested node is not a directory.
    #[error("not a directory")]
    #[mesh(5)]
    NotADirectory,
    /// A requested node was not found.
    #[error("not found")]
    #[mesh(6)]
    NotFound,
    /// An internal error occurred.
    #[error("internal error")]
    #[mesh(7)]
    Internal,
}

impl From<InternalError> for Error {
    fn from(value: InternalError) -> Self {
        match value {
            InternalError::Immutable => Self::Immutable,
            InternalError::Update(v) => Self::Update(v),
            InternalError::NotADirectory => Self::NotADirectory,
            InternalError::Unresolved => Self::Unresolved,
            InternalError::Mesh(v) => Self::Mesh(v),
        }
    }
}

/// Implement Debug by calling Display.
struct DebugFromDisplay<T>(T);

impl<T: fmt::Display> fmt::Debug for DebugFromDisplay<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Node::Unevaluated => f.pad("_"),
            Node::Failed(err) => write!(f, "error ({err})"),
            Node::Value(v) => fmt::Display::fmt(v, f),
            Node::Dir(children) => {
                let mut map = f.debug_map();
                for entry in children {
                    map.entry(
                        &DebugFromDisplay(&entry.name),
                        &DebugFromDisplay(&entry.node),
                    );
                }
                map.finish()
            }
        }
    }
}

impl Node {
    fn merge_list(children: &mut Vec<Entry>) {
        // Sort the new list of children.
        children.sort_by(|a, b| natural_sort::compare(&a.name, &b.name));

        // Merge duplicates.
        {
            let mut last: Option<&mut Entry> = None;
            for entry in children.iter_mut() {
                if entry.name.is_empty() {
                    continue;
                }
                match &mut last {
                    Some(last_entry) if *last_entry.name == entry.name => {
                        last_entry
                            .node
                            .merge(core::mem::replace(&mut entry.node, Node::Unevaluated));
                        entry.name.clear();
                    }
                    _ => {
                        last = Some(entry);
                    }
                }
            }
        }

        // Remove nameless children.
        children.retain(|entry| !entry.name.is_empty());
    }

    fn merge(&mut self, other: Node) {
        if matches!(other, Node::Unevaluated) {
            return;
        }
        match self {
            Node::Unevaluated | Node::Failed(_) | Node::Value(_) => {
                // Cannot merge, so the later node takes precedence.
                *self = other;
            }

            Node::Dir(children) => match other {
                Node::Unevaluated | Node::Failed(_) | Node::Value(_) => {
                    // Cannot merge, so the directory takes precedence.
                }
                Node::Dir(mut other_children) => {
                    children.append(&mut other_children);
                    Self::merge_list(children);
                }
            },
        }
    }

    fn skip(mut self, mut n: usize) -> Node {
        while n > 0 {
            self = match self {
                Node::Dir(d) => {
                    if d.len() == 1 {
                        d.into_iter().next().unwrap().node
                    } else if d.is_empty() {
                        return Node::Failed(Error::NotFound);
                    } else {
                        // The walk should not have produced a multi-child
                        // directory node here.
                        return Node::Failed(Error::Internal);
                    }
                }
                Node::Failed(_) | Node::Unevaluated => return self,
                Node::Value(_) => {
                    // The walk should not have produced a value here.
                    return Node::Failed(Error::Internal);
                }
            };
            n -= 1;
        }
        self
    }

    fn compute_since(&self, last: &Node, t: f64) -> Node {
        match (self, last) {
            (Node::Value(value), Node::Value(last)) if value.flags.count() => {
                let kind = match (&value.kind, &last.kind) {
                    (ValueKind::Unsigned(x), ValueKind::Unsigned(y)) => {
                        ValueKind::Double((x - y) as f64 / t)
                    }
                    (ValueKind::Signed(x), ValueKind::Signed(y)) => {
                        ValueKind::Double((x - y) as f64 / t)
                    }
                    (ValueKind::Float(x), ValueKind::Float(y)) => {
                        ValueKind::Double((x - y) as f64 / t)
                    }
                    (ValueKind::Double(x), ValueKind::Double(y)) => ValueKind::Double((x - y) / t),
                    (kind, _) => kind.clone(),
                };
                Node::Value(Value {
                    kind,
                    flags: value.flags,
                })
            }
            (Node::Dir(this), Node::Dir(last)) => {
                let mut children = Vec::new();
                let mut this = this.iter().peekable();
                let mut last = last.iter().peekable();
                while let (Some(&this_entry), Some(&last_entry)) = (this.peek(), last.peek()) {
                    match this_entry.name.cmp(&last_entry.name) {
                        Ordering::Less => {
                            children.push(this_entry.clone());
                            this.next();
                        }
                        Ordering::Equal => {
                            children.push(Entry {
                                node: this_entry.node.compute_since(&last_entry.node, t),
                                ..this_entry.clone()
                            });
                            this.next();
                            last.next();
                        }
                        Ordering::Greater => {
                            last.next();
                        }
                    }
                }
                children.extend(this.cloned());
                Node::Dir(children)
            }
            (node, _) => node.clone(),
        }
    }

    /// Computes the differences in this node from a previous snapshot of the
    /// same node.
    ///
    /// For ordinary values, the result will just have the new values.
    ///
    /// For values marked as counters, the result will be the difference since
    /// the last node, divided by `duration` (in seconds). The value type will
    /// be changed to floating point to capture the non-integral portion of the
    /// result.
    pub fn since(&self, last: &Node, duration: Duration) -> Self {
        self.compute_since(last, duration.as_secs_f64())
    }

    /// Returns an object that implements [`Display`](core::fmt::Display) to output JSON.
    pub fn json(&self) -> impl '_ + fmt::Display {
        JsonDisplay(self)
    }
}

struct JsonDisplay<'a>(&'a Node);

impl fmt::Display for JsonDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Node::Unevaluated => f.write_str("null"),
            // Errors are serialized using a key that should not occur in valid output, since those
            // are normally valid Rust identifiers.
            Node::Failed(error) => write!(f, r#"{{"$error":{:?}}}"#, error.to_string()),
            Node::Value(value) => match &value.kind {
                ValueKind::Signed(v) => {
                    // For human readability, values that should be hex or binary are serialized as
                    // strings.
                    if value.flags.hex() {
                        write!(f, r#""{v:#x}""#)
                    } else if value.flags.binary() {
                        write!(f, r#""{v:#b}""#)
                    } else {
                        write!(f, "{v}")
                    }
                }
                ValueKind::Unsigned(v) => {
                    if value.flags.hex() {
                        write!(f, r#""{v:#x}""#)
                    } else if value.flags.binary() {
                        write!(f, r#""{v:#b}""#)
                    } else {
                        write!(f, "{v}")
                    }
                }
                ValueKind::Float(v) => write!(f, "{v}"),
                ValueKind::Double(v) => write!(f, "{v}"),
                ValueKind::Bool(v) => write!(f, "{v}"),
                ValueKind::String(v) => write!(f, "{v:?}"),
                ValueKind::Bytes(b) => {
                    // Use base64 encoding to match typical JSON conventions.
                    write!(
                        f,
                        r#""{}""#,
                        Base64Display::new(b, &base64::engine::general_purpose::STANDARD_NO_PAD)
                    )
                }
            },
            Node::Dir(children) => {
                f.write_char('{')?;
                let mut comma = "";
                for entry in children {
                    let child = JsonDisplay(&entry.node);
                    let name = &entry.name;
                    write!(f, "{comma}{name:?}:{child}")?;
                    comma = ",";
                }
                f.write_char('}')?;
                Ok(())
            }
        }
    }
}

fn path_node_count(path: &str) -> usize {
    path.split('/').filter(|x| !x.is_empty()).count()
}

/// A builder for an inspection request.
pub struct InspectionBuilder<'a> {
    path: &'a str,
    depth: Option<usize>,
    sensitivity: Option<SensitivityLevel>,
}

impl<'a> InspectionBuilder<'a> {
    /// Creates a new builder for an inspection request.
    pub fn new(path: &'a str) -> Self {
        Self {
            path,
            depth: None,
            sensitivity: None,
        }
    }

    /// Sets the maximum depth of the inspection request.
    pub fn depth(mut self, depth: Option<usize>) -> Self {
        self.depth = depth;
        self
    }

    /// Sets the [`SensitivityLevel`] of the inspection request.
    pub fn sensitivity(mut self, sensitivity: Option<SensitivityLevel>) -> Self {
        self.sensitivity = sensitivity;
        self
    }

    /// Inspects `obj` for state at the initially given `path`.
    pub fn inspect(self, obj: impl InspectMut) -> Inspection {
        let root = self.root();
        let (params, skip) = self.build(&root);
        Inspection {
            node: params.inspect(obj),
            skip,
        }
    }

    /// Updates a value in `obj` at the initially given `path` to value `value`.
    pub fn update(self, value: &'a str, obj: impl InspectMut) -> Update {
        let mut root = self.root();
        root.value = Some(value);
        let (params, skip) = self.build(&root);
        Update {
            node: params.inspect(obj),
            skip,
        }
    }

    fn root(&self) -> RootParams<'_> {
        RootParams {
            full_path: self.path,
            value: None,
            sensitivity: self.sensitivity.unwrap_or(SensitivityLevel::Sensitive),
        }
    }

    fn build<'b>(&self, root: &'b RootParams<'_>) -> (RequestParams<'b>, usize) {
        // Account for the root node by bumping depth.
        // Also enforce a maximum depth of 4096, anything deeper than that is
        // most likely a bug, and we don't want to cause an infinite loop.
        const MAX_INSPECT_DEPTH: usize = 4096;
        let depth_with_root = if let Some(depth) = self.depth {
            depth.saturating_add(1).min(MAX_INSPECT_DEPTH)
        } else {
            MAX_INSPECT_DEPTH
        };
        let params = RequestParams {
            root,
            path_start: 0,
            depth: depth_with_root,
            number_format: NumberFormat::default(),
            parent_sensitivity: SensitivityLevel::Unspecified,
        };
        (params, path_node_count(root.full_path))
    }
}

/// Inspects `obj` for state at `path`.
///
/// ```rust
/// # use inspect::{Inspect, Request, Node, inspect, Value, ValueKind};
/// # use futures::executor::block_on;
/// # use core::time::Duration;
/// struct Obj;
/// impl Inspect for Obj {
///     fn inspect(&self, req: Request) {
///         req.respond().field("field", 3);
///     }
/// }
/// let mut inspection = inspect("field", &Obj);
/// block_on(inspection.resolve());
/// let node = inspection.results();
/// assert!(matches!(node, Node::Value(Value { kind: ValueKind::Signed(3), .. })));
/// ```
pub fn inspect(path: &str, obj: impl InspectMut) -> Inspection {
    InspectionBuilder::new(path).inspect(obj)
}

/// An active inspection, returned by [`inspect()`] or [`InspectionBuilder::inspect()`].
#[derive(Debug)]
pub struct Inspection {
    node: InternalNode,
    skip: usize,
}

impl Inspection {
    /// Resolves any deferred inspection nodes, waiting indefinitely until they
    /// are responded to.
    ///
    /// This future may be dropped (e.g. after a timeout) to stop collecting
    /// inspection results without losing results that have already been
    /// collected.
    pub async fn resolve(&mut self) {
        self.node.resolve().await
    }

    /// Returns the current results of the inspection.
    ///
    /// This may have unresolved nodes if [`Self::resolve`] was not called or
    /// was cancelled before it completed.
    pub fn results(self) -> Node {
        self.node.into_node().skip(self.skip)
    }
}

/// Updates a value in `obj` at `path` to value `value`.
pub fn update(path: &str, value: &str, obj: impl InspectMut) -> Update {
    InspectionBuilder::new(path).update(value, obj)
}

/// An active update operation, returned by [`update()`] or [`InspectionBuilder::update()`].
pub struct Update {
    node: InternalNode,
    skip: usize,
}

impl Future for Update {
    type Output = Result<Value, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        core::task::ready!(this.node.poll_resolve(cx));
        Poll::Ready(
            match std::mem::replace(&mut this.node, InternalNode::Unevaluated)
                .into_node()
                .skip(this.skip)
            {
                Node::Unevaluated => Err(Error::Unresolved),
                Node::Failed(err) => Err(err),
                Node::Value(v) => Ok(v),
                Node::Dir(_) => Err(Error::Unresolved),
            },
        )
    }
}

impl InternalNode {
    fn poll_resolve(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            match self {
                InternalNode::Dir(children) => {
                    // Poll each child, even if a previous one returned pending,
                    // in order to collect as many results as possible. This is
                    // important in case the resolve operation is timed out.
                    if !children
                        .iter_mut()
                        .all(|entry| entry.node.poll_resolve(cx).is_ready())
                    {
                        break Poll::Pending;
                    }
                    // Remember that this node is resolved to avoid recursing
                    // again.
                    *self = InternalNode::DirResolved(core::mem::take(children));
                }
                InternalNode::Deferred(recv) => match Pin::new(recv).poll(cx) {
                    Poll::Ready(node) => {
                        *self = match node {
                            Ok(node) => {
                                // N.B. This could be another deferred node, or
                                // a directory with deferred child nodes.
                                node
                            }
                            Err(err) => InternalNode::Failed(match err {
                                mesh::RecvError::Closed => InternalError::Unresolved,
                                mesh::RecvError::Error(err) => InternalError::Mesh(err.to_string()),
                            }),
                        };
                    }
                    _ => break Poll::Pending,
                },
                _ => break Poll::Ready(()),
            }
        }
    }

    async fn resolve(&mut self) {
        poll_fn(|cx| self.poll_resolve(cx)).await
    }

    fn into_node(self) -> Node {
        match self {
            InternalNode::Dir(children) | InternalNode::DirResolved(children) => {
                // Convert child nodes and merge any nameless children.
                let mut child_nodes = Vec::new();
                for entry in children {
                    if matches!(entry.node, InternalNode::Ignored) {
                        continue;
                    }
                    let mut child_node = entry.node.into_node();

                    if entry.name.is_empty() {
                        if let Node::Dir(grandchildren) = child_node {
                            // No name for the node--merge the grandchildren in.
                            child_nodes.extend(grandchildren);
                        }
                    } else {
                        let mut name = entry.name;
                        let root_len = {
                            // Handle multi-level names like foo/bar/baz.
                            let mut names = name.split('/');
                            let root_name = names.next().unwrap();
                            for interior_name in names.rev() {
                                child_node = Node::Dir(vec![Entry {
                                    name: interior_name.to_owned(),
                                    node: child_node,
                                    sensitivity: entry.sensitivity,
                                }]);
                            }
                            root_name.len()
                        };
                        name.truncate(root_len);
                        child_nodes.push(Entry {
                            name,
                            node: child_node,
                            sensitivity: entry.sensitivity,
                        });
                    }
                }

                Node::merge_list(&mut child_nodes);
                Node::Dir(child_nodes)
            }
            InternalNode::Value(v) => Node::Value(v),
            InternalNode::Failed(err) => Node::Failed(err.into()),
            InternalNode::Deferred(_) => Node::Failed(Error::Unresolved),
            InternalNode::DepthExhausted | InternalNode::Unevaluated | InternalNode::Ignored => {
                Node::Unevaluated
            }
        }
    }
}
