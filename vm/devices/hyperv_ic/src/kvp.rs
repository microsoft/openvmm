// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVP IC device.

use crate::common::IcPipe;
use crate::common::NegotiateState;
use crate::common::Versions;
use anyhow::Context;
use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FusedStream;
use futures::stream::once;
use futures_concurrency::stream::Merge;
use hyperv_ic_protocol::HeaderFlags;
use hyperv_ic_protocol::Status;
use hyperv_ic_protocol::kvp as proto;
use hyperv_ic_resources::kvp::AddressOrigin;
use hyperv_ic_resources::kvp::IpInfo;
use hyperv_ic_resources::kvp::Ipv4AddressInfo;
use hyperv_ic_resources::kvp::Ipv6AddressInfo;
use hyperv_ic_resources::kvp::KeyValue;
use hyperv_ic_resources::kvp::KvpConnectRpc;
use hyperv_ic_resources::kvp::KvpRpc;
use hyperv_ic_resources::kvp::Value;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::FailableRpc;
use std::pin::pin;
use task_control::Cancelled;
use task_control::StopTask;
use thiserror::Error;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::channel::ChannelOpenError;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_channel::simple::SaveRestoreSimpleVmbusDevice;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::save_restore::NoSavedState;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

const KVP_VERSIONS: &[hyperv_ic_protocol::Version] = &[
    proto::KVP_VERSION_3,
    proto::KVP_VERSION_4,
    proto::KVP_VERSION_5,
];

/// KVP IC device.
#[derive(InspectMut)]
pub struct KvpIc {
    #[inspect(skip)]
    recv: mesh::Receiver<KvpConnectRpc>,
    #[inspect(skip)]
    wait_ready: Vec<FailableRpc<(), (mesh::Sender<KvpRpc>, mesh::OneshotReceiver<()>)>>,
}

#[doc(hidden)]
#[derive(InspectMut)]
pub struct KvpChannel {
    #[inspect(mut)]
    pipe: IcPipe,
    state: ChannelState,
}

#[expect(clippy::large_enum_variant)]
#[derive(Inspect)]
#[inspect(external_tag)]
enum ChannelState {
    Negotiate(#[inspect(rename = "state")] NegotiateState),
    Ready {
        versions: Versions,
        #[inspect(with = "|x| x.len()")]
        clients: Vec<mesh::OneshotSender<()>>,
        #[inspect(skip)]
        rpc_recv: mesh::Receiver<KvpRpc>,
        state: ReadyState,
    },
    Failed,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ReadyState {
    Ready,
    SendingRequest(#[inspect(skip)] KvpRpc),
    WaitingResponse(#[inspect(skip)] KvpRpc),
}

#[async_trait]
impl SimpleVmbusDevice for KvpIc {
    type SavedState = NoSavedState;
    type Runner = KvpChannel;

    fn offer(&self) -> OfferParams {
        OfferParams {
            interface_name: "kvp_ic".to_owned(),
            instance_id: proto::INSTANCE_ID,
            interface_id: proto::INTERFACE_ID,
            channel_type: ChannelType::Pipe { message_mode: true },
            ..Default::default()
        }
    }

    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>) {
        req.respond().merge(self).merge(runner);
    }

    fn open(
        &mut self,
        channel: RawAsyncChannel<GpadlRingMem>,
        _guest_memory: guestmem::GuestMemory,
    ) -> Result<Self::Runner, ChannelOpenError> {
        KvpChannel::new(channel, None)
    }

    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(async { runner.process(self).await })
            .await
    }

    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusDevice<SavedState = Self::SavedState, Runner = Self::Runner>,
    > {
        None
    }
}

impl KvpIc {
    /// Create a new KVP IC device.
    pub fn new(recv: mesh::Receiver<KvpConnectRpc>) -> Self {
        Self {
            recv,
            wait_ready: Vec::new(),
        }
    }
}

impl KvpChannel {
    fn new(
        channel: RawAsyncChannel<GpadlRingMem>,
        restore_state: Option<ChannelState>,
    ) -> Result<Self, ChannelOpenError> {
        let pipe = IcPipe::new(channel)?;
        Ok(Self {
            pipe,
            state: restore_state.unwrap_or(ChannelState::Negotiate(NegotiateState::default())),
        })
    }

    async fn process(&mut self, ic: &mut KvpIc) -> ! {
        enum Event {
            StateMachine(anyhow::Result<()>),
            Request(KvpConnectRpc),
        }

        loop {
            let event = pin!(
                (
                    once(
                        self.process_state_machine(&mut ic.wait_ready)
                            .map(Event::StateMachine)
                    ),
                    (&mut ic.recv).map(Event::Request),
                )
                    .merge()
            )
            .next()
            .await
            .unwrap();
            match event {
                Event::StateMachine(r) => {
                    if let Err(err) = r {
                        tracing::error!(
                            error = err.as_ref() as &dyn std::error::Error,
                            "kvp ic error"
                        );
                        self.state = ChannelState::Failed;
                        for rpc in ic.wait_ready.drain(..) {
                            rpc.fail(anyhow::anyhow!("kvp channel failed"));
                        }
                    }
                }
                Event::Request(req) => match req {
                    KvpConnectRpc::WaitForGuest(rpc) => match &mut self.state {
                        ChannelState::Negotiate(_) => ic.wait_ready.push(rpc),
                        ChannelState::Ready {
                            clients, rpc_recv, ..
                        } => {
                            let rpc_send = rpc_recv.sender();
                            let (send, recv) = mesh::oneshot();
                            clients.retain(|c| !c.is_closed());
                            clients.push(send);
                            rpc.complete(Ok((rpc_send, recv)));
                        }
                        ChannelState::Failed => {
                            rpc.fail(anyhow::anyhow!("kvp channel failed"));
                        }
                    },
                },
            }
        }
    }

    async fn process_state_machine(
        &mut self,
        wait_ready: &mut Vec<FailableRpc<(), (mesh::Sender<KvpRpc>, mesh::OneshotReceiver<()>)>>,
    ) -> anyhow::Result<()> {
        match self.state {
            ChannelState::Negotiate(ref mut state) => {
                if let Some(versions) = self.pipe.negotiate(state, KVP_VERSIONS).await? {
                    let mut rpc_recv = mesh::Receiver::new();
                    let clients = wait_ready
                        .drain(..)
                        .map(|rpc| {
                            let rpc_send = rpc_recv.sender();
                            let (send, recv) = mesh::oneshot();
                            rpc.complete(Ok((rpc_send, recv)));
                            send
                        })
                        .collect();

                    self.state = ChannelState::Ready {
                        versions,
                        clients,
                        rpc_recv,
                        state: ReadyState::Ready,
                    };
                }
            }
            ChannelState::Ready {
                ref versions,
                rpc_recv: ref mut recv,
                ref mut state,
                clients: _,
            } => match state {
                ReadyState::Ready => {
                    if recv.is_terminated() {
                        return std::future::pending().await;
                    }
                    if let Some(rpc) = recv.next().await {
                        *state = ReadyState::SendingRequest(rpc);
                    }
                }
                ReadyState::SendingRequest(rpc) => {
                    match rpc {
                        KvpRpc::Set(rpc) => {
                            let params = rpc.input();
                            let mut message = proto::MessageGetSet::new_zeroed();
                            message.value.key_size =
                                write_str(&mut message.value.key, &params.key)?;

                            (message.value.value_type, message.value.value_size) =
                                write_value(&mut message.value.value, &params.value)?;

                            self.pipe
                                .write_message(
                                    versions,
                                    hyperv_ic_protocol::MessageType::KVP_EXCHANGE,
                                    HeaderFlags::new().with_request(true).with_transaction(true),
                                    msg(proto::KvpOperation::SET, pool_cvt(params.pool), message)
                                        .as_bytes(),
                                )
                                .await?;
                        }
                        KvpRpc::Delete(rpc) => {
                            let params = rpc.input();
                            let mut message = proto::MessageDelete::new_zeroed();
                            message.key_size = write_str(&mut message.key, &params.key)?;
                            self.pipe
                                .write_message(
                                    versions,
                                    hyperv_ic_protocol::MessageType::KVP_EXCHANGE,
                                    HeaderFlags::new().with_request(true).with_transaction(true),
                                    msg(
                                        proto::KvpOperation::DELETE,
                                        pool_cvt(params.pool),
                                        message,
                                    )
                                    .as_bytes(),
                                )
                                .await?;
                        }
                        KvpRpc::Enumerate(rpc) => {
                            let params = rpc.input();
                            let message = proto::MessageEnumerate {
                                index: params.index,
                                value: proto::Value::new_zeroed(),
                            };

                            self.pipe
                                .write_message(
                                    versions,
                                    hyperv_ic_protocol::MessageType::KVP_EXCHANGE,
                                    HeaderFlags::new().with_request(true).with_transaction(true),
                                    msg(
                                        proto::KvpOperation::ENUMERATE,
                                        pool_cvt(params.pool),
                                        message,
                                    )
                                    .as_bytes(),
                                )
                                .await?;
                        }
                        KvpRpc::GetIpInfo(rpc) => {
                            let params = rpc.input();
                            let message = match prepare_get_ip_info(versions, &params.adapter_id) {
                                Ok(message) => message,
                                Err(err) => {
                                    let ReadyState::SendingRequest(KvpRpc::GetIpInfo(rpc)) =
                                        std::mem::replace(state, ReadyState::Ready)
                                    else {
                                        unreachable!()
                                    };
                                    rpc.fail(err);
                                    return Ok(());
                                }
                            };
                            self.pipe
                                .write_message(
                                    versions,
                                    hyperv_ic_protocol::MessageType::KVP_EXCHANGE,
                                    HeaderFlags::new().with_request(true).with_transaction(true),
                                    message.as_bytes(),
                                )
                                .await?;
                        }
                        KvpRpc::SetIpInfo(rpc) => {
                            let params = rpc.input();
                            let message = match prepare_set_ip_info(
                                versions,
                                &params.adapter_id,
                                &params.info,
                            ) {
                                Ok(message) => message,
                                Err(err) => {
                                    let ReadyState::SendingRequest(KvpRpc::SetIpInfo(rpc)) =
                                        std::mem::replace(state, ReadyState::Ready)
                                    else {
                                        unreachable!()
                                    };
                                    rpc.fail(err);
                                    return Ok(());
                                }
                            };
                            self.pipe
                                .write_message(
                                    versions,
                                    hyperv_ic_protocol::MessageType::KVP_EXCHANGE,
                                    HeaderFlags::new().with_request(true).with_transaction(true),
                                    message.as_bytes(),
                                )
                                .await?;
                        }
                    }
                    let ReadyState::SendingRequest(rpc) =
                        std::mem::replace(state, ReadyState::Ready)
                    else {
                        unreachable!()
                    };
                    *state = ReadyState::WaitingResponse(rpc);
                }
                ReadyState::WaitingResponse(_) => {
                    let (status, response) = self.pipe.read_response().await?;
                    let r = if status == Status::SUCCESS {
                        Ok(())
                    } else {
                        Err(RequestError(status))
                    };
                    let ReadyState::WaitingResponse(rpc) =
                        std::mem::replace(state, ReadyState::Ready)
                    else {
                        unreachable!()
                    };
                    match rpc {
                        KvpRpc::Set(rpc) => rpc.handle_failable_sync(|_| r),
                        KvpRpc::Delete(rpc) => rpc.handle_failable_sync(|_| r),
                        KvpRpc::Enumerate(rpc) => match r {
                            Ok(()) => {
                                let v = parse_enumerate_response(response)?;
                                rpc.complete(Ok(Some(v)));
                            }
                            Err(RequestError(Status::NO_MORE_ITEMS)) => {
                                rpc.complete(Ok(None));
                            }
                            Err(err) => rpc.fail(err),
                        },
                        KvpRpc::GetIpInfo(rpc) => match r {
                            Ok(()) => {
                                let v = parse_ip_info_response(response)?;
                                rpc.complete(Ok(v))
                            }
                            Err(err) => rpc.fail(err),
                        },
                        KvpRpc::SetIpInfo(rpc) => rpc.handle_failable_sync(|_| r),
                    }
                }
            },
            ChannelState::Failed => std::future::pending().await,
        }
        Ok(())
    }
}

fn parse_response<T: FromBytes + Immutable>(response: &[u8]) -> Result<T, anyhow::Error> {
    let response = response
        .get(align_of::<T>().max(size_of::<proto::KvpHeader>())..)
        .context("missing header")?;
    let (response, _) = T::read_from_prefix(response)
        .ok()
        .context("missing response")?;
    Ok(response)
}

fn parse_enumerate_response(response: &[u8]) -> Result<KeyValue, anyhow::Error> {
    let response = parse_response::<proto::MessageEnumerate>(response)?;
    let key = parse_str(&response.value.key, response.value.key_size)?;
    let value = match response.value.value_type {
        proto::ValueType::DWORD => {
            if response.value.value_size != 4 {
                anyhow::bail!("invalid dword value size");
            }
            let value = u32::from_le_bytes(response.value.value[..4].try_into().unwrap());
            Value::U32(value)
        }
        proto::ValueType::QWORD => {
            if response.value.value_size != 8 {
                anyhow::bail!("invalid qword value size");
            }
            let value = u64::from_le_bytes(response.value.value[..8].try_into().unwrap());
            Value::U64(value)
        }
        proto::ValueType::STRING | proto::ValueType::EXPAND_STRING => {
            let value = parse_str(
                <[u16]>::ref_from_bytes(&response.value.value).unwrap(),
                response.value.value_size,
            )?;
            Value::String(value)
        }
        proto::ValueType(v) => {
            anyhow::bail!("invalid value type {v:#x}")
        }
    };

    Ok(KeyValue { key, value })
}

fn parse_ip_info_response(response: &[u8]) -> Result<IpInfo, anyhow::Error> {
    fn parse_ipv4(
        addresses: &[proto::IpAddressV4],
        count: u32,
    ) -> anyhow::Result<impl Iterator<Item = std::net::Ipv4Addr> + '_> {
        anyhow::Ok(
            addresses
                .get(..count as usize)
                .context("invalid ipv4 address count")?
                .iter()
                .map(|x| x.0.into()),
        )
    }
    fn parse_ipv6(
        addresses: &[proto::IpAddressV6],
        count: u32,
    ) -> anyhow::Result<impl Iterator<Item = std::net::Ipv6Addr> + '_> {
        anyhow::Ok(
            addresses
                .get(..count as usize)
                .context("invalid ipv6 address count")?
                .iter()
                .map(|x| x.0.into()),
        )
    }
    fn parse_origin(origin: proto::IpAddressOrigin) -> AddressOrigin {
        match origin {
            proto::IpAddressOrigin::UNKNOWN => AddressOrigin::Unknown,
            proto::IpAddressOrigin::OTHER => AddressOrigin::Other,
            proto::IpAddressOrigin::STATIC => AddressOrigin::Static,
            _ => AddressOrigin::Unknown,
        }
    }

    let response = parse_response::<proto::MessageIpAddressInfoBinary>(response)?;
    let (ipv4_origins, rest) = response
        .ip_address_origins
        .split_at_checked(response.ipv4_address_count as usize)
        .context("invalid ipv4 address count")?;

    let ipv6_origins = rest
        .get(..response.ipv6_address_count as usize)
        .context("invalid ipv6 address count")?;

    let info = IpInfo {
        ipv4: matches!(
            response.address_family,
            proto::AddressFamily::IPV4 | proto::AddressFamily::IPV4V6
        ),
        ipv6: matches!(
            response.address_family,
            proto::AddressFamily::IPV6 | proto::AddressFamily::IPV4V6
        ),
        dhcp_enabled: response.dhcp_enabled != 0,
        ipv4_addresses: parse_ipv4(&response.ipv4_addresses, response.ipv4_address_count)?
            .zip(parse_ipv4(
                &response.ipv4_subnets,
                response.ipv4_address_count,
            )?)
            .zip(ipv4_origins)
            .map(|((address, subnet), &origin)| Ipv4AddressInfo {
                address,
                subnet,
                origin: parse_origin(origin),
            })
            .collect(),
        ipv6_addresses: parse_ipv6(&response.ipv6_addresses, response.ipv6_address_count)?
            .zip(
                response
                    .ipv6_subnets
                    .get(..response.ipv6_address_count as usize)
                    .context("invalid ipv6 address count")?,
            )
            .zip(ipv6_origins)
            .map(|((address, &subnet), &origin)| Ipv6AddressInfo {
                address,
                subnet,
                origin: parse_origin(origin),
            })
            .collect(),
        ipv4_dns_servers: parse_ipv4(&response.ipv4_dns_servers, response.ipv4_dns_server_count)?
            .collect(),
        ipv6_dns_servers: parse_ipv6(&response.ipv6_dns_servers, response.ipv6_dns_server_count)?
            .collect(),
        ipv4_gateways: parse_ipv4(&response.ipv4_gateways, response.ipv4_gateway_count)?.collect(),
        ipv6_gateways: parse_ipv6(&response.ipv6_gateways, response.ipv6_gateway_count)?.collect(),
    };

    Ok(info)
}

fn prepare_get_ip_info(
    versions: &Versions,
    adapter_id: &str,
) -> anyhow::Result<Box<proto::KvpMessage2>> {
    if versions.message_version < proto::KVP_VERSION_5 {
        anyhow::bail!("non-binary protocol not supported");
    }
    let mut message = proto::MessageIpAddressInfoBinary::new_zeroed();
    write_str(&mut message.adapter_id, adapter_id)?;
    Ok(msg2(
        proto::KvpOperation::GET_IP_ADDRESS_INFO,
        proto::KvpPool::GUEST,
        message,
    ))
}

fn prepare_set_ip_info(
    versions: &Versions,
    adapter_id: &str,
    info: &IpInfo,
) -> anyhow::Result<Box<proto::KvpMessage2>> {
    if versions.message_version < proto::KVP_VERSION_5 {
        anyhow::bail!("non-binary protocol not supported");
    }
    let mut message = proto::MessageIpAddressInfoBinary::new_zeroed();
    write_str(&mut message.adapter_id, adapter_id)?;
    message.dhcp_enabled = info.dhcp_enabled as u8;
    message.address_family = match (info.ipv4, info.ipv6) {
        (true, true) => proto::AddressFamily::IPV4V6,
        (true, false) => proto::AddressFamily::IPV4,
        (false, true) => proto::AddressFamily::IPV6,
        (false, false) => proto::AddressFamily::NONE,
    };

    for ((da, ds), a) in message
        .ipv4_addresses
        .get_mut(..info.ipv4_addresses.len())
        .context("invalid ipv4 address count")?
        .iter_mut()
        .zip(&mut message.ipv4_subnets)
        .zip(&info.ipv4_addresses)
    {
        da.0 = a.address.octets();
        ds.0 = a.subnet.octets();
    }
    message.ipv4_address_count = info.ipv4_addresses.len() as u32;

    for ((da, ds), a) in message
        .ipv6_addresses
        .get_mut(..info.ipv6_addresses.len())
        .context("invalid ipv6 address count")?
        .iter_mut()
        .zip(&mut message.ipv6_subnets)
        .zip(&info.ipv6_addresses)
    {
        da.0 = a.address.octets();
        *ds = a.subnet;
    }
    message.ipv6_address_count = info.ipv6_addresses.len() as u32;

    for (da, a) in message
        .ipv4_gateways
        .get_mut(..info.ipv4_gateways.len())
        .context("invalid ipv4 gateway count")?
        .iter_mut()
        .zip(&info.ipv4_gateways)
    {
        da.0 = a.octets();
    }
    message.ipv4_gateway_count = info.ipv4_gateways.len() as u32;

    for (da, a) in message
        .ipv6_gateways
        .get_mut(..info.ipv6_gateways.len())
        .context("invalid ipv6 gateway count")?
        .iter_mut()
        .zip(&info.ipv6_gateways)
    {
        da.0 = a.octets();
    }
    message.ipv6_gateway_count = info.ipv6_gateways.len() as u32;

    for (da, a) in message
        .ipv4_dns_servers
        .get_mut(..info.ipv4_dns_servers.len())
        .context("invalid ipv4 dns server count")?
        .iter_mut()
        .zip(&info.ipv4_dns_servers)
    {
        da.0 = a.octets();
    }
    message.ipv4_dns_server_count = info.ipv4_dns_servers.len() as u32;

    for (da, a) in message
        .ipv6_dns_servers
        .get_mut(..info.ipv6_dns_servers.len())
        .context("invalid ipv6 dns server count")?
        .iter_mut()
        .zip(&info.ipv6_dns_servers)
    {
        da.0 = a.octets();
    }
    message.ipv6_dns_server_count = info.ipv6_dns_servers.len() as u32;

    Ok(msg2(
        proto::KvpOperation::SET_IP_ADDRESS_INFO,
        proto::KvpPool::GUEST,
        message,
    ))
}

#[derive(Debug, Error)]
#[error("KVP error: {0:x?}")]
struct RequestError(Status);

fn pool_cvt(pool: hyperv_ic_resources::kvp::KvpPool) -> proto::KvpPool {
    match pool {
        hyperv_ic_resources::kvp::KvpPool::External => proto::KvpPool::EXTERNAL,
        hyperv_ic_resources::kvp::KvpPool::Guest => proto::KvpPool::GUEST,
        hyperv_ic_resources::kvp::KvpPool::Auto => proto::KvpPool::AUTO,
        hyperv_ic_resources::kvp::KvpPool::AutoExternal => proto::KvpPool::AUTO_EXTERNAL,
    }
}

fn msg<T: IntoBytes + Immutable>(
    operation: proto::KvpOperation,
    pool: proto::KvpPool,
    message: T,
) -> Box<proto::KvpMessage> {
    let mut m = Box::new(proto::KvpMessage {
        header: proto::KvpHeader { operation, pool },
        data: [0; 2578],
    });
    let offset = align_of::<T>().saturating_sub(size_of::<proto::KvpHeader>());
    message.write_to_prefix(&mut m.data[offset..]).unwrap();
    m
}

fn msg2<T: IntoBytes + Immutable>(
    operation: proto::KvpOperation,
    pool: proto::KvpPool,
    message: T,
) -> Box<proto::KvpMessage2> {
    let mut m = Box::new(proto::KvpMessage2 {
        header: proto::KvpHeader { operation, pool },
        data: [0; 0x1d02],
    });
    let offset = align_of::<T>().saturating_sub(size_of::<proto::KvpHeader>());
    message.write_to_prefix(&mut m.data[offset..]).unwrap();
    m
}

fn parse_str(v: &[u16], n: u32) -> anyhow::Result<String> {
    if !n.is_multiple_of(2) {
        anyhow::bail!("invalid string length");
    }
    let v = v.get(..n as usize / 2).context("string length too large")?;
    if v.last() != Some(&0) {
        anyhow::bail!("missing null terminator");
    }
    String::from_utf16(&v[..v.len() - 1]).context("invalid utf-16")
}

fn write_str(v: &mut [u16], s: &str) -> anyhow::Result<u32> {
    let mut i = 0;
    for (s, d) in s.encode_utf16().zip(&mut *v) {
        *d = s;
        i += 1;
    }
    *v.get_mut(i).context("string too long")? = 0;
    Ok((i + 1) as u32 * 2)
}

fn write_value(buf: &mut [u8], v: &Value) -> anyhow::Result<(proto::ValueType, u32)> {
    let r = match *v {
        Value::String(ref s) => (
            proto::ValueType::STRING,
            write_str(<[u16]>::mut_from_bytes(buf).unwrap(), s)?,
        ),
        Value::U32(v) => {
            buf[..4].copy_from_slice(&v.to_le_bytes());
            (proto::ValueType::DWORD, 4)
        }
        Value::U64(v) => {
            buf[..8].copy_from_slice(&v.to_le_bytes());
            (proto::ValueType::QWORD, 8)
        }
    };
    Ok(r)
}
