// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mesh_derive::Protobuf;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::str::FromStr;

/// A unique ID.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Protobuf)]
pub struct Uuid(pub [u8; 16]);

impl Uuid {
    fn new() -> Self {
        // Generate a cryptographically random ID so that a malicious peer
        // cannot guess a port ID.
        let mut id = Self([0; 16]);
        getrandom::fill(&mut id.0[..]).expect("rng failure");
        id
    }
}

impl Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", u128::from_be_bytes(self.0))
    }
}

impl Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

#[derive(Debug)]
pub struct ParseUuidError;

impl FromStr for Uuid {
    type Err = ParseUuidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.as_bytes()[0] == b'+' {
            return Err(ParseUuidError);
        }
        u128::from_str_radix(s, 16)
            .map(|n| Self(n.to_be_bytes()))
            .map_err(|_| ParseUuidError)
    }
}

#[cfg(debug_assertions)]
mod debug {
    //! In debug builds, conditionally return linear node and port IDs instead
    //! of random ones, based on the contents of an environment variable. This
    //! breaks some of the mesh security guarantees, so it is never safe for
    //! production use, but it simplifies mesh debugging.

    use super::Uuid;
    use std::sync::Once;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    static CHECK_ONCE: Once = Once::new();
    static USE_LINEAR_IDS: AtomicBool = AtomicBool::new(false);

    pub struct DebugUuidSource(AtomicU64);

    impl DebugUuidSource {
        pub const fn new() -> Self {
            Self(AtomicU64::new(1))
        }

        pub fn next(&self) -> Option<Uuid> {
            CHECK_ONCE.call_once(|| {
                if std::env::var_os("__MESH_UNSAFE_DEBUG_IDS__").is_some_and(|x| !x.is_empty()) {
                    tracing::error!("using unsafe debugging mesh IDs--this mesh could be compromised by external callers");
                    USE_LINEAR_IDS.store(true, Ordering::Relaxed);
                }
            });

            if !USE_LINEAR_IDS.load(Ordering::Relaxed) {
                return None;
            }

            Some(Uuid(
                u128::from(self.0.fetch_add(1, Ordering::Relaxed)).to_be_bytes(),
            ))
        }
    }
}

/// A node ID.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Protobuf)]
pub struct NodeId(pub Uuid);

impl NodeId {
    pub const ZERO: Self = Self(Uuid([0; 16]));

    pub fn new() -> Self {
        #[cfg(debug_assertions)]
        {
            static SOURCE: debug::DebugUuidSource = debug::DebugUuidSource::new();
            if let Some(id) = SOURCE.next() {
                return Self(id);
            }
        }
        Self(Uuid::new())
    }
}

impl Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "N-{:?}", &self.0)
    }
}

/// A port ID.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Protobuf)]
pub struct PortId(pub Uuid);

impl PortId {
    pub fn new() -> Self {
        #[cfg(debug_assertions)]
        {
            static SOURCE: debug::DebugUuidSource = debug::DebugUuidSource::new();
            if let Some(id) = SOURCE.next() {
                return Self(id);
            }
        }
        Self(Uuid::new())
    }
}

impl Debug for PortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "P-{:?}", &self.0)
    }
}

/// A port address.
#[derive(Copy, Clone, PartialEq, Eq, Protobuf)]
pub struct Address {
    pub node: NodeId,
    pub port: PortId,
}

impl Address {
    pub fn new(node: NodeId, port: PortId) -> Self {
        Self { node, port }
    }
}

impl Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}.{:?}", &self.node, &self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::Uuid;

    #[test]
    fn test_uuid() {
        Uuid::new();
    }
}
