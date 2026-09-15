// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::comms::SerialCommsServer;
use crate::deserializer::Deserializer;
use crate::deserializer::syzlang::SyzlangDeserializer;
use crate::functions::FunctionRegistry;
use crate::functions::FuzzFunction;
use crate::functions::hyperv;
#[cfg(target_arch = "x86_64")]
use crate::functions::io_port;
use crate::functions::netvsp;
use crate::functions::vmbus;
use crate::prelude::*;

use cfg_if::cfg_if;
use opentmk_exec_packet::OpenTMKAckPacket;
use opentmk_exec_packet::OpenTMKConfigurationPacket;
use opentmk_exec_packet::OpenTMKErrorPacket;
use opentmk_exec_packet::OpenTMKFuzzTest;
use opentmk_exec_packet::OpenTMKGrammarDeserializer;
use opentmk_exec_packet::OpenTMKPacket;

use spin::Mutex;

#[cfg(test)]
mod test;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExecutorError {
    HandshakeInvalidSynMagic,
    HandshakeInvalidSynAckMagic,
    PacketInvalidHeaderMagic,
    PacketInvalidFooterMagic,
    PacketPayloadDeserializeFailed,
    PacketPayloadSerializeFailed,
    UnexpectedPacketReceived,
    NoDeserializerEnabled,
    SyzlangDeserializerFailed(String),
    DecoderMappingsDeserializeFailed,
}

pub(crate) struct Executor<T = OpenTmkSerialIo> {
    comms: SerialCommsServer<T>,
    deserializer_type: OpenTMKGrammarDeserializer,
    deserializer: Option<Box<dyn Deserializer>>,
    fn_registry: Arc<Mutex<FunctionRegistry>>,
}

impl Executor<OpenTmkSerialIo> {
    pub fn new(port: SerialPort) -> Self {
        Self::new_with_comms(SerialCommsServer::new(port))
    }
}

impl<T: SerialIo> Executor<T> {
    pub(crate) fn new_with_comms(comms: SerialCommsServer<T>) -> Self {
        Self {
            comms,
            deserializer_type: OpenTMKGrammarDeserializer::None,
            deserializer: None,
            fn_registry: Default::default(),
        }
    }

    pub fn initialize(&mut self) -> Result<(), ExecutorError> {
        self.comms.handshake()?;
        Ok(())
    }

    pub fn register_fuzz_functions(&mut self) {
        let mut fn_registry = self.fn_registry.lock();
        cfg_if!{
            if #[cfg(target_arch = "x86_64")] {
                static REGISTRY_ARCH: &[(&str, FuzzFunction)] = &[
                    ("port_write8", io_port::write_ioport_u8),
                    ("port_write16", io_port::write_ioport_u16),
                    ("port_write32", io_port::write_ioport_u32),
                    ("port_read8", io_port::read_ioport_u8),
                    ("port_read16", io_port::read_ioport_u16),
                    ("port_read32", io_port::read_ioport_u32),
                ];
            } else {
                static REGISTRY_ARCH: &[(&str, FuzzFunction)] = &[];
            }
        }

        static REGISTRY: &[(&str, FuzzFunction)] = &[
            ("hvcall", hyperv::hvcall),
            ("send_nvsp", netvsp::send_nvsp),
            ("send_rndis", netvsp::send_rndis),
            ("open_channel", netvsp::open_channel),
            ("renew_buffer", netvsp::renew_buffer),
            ("vmbus_msg", vmbus::vmbus_msg),
            ("vmbus_msg_comp", vmbus::vmbus_msg_comp),
            ("vmbus_packet", vmbus::vmbus_packet),
            ("vmbus_reopen_channel", vmbus::vmbus_reopen_channel),
            ("vmbus_fill_relids", vmbus::vmbus_fill_relids),
        ];

        for (name, func) in REGISTRY_ARCH.iter().chain(REGISTRY) {
            fn_registry.register(name, *func);
        }
    }

    pub fn run(&mut self) -> Result<(), ExecutorError> {
        // enter the main run loop
        loop {
            self.process_next_packet()?;
        }
    }

    pub(crate) fn process_next_packet(&mut self) -> Result<(), ExecutorError> {
        let pkt = self.comms.read_packet_blocking()?;

        let response_pkt = match pkt {
            OpenTMKPacket::Configuration(cfg) => self.on_receive_configuration_packet(&cfg),
            OpenTMKPacket::FuzzTest(mut fuzz) => self.on_receive_fuzz_test_packet(&mut fuzz),
            OpenTMKPacket::Ack(a) => self.on_receive_ack_packet(&a),
            OpenTMKPacket::Error(a) => self.on_receive_error_packet(&a),
        };

        match response_pkt {
            Ok(None) => (),
            Ok(Some(pkt)) => self.comms.write_packet_blocking(&pkt)?,
            Err(e) => {
                // Attempt to send an error packet. Once we do so bail and exit
                self.comms
                    .write_packet_blocking(&OpenTMKPacket::Error(OpenTMKErrorPacket {
                        message: format!("{e:?}"),
                    }))?;
                return Err(e);
            }
        }

        Ok(())
    }

    pub fn on_receive_configuration_packet(
        &mut self,
        pkt: &OpenTMKConfigurationPacket,
    ) -> Result<Option<OpenTMKPacket>, ExecutorError> {
        let mut deserializer = match pkt.deserializer {
            OpenTMKGrammarDeserializer::None => Err(ExecutorError::NoDeserializerEnabled)?,
            OpenTMKGrammarDeserializer::SyzDecoder => Box::new(SyzlangDeserializer::new()),
        };

        deserializer.set_function_registry(self.fn_registry.clone());
        deserializer.set_mappings(pkt.mapping.clone())?;

        self.deserializer_type = pkt.deserializer;
        self.deserializer = Some(deserializer);

        log::info!(
            "Setting active deserializer to {:?}",
            self.deserializer_type
        );
        Ok(Some(OpenTMKPacket::Ack(OpenTMKAckPacket { code: 0 })))
    }

    pub fn on_receive_fuzz_test_packet(
        &mut self,
        pkt: &mut OpenTMKFuzzTest,
    ) -> Result<Option<OpenTMKPacket>, ExecutorError> {
        // Isolate each testcase: tear down any existing netvsp data path
        // so the next handler call rebuilds a clean one. Prevents state
        // (a wedged send ring, a revoked buffer, a mutated RNDIS filter)
        // from leaking across testcases and recovers a datapath a prior
        // testcase wedged. Lazy: no-op when no session exists yet.
        netvsp::reset_session();
        vmbus::reset_session();
        match self.deserializer.as_mut() {
            None => Err(ExecutorError::NoDeserializerEnabled),
            Some(t) => Ok(Some(OpenTMKPacket::Ack(OpenTMKAckPacket {
                code: t.as_mut().deserialize_and_execute(pkt)?,
            }))),
        }
    }

    pub fn on_receive_ack_packet(
        &mut self,
        _: &OpenTMKAckPacket,
    ) -> Result<Option<OpenTMKPacket>, ExecutorError> {
        log::error!("Received ACK packet incorrectly");
        Err(ExecutorError::UnexpectedPacketReceived)
    }

    pub fn on_receive_error_packet(
        &mut self,
        _: &OpenTMKErrorPacket,
    ) -> Result<Option<OpenTMKPacket>, ExecutorError> {
        log::error!("Received error packet incorrectly");
        Err(ExecutorError::UnexpectedPacketReceived)
    }
}
