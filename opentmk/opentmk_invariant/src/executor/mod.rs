// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::prelude::*;
use crate::{
    comms::SerialCommsServer,
    deserializer::{Deserializer, syzlang::SyzlangDeserializer},
    functions::{FunctionRegistry, hyperv},
    serial::{OpenTmkSerialIo, SerialIo, SerialPort},
};

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use crate::functions::io_port;

use inv_packet::{
    OpenTMKAckPacket, OpenTMKConfigurationPacket, OpenTMKErrorPacket, OpenTMKFuzzTest,
    OpenTMKGrammarDeserializer, OpenTMKPacket,
};

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
    DeserializerUnset,
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

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            static X86_REGISTRY: &[(&str, crate::functions::FuzzFunction)] = &[
                ("port_write8", io_port::write_ioport_u8),
                ("port_write16", io_port::write_ioport_u16),
                ("port_write32", io_port::write_ioport_u32),
                ("port_read8", io_port::read_ioport_u8),
                ("port_read16", io_port::read_ioport_u16),
                ("port_read32", io_port::read_ioport_u32),
            ];

            for (name, func) in X86_REGISTRY {
                fn_registry.register(name, *func);
            }
        };

        fn_registry.register("hvcall", hyperv::hvcall);
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
            OpenTMKPacket::Configuration(cfg) => self.on_receive_configuration_packet(&cfg)?,
            OpenTMKPacket::FuzzTest(mut fuzz) => self.on_receive_fuzz_test_packet(&mut fuzz)?,
            OpenTMKPacket::Ack(a) => self.on_receive_ack_packet(&a)?,
            OpenTMKPacket::Error(a) => self.on_receive_error_packet(&a)?,
        };

        if let Some(resp) = response_pkt {
            self.comms.write_packet_blocking(&resp)?;
        }

        Ok(())
    }

    pub fn on_receive_configuration_packet(
        &mut self,
        pkt: &OpenTMKConfigurationPacket,
    ) -> Result<Option<OpenTMKPacket>, ExecutorError> {
        self.deserializer_type = pkt.deserializer;

        match self.deserializer_type {
            OpenTMKGrammarDeserializer::None => Err(ExecutorError::NoDeserializerEnabled)?,
            OpenTMKGrammarDeserializer::SyzDecoder => {
                self.deserializer = Some(Box::new(SyzlangDeserializer::new()));
            }
        };

        self.deserializer
            .as_mut()
            .ok_or(ExecutorError::DeserializerUnset)?
            .set_function_registry(self.fn_registry.clone());

        self.deserializer
            .as_mut()
            .ok_or(ExecutorError::DeserializerUnset)?
            .set_mappings(pkt.mapping.clone())
            .map(|_| ())?;

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
        match self.deserializer.as_mut() {
            None => Err(ExecutorError::NoDeserializerEnabled),
            Some(t) => Ok(Some(match t.as_mut().deserialize_and_execute(pkt) {
                Ok(code) => OpenTMKPacket::Ack(OpenTMKAckPacket { code }),
                Err(e) => OpenTMKPacket::Error(OpenTMKErrorPacket {
                    message: format!("{:?}", e),
                }),
            })),
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
