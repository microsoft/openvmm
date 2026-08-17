// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
pub(crate) mod test;

use crate::executor::ExecutorError;
use inv_packet::{
    COMMS_ACK_MAGIC, COMMS_PACKET_FOOTER_MAGIC, COMMS_PACKET_HEADER_MAGIC, COMMS_SYN_ACK_MAGIC,
    COMMS_SYN_MAGIC, OpenTMKPacket,
};

#[cfg_attr(not(target_os = "uefi"), expect(unused_imports))]
use crate::prelude::*;

use opentmk_core::arch::serial::{InstrIoAccess, Serial, SerialPort};

pub(crate) trait SerialIo {
    fn init(&mut self);
    fn drain(&mut self);
    fn write_byte(&mut self, byte: u8);
    fn read_byte(&mut self) -> u8;
}

pub(crate) struct OpenTmkSerialIo {
    handle: Serial<InstrIoAccess>,
}

impl OpenTmkSerialIo {
    fn new(port: SerialPort) -> Self {
        log::info!("creating serial port");
        Self {
            handle: Serial::new(port, InstrIoAccess),
        }
    }
}

impl SerialIo for OpenTmkSerialIo {
    fn init(&mut self) {
        self.handle.init();
    }

    fn drain(&mut self) {
        self.handle.drain();
    }

    fn write_byte(&mut self, byte: u8) {
        self.handle.write_byte(byte);
    }

    fn read_byte(&mut self) -> u8 {
        self.handle.read_byte()
    }
}

pub(crate) struct SerialCommsServer<T> {
    pub(crate) handle: T,
    connected: bool,
}

impl SerialCommsServer<OpenTmkSerialIo> {
    pub fn new(port: SerialPort) -> Self {
        Self::new_with_transport(OpenTmkSerialIo::new(port))
    }
}

impl<T> SerialCommsServer<T> {
    pub(crate) fn new_with_transport(handle: T) -> Self {
        Self {
            handle,
            connected: false,
        }
    }
}

impl<T: SerialIo> SerialCommsServer<T> {
    pub fn handshake(&mut self) -> Result<(), ExecutorError> {
        if self.connected {
            return Ok(());
        }

        self.handle.init();

        // drain any data in the serial FIFO
        self.handle.drain();

        //1. Read a QWord from the host, it should be SerialCommsSynMagic
        let syn = self.read_u64_blocking();
        if syn != COMMS_SYN_MAGIC {
            log::error!("0x{:016x}", syn);
            return Err(ExecutorError::HandshakeInvalidSynMagic);
        }

        //2. Send SerialCommsSynAckMagic to host
        self.send_u64_blocking(COMMS_SYN_ACK_MAGIC);

        //3. Read a QWORD from the host, it should be SerialCommsAckMagic
        let ack = self.read_u64_blocking();
        if ack != COMMS_ACK_MAGIC {
            log::error!("0x{:016x}", ack);
            return Err(ExecutorError::HandshakeInvalidSynAckMagic);
        }

        self.connected = true;
        log::info!("Serial Comms sucessfully connected");

        Ok(())
    }

    pub fn read_packet_blocking(&mut self) -> Result<OpenTMKPacket, ExecutorError> {
        //1. Read packet header magic
        let magic = self.read_u64_blocking();
        if magic != COMMS_PACKET_HEADER_MAGIC {
            log::error!("0x{:016x}", magic);
            return Err(ExecutorError::PacketInvalidHeaderMagic);
        }

        //2. Read packet size
        let payload_sz = self.read_u64_blocking();

        //3. Read payload
        let mut buffer: Vec<u8> = Vec::with_capacity(payload_sz as usize);

        for _ in 0..payload_sz {
            buffer.push(self.read_u8_blocking());
        }

        //4. Read packet footer magic
        let footer = self.read_u64_blocking();
        if footer != COMMS_PACKET_FOOTER_MAGIC {
            log::error!("0x{:016x}", footer);
            return Err(ExecutorError::PacketInvalidFooterMagic);
        }

        let packet: OpenTMKPacket = match postcard::from_bytes(&buffer) {
            Ok(p) => p,
            Err(_) => return Err(ExecutorError::PacketPayloadDeserializeFailed),
        };

        Ok(packet)
    }

    pub fn write_packet_blocking(&mut self, pkt: &OpenTMKPacket) -> Result<(), ExecutorError> {
        let data = match postcard::to_allocvec(&pkt) {
            Ok(p) => p,
            Err(_) => return Err(ExecutorError::PacketPayloadSerializeFailed),
        };

        //1. Write packet header magic
        self.send_u64_blocking(COMMS_PACKET_HEADER_MAGIC);

        //2. Write packet size
        self.send_u64_blocking(data.len() as u64);

        //3. Write payload
        for byte in data {
            self.send_u8_blocking(byte);
        }

        //4. Write packet footer magic
        self.send_u64_blocking(COMMS_PACKET_FOOTER_MAGIC);

        Ok(())
    }

    fn send_u8_blocking(&mut self, d: u8) {
        self.handle.write_byte(d);
    }

    fn send_u64_blocking(&mut self, d: u64) {
        for i in 0..8 {
            self.send_u8_blocking((d >> (i * 8)) as u8);
        }
    }

    fn read_u8_blocking(&mut self) -> u8 {
        self.handle.read_byte()
    }

    fn read_u64_blocking(&mut self) -> u64 {
        let mut val: u64 = 0;
        for i in 0..8 {
            let byte = self.read_u8_blocking() as u64;
            val |= byte << (i * 8);
        }
        val
    }
}
