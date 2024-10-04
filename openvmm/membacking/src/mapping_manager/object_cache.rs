// Copyright (C) Microsoft Corporation. All rights reserved.

//! A cache of objects by a random object ID.
//!
//! This is used to single instance VA mappers in a process even if they are
//! created multiple times via multiple
//! [`MappingManagerClient`](super::manager::MappingManagerClient) objects.

use mesh::MeshPayload;
use parking_lot::Mutex;
use std::future::Future;
use std::sync::Arc;
use std::sync::Weak;

/// An object cache.
pub struct ObjectCache<T>(Mutex<Vec<(ObjectId, Entry<T>)>>);

/// A unique object ID.
#[derive(Copy, Clone, Debug, PartialEq, Eq, MeshPayload)]
pub struct ObjectId([u8; 16]);

impl ObjectId {
    /// Returns a new random object ID.
    pub fn new() -> Self {
        let mut v = [0; 16];
        getrandom::getrandom(&mut v).unwrap();
        Self(v)
    }
}

enum Entry<T> {
    Building(Vec<mesh::OneshotSender<Arc<T>>>),
    Built(Weak<T>),
}

impl<T: 'static + Send + Sync> ObjectCache<T> {
    /// Returns an empty cache.
    pub const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    /// Gets an entry by ID, or awaits `f` to create it if it's not already in the cache.
    pub async fn get_or_insert_with<Fut, E>(&self, id: &ObjectId, f: Fut) -> Result<Arc<T>, E>
    where
        Fut: Future<Output = Result<T, E>>,
    {
        'outer: loop {
            let recv = {
                let mut objects = self.0.lock();
                // Reap old entries. This ensures T doesn't have to clear the cache on drop.
                objects.retain(|(_, entry)| match entry {
                    Entry::Built(weak) => weak.strong_count() != 0,
                    Entry::Building(_) => true,
                });

                loop {
                    match objects.iter_mut().position(|(oid, _)| oid == id) {
                        Some(i) => match &mut objects[i].1 {
                            Entry::Building(waiters) => {
                                let (send, recv) = mesh::oneshot();
                                waiters.push(send);
                                break recv;
                            }
                            Entry::Built(weak) => {
                                if let Some(v) = weak.upgrade() {
                                    return Ok(v);
                                }
                                // The object went away after the reap above. Remove
                                // it and loop around again.
                                objects.swap_remove(i);
                            }
                        },
                        None => {
                            objects.push((*id, Entry::Building(Vec::new())));
                            break 'outer;
                        }
                    }
                }
            };

            // Wait for the currently active builder.
            if let Ok(v) = recv.await {
                return Ok(v);
            }
        }

        // Build the object and put it into the list.
        let r = f.await.map(Arc::new);
        let mut objects = self.0.lock();
        let i = objects.iter_mut().position(|(oid, _)| oid == id).unwrap();
        match &r {
            Ok(v) => {
                let weak = Arc::downgrade(v);
                match std::mem::replace(&mut objects[i].1, Entry::Built(weak)) {
                    Entry::Building(waiters) => {
                        // Send the object to each waiter.
                        for waiter in waiters {
                            waiter.send(v.clone());
                        }
                    }
                    Entry::Built(_) => unreachable!(),
                }
            }
            Err(_) => {
                objects.swap_remove(i);
            }
        }
        r
    }
}
