// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! GPADL support.

use inspect::Inspect;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use thiserror::Error;
pub use vmbus_core::protocol::GpadlId;
use vmbus_ring::gparange::MultiPagedRangeBuf;

/// Object to call when a GPADL is torn down.
pub type TeardownFn = Box<dyn FnOnce() + Send>;

struct Teardown(TeardownFn);

impl std::fmt::Debug for Teardown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("Teardown")
    }
}

#[derive(Debug)]
struct GpadlState {
    tearing_down: bool,
    map_count: usize,
    teardown: Option<Teardown>,
}

#[derive(Debug)]
struct Gpadl {
    id: GpadlId,
    buf: MultiPagedRangeBuf,
    state: Mutex<GpadlState>,
}

impl Inspect for Gpadl {
    fn inspect(&self, req: inspect::Request<'_>) {
        let state = self.state.lock();
        req.respond()
            .field("tearing_down", state.tearing_down)
            .field("map_count", state.map_count)
            .child("ranges", |req| {
                let mut resp = req.respond();
                for (i, range) in self.buf.iter().enumerate() {
                    resp.child(&i.to_string(), |req| {
                        req.respond()
                            .hex("len", range.len())
                            .hex("offset", range.offset())
                            .field(
                                "pages",
                                range
                                    .gpns()
                                    .iter()
                                    .map(|gpa| format!("{:x}", gpa))
                                    .collect::<Vec<_>>()
                                    .join(" "),
                            );
                    });
                }
            });
    }
}

/// A GPADL that has been provided by the guest. It has not yet been locked.
///
/// The guest will not reuse the associated memory while this exists.
#[derive(Debug)]
pub struct GpadlView(Arc<Gpadl>);

impl Clone for GpadlView {
    fn clone(&self) -> Self {
        let clone = GpadlView(self.0.clone());
        let mut state = self.0.state.lock();
        state.map_count += 1;
        clone
    }
}

impl GpadlView {
    /// Returns the GPADL identifier.
    pub fn id(&self) -> GpadlId {
        self.0.id
    }
}

impl Deref for GpadlView {
    type Target = MultiPagedRangeBuf;
    fn deref(&self) -> &Self::Target {
        &self.0.buf
    }
}

impl Drop for GpadlView {
    fn drop(&mut self) {
        let teardown = {
            let mut state = self.0.state.lock();
            state.map_count -= 1;
            if state.map_count == 0 && state.tearing_down {
                state.teardown.take()
            } else {
                None
            }
        };
        if let Some(Teardown(teardown)) = teardown {
            teardown();
        }
    }
}

/// A set of GPADLs that the guest has made available to the host.
#[derive(Debug)]
pub struct GpadlMap {
    map: Mutex<HashMap<GpadlId, Arc<Gpadl>>>,
}

impl Inspect for GpadlMap {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .fields("", self.map.lock().iter().map(|(id, gpadl)| (&id.0, gpadl)));
    }
}

impl GpadlMap {
    /// Creates an empty map.
    pub fn new() -> Arc<Self> {
        Arc::new(GpadlMap {
            map: Default::default(),
        })
    }

    /// Adds the specified GPADL to the map.
    pub fn add(&self, id: GpadlId, buf: MultiPagedRangeBuf) {
        let gpadl = Arc::new(Gpadl {
            id,
            buf,
            state: Mutex::new(GpadlState {
                tearing_down: false,
                map_count: 0,
                teardown: None,
            }),
        });
        let mut map = self.map.lock();
        map.insert(id, gpadl);
    }

    /// Removes the specified GPADL from the mapping, calling `f` when there are
    /// no more [`GpadlView`] instances.
    pub fn remove(&self, id: GpadlId, f: TeardownFn) -> Option<TeardownFn> {
        let gpadl = {
            let mut map = self.map.lock();
            map.remove(&id).unwrap()
        };
        let mut state = gpadl.state.lock();
        assert!(!state.tearing_down && state.teardown.is_none());
        state.tearing_down = true;
        if state.map_count > 0 {
            state.teardown = Some(Teardown(f));
            None
        } else {
            Some(f)
        }
    }

    /// Constructs a GPADL map view.
    pub fn view(self: Arc<Self>) -> GpadlMapView {
        GpadlMapView(Some(self))
    }
}

/// A GPADL map view for mapping GPADLs.
#[derive(Debug, Default, Clone)]
pub struct GpadlMapView(Option<Arc<GpadlMap>>);

/// An error indicating an attempt was made to map an unknown GPADL.
#[derive(Debug, Error)]
#[error("unknown gpadl ID {:#x}", (.0).0)]
pub struct UnknownGpadlId(GpadlId);

impl GpadlMapView {
    /// Maps the GPADL with `id`.
    pub fn map(&self, id: GpadlId) -> Result<GpadlView, UnknownGpadlId> {
        self.try_map(id).ok_or(UnknownGpadlId(id))
    }

    fn try_map(&self, id: GpadlId) -> Option<GpadlView> {
        let gpadl = {
            let map = self.0.as_ref()?.map.lock();
            map.get(&id)?.clone()
        };
        {
            let mut state = gpadl.state.lock();
            if state.tearing_down {
                return None;
            }
            state.map_count += 1;
        }
        Some(GpadlView(gpadl))
    }
}
