// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Null (disconnected) endpoint.

use crate::BufferAccess;
use crate::Endpoint;
use crate::MultiQueueSupport;
use crate::Queue;
use crate::QueueConfig;
use crate::RssConfig;
use crate::RxId;
use crate::TxId;
use crate::TxOffloadSupport;
use crate::TxSegment;
use crate::resolve::ResolveEndpointParams;
use crate::resolve::ResolvedEndpoint;
use async_trait::async_trait;
use inspect::InspectMut;
use net_backend_resources::null::NullHandle;
use std::convert::Infallible;
use std::task::Context;
use std::task::Poll;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::NetEndpointHandleKind;

pub struct NullResolver;

declare_static_resolver! {
    NullResolver,
    (NetEndpointHandleKind, NullHandle),
}

impl ResolveResource<NetEndpointHandleKind, NullHandle> for NullResolver {
    type Output = ResolvedEndpoint;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: NullHandle,
        _input: ResolveEndpointParams,
    ) -> Result<Self::Output, Self::Error> {
        Ok(NullEndpoint::new().into())
    }
}

/// An endpoint that never sends or receives any data.
#[non_exhaustive]
#[derive(InspectMut)]
pub struct NullEndpoint {}

impl NullEndpoint {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Endpoint for NullEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "null"
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        _rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        queues.extend(config.iter().map(|_| Box::new(NullQueue) as _));
        Ok(())
    }

    async fn stop(&mut self) {}

    fn is_ordered(&self) -> bool {
        true
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport {
            ipv4_header: true,
            tcp: true,
            udp: true,
            tso: true,
        }
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        MultiQueueSupport {
            max_queues: u16::MAX,
            indirection_table_size: 128,
        }
    }

    fn restore(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A queue that never sends or receives data.
#[derive(InspectMut)]
struct NullQueue;

impl Queue for NullQueue {
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }

    fn rx_avail(&mut self, _done: &[RxId]) {}

    fn rx_poll(&mut self, _packets: &mut [RxId]) -> anyhow::Result<usize> {
        Ok(0)
    }

    fn tx_avail(&mut self, packets: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        Ok((true, packets.len()))
    }

    fn tx_poll(&mut self, _done: &mut [TxId]) -> anyhow::Result<usize> {
        Ok(0)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
        None
    }
}
