// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for filtering vmbus offers. This is useful for redirecting offers to
//! separate client drivers.

use crate::ConnectResult;
use crate::OfferInfo;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::pin::pin;
use vmbus_core::protocol::OfferChannel;

/// A filter.
///
/// Create using [`ClientFilterBuilder`].
pub struct ClientFilter {
    req: mesh::Sender<FilterRequest>,
    task: Task<()>,
}

impl Inspect for ClientFilter {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.req.send(FilterRequest::Inspect(req.defer()));
    }
}

enum FilterRequest {
    Inspect(inspect::Deferred),
}

impl ClientFilter {
    /// Shuts down the filter.
    pub async fn shutdown(self) {
        drop(self.req);
        self.task.await;
    }
}

/// A builder for creating a [`ClientFilter`].
pub struct ClientFilterBuilder<'a> {
    clients: Vec<&'a mut FilterDefinition>,
}

#[derive(InspectMut)]
struct FilterWorker {
    #[inspect(flatten)]
    filters: Filters,
    #[inspect(skip)]
    clients: Vec<mesh::Sender<OfferInfo>>,
}

struct Filters {
    interfaces: Vec<(Guid, usize)>,
    instances: Vec<(Guid, Guid, usize)>,
    rest: Option<usize>,
    names: Vec<String>,
}

/// A single filter definition.
pub struct FilterDefinition {
    name: String,
    interfaces: Vec<Guid>,
    instances: Vec<(Guid, Guid)>,
    rest: bool,
    result: Option<ConnectResult>,
}

impl FilterDefinition {
    /// Returns a new filter instance with the given name (for diagnostics).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            interfaces: Vec::new(),
            instances: Vec::new(),
            rest: false,
            result: None,
        }
    }

    /// Adds the specified interface ID to the filter, to include offers for that interface.
    pub fn by_interface(mut self, interface_id: Guid) -> Self {
        self.interfaces.push(interface_id);
        self
    }

    /// Adds the specified interface ID and instance ID to the filter, to
    /// include offers for a specific offer instance.
    pub fn by_instance(mut self, interface_id: Guid, instance_id: Guid) -> Self {
        self.instances.push((interface_id, instance_id));
        self
    }

    /// Filter all remaining offers that do not match any other filter.
    pub fn rest(mut self) -> Self {
        self.rest = true;
        self
    }

    /// Takes a filtered connection result.
    ///
    /// This should be called only after the filter has been built and offers
    /// have been processed, via [`ClientFilterBuilder::build`]. Panics
    /// otherwise.
    pub fn take(mut self) -> ConnectResult {
        self.result
            .take()
            .expect("failed to call ClientFilterBuilder::build")
    }
}

impl<'a> ClientFilterBuilder<'a> {
    /// Creates a new filter builder.
    pub fn new() -> Self {
        Self {
            clients: Vec::new(),
        }
    }

    /// Adds a filter definition.
    pub fn add(&mut self, client: &'a mut FilterDefinition) -> &mut Self {
        self.clients.push(client);
        self
    }

    /// Builds a filter instance, which applies the assigned filters to the
    /// initial and dynamic offers in `connection`.
    ///
    /// Uses `driver` to spawn the filter worker task.
    pub fn build(mut self, driver: impl Spawn, connection: ConnectResult) -> ClientFilter {
        let mut filters = Filters {
            interfaces: Vec::new(),
            instances: Vec::new(),
            rest: None,
            names: self.clients.iter().map(|c| c.name.clone()).collect(),
        };
        let mut offer_send = Vec::with_capacity(self.clients.len());
        for (i, client) in self.clients.iter_mut().enumerate() {
            let (send, recv) = mesh::channel();
            client.result = Some(ConnectResult {
                version: connection.version,
                offers: Vec::new(),
                offer_recv: recv,
            });
            offer_send.push(send);
            for &interface in &client.interfaces {
                filters.interfaces.push((interface, i));
            }
            for &(interface, instance) in &client.instances {
                filters.instances.push((interface, instance, i));
            }
            if client.rest {
                assert!(filters.rest.is_none(), "multiple rest filters set");
                filters.rest = Some(i);
            }
        }

        for offer in connection.offers {
            if let Some(i) = filters.find(&offer.offer) {
                self.clients[i].result.as_mut().unwrap().offers.push(offer);
            }
        }

        let (req_send, req_recv) = mesh::channel();
        let mut worker = FilterWorker {
            filters,
            clients: offer_send,
        };

        let offer_recv = connection.offer_recv;
        let task = driver.spawn("client_filter", async move {
            worker.run(req_recv, offer_recv).await;
        });
        ClientFilter {
            task,
            req: req_send,
        }
    }
}

impl Filters {
    fn find(&self, offer: &OfferChannel) -> Option<usize> {
        let interface = &offer.interface_id;
        let instance = &offer.instance_id;
        let (&v, ty) = if let Some(v) = self.instances.iter().find_map(|(iface, inst, send)| {
            ((iface, inst) == (interface, instance)).then_some(send)
        }) {
            (v, "instance")
        } else if let Some(v) = self
            .interfaces
            .iter()
            .find_map(|(iface, send)| (iface == interface).then_some(send))
        {
            (v, "interface")
        } else if let Some(v) = self.rest.as_ref() {
            (v, "rest")
        } else {
            tracing::warn!(%interface, %instance, "rejecting offer");
            return None;
        };
        tracing::debug!(
            %interface,
            %instance,
            filter_type = ty,
            client = self.names[v],
            "accepting offer"
        );
        Some(v)
    }
}

impl Inspect for Filters {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        let Self {
            interfaces,
            instances,
            rest,
            names,
        } = self;
        for &(interface, i) in interfaces {
            resp.field(&format!("by_interface/{}", interface), &names[i]);
        }
        for &(interface, instance, i) in instances {
            resp.field(
                &format!("by_instance/{}_{}", interface, instance),
                &names[i],
            );
        }
        if let Some(i) = rest {
            resp.field("rest", &names[*i]);
        }
    }
}

impl FilterWorker {
    async fn run(&mut self, req: mesh::Receiver<FilterRequest>, offers: mesh::Receiver<OfferInfo>) {
        #[expect(clippy::large_enum_variant)]
        enum Event {
            Request(FilterRequest),
            Done,
            Offer(OfferInfo),
        }
        let req = req
            .map(Event::Request)
            .chain(futures::stream::once(async { Event::Done }));
        let offers = offers.map(Event::Offer);
        let mut events = pin!((req, offers).merge());

        while let Some(event) = events.next().await {
            match event {
                Event::Request(FilterRequest::Inspect(deferred)) => deferred.inspect(&mut *self),
                Event::Done => break,
                Event::Offer(offer_info) => {
                    if let Some(i) = self.filters.find(&offer_info.offer) {
                        self.clients[i].send(offer_info);
                    }
                }
            }
        }
    }
}
