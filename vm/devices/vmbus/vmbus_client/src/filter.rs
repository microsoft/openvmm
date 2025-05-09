use crate::OfferInfo;
use futures::StreamExt;
use guid::Guid;
use pal_async::task::Spawn;
use pal_async::task::Task;

pub struct ClientFilter {
    task: Task<()>,
}

impl ClientFilter {
    pub async fn shutdown(self) {
        self.task.cancel().await;
    }
}

pub struct ClientFilterBuilder {
    interfaces: Vec<(Guid, mesh::Sender<OfferInfo>)>,
    instances: Vec<(Guid, Guid, mesh::Sender<OfferInfo>)>,
    rest: Option<mesh::Sender<OfferInfo>>,
}

impl ClientFilterBuilder {
    pub fn new() -> Self {
        Self {
            interfaces: Vec::new(),
            instances: Vec::new(),
            rest: None,
        }
    }

    pub fn by_interface(&mut self, interface_id: Guid, send: mesh::Sender<OfferInfo>) -> &mut Self {
        self.interfaces.push((interface_id, send));
        self
    }

    pub fn by_instance(
        &mut self,
        interface_id: Guid,
        instance_id: Guid,
        send: mesh::Sender<OfferInfo>,
    ) -> &mut Self {
        self.instances.push((interface_id, instance_id, send));
        self
    }

    pub fn rest(&mut self, send: mesh::Sender<OfferInfo>) -> &mut Self {
        self.rest = Some(send);
        self
    }

    async fn run(&mut self, mut offers: mesh::Receiver<OfferInfo>) {
        while let Some(offer) = offers.next().await {
            let interface = &offer.offer.interface_id;
            let instance = &offer.offer.instance_id;
            let send = if let Some(send) = self.instances.iter().find_map(|(iface, inst, send)| {
                ((iface, inst) == (interface, instance)).then_some(send)
            }) {
                tracing::debug!(%interface, %instance, "filtering by instance");
                send
            } else if let Some(send) = self
                .interfaces
                .iter()
                .find_map(|(iface, send)| (iface == interface).then_some(send))
            {
                tracing::debug!(%interface, %instance, "filtering by interface");
                send
            } else if let Some(send) = self.rest.as_ref() {
                tracing::debug!(%interface, %instance, "filtering by rest");
                send
            } else {
                tracing::warn!(%interface, %instance, "dropping unfiltered offer");
                continue;
            };
            send.send(offer);
        }
    }

    pub fn build(mut self, driver: impl Spawn, offers: mesh::Receiver<OfferInfo>) -> ClientFilter {
        let task = driver.spawn("client_filter", async move {
            self.run(offers).await;
        });
        ClientFilter { task }
    }
}
