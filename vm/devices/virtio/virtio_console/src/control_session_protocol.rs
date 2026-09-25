// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Frozen outer protocol for the control-session broker.

use core::fmt;
use thiserror::Error;

pub const HEADER_LEN: usize = 44;
pub const MAX_DATA_LEN: usize = 65_536;
pub const MIN_RECEIVE_CREDIT: u32 = MAX_DATA_LEN as u32;
pub const MAX_RECEIVE_CREDIT: u32 = 4 * 1024 * 1024 + 10;
const MAGIC: [u8; 4] = *b"NVXS";
const VERSION: u16 = 1;

/// A control-session record type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RecordType {
    GuestAttach = 1,
    HostAttach = 2,
    Reset = 3,
    Ack = 4,
    Data = 5,
    Wait = 6,
    Ready = 7,
    Error = 8,
    Credit = 9,
}

impl TryFrom<u8> for RecordType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::GuestAttach),
            2 => Ok(Self::HostAttach),
            3 => Ok(Self::Reset),
            4 => Ok(Self::Ack),
            5 => Ok(Self::Data),
            6 => Ok(Self::Wait),
            7 => Ok(Self::Ready),
            8 => Ok(Self::Error),
            9 => Ok(Self::Credit),
            _ => Err(ProtocolError::InvalidType(value)),
        }
    }
}

/// A protocol-v1 Error record code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum ErrorCode {
    /// The host failed capability authentication.
    Authentication = 1,
}

impl TryFrom<u32> for ErrorCode {
    type Error = ProtocolError;

    fn try_from(value: u32) -> Result<Self, ProtocolError> {
        match value {
            1 => Ok(Self::Authentication),
            _ => Err(ProtocolError::InvalidErrorCode(value)),
        }
    }
}

/// A validated protocol-v1 record.
#[derive(Clone, Eq, PartialEq)]
pub struct Record {
    pub record_type: RecordType,
    pub instance_id: [u8; 16],
    pub epoch: u64,
    pub sequence: u64,
    pub payload: Vec<u8>,
}

impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Record")
            .field("record_type", &self.record_type)
            .field("instance_id", &self.instance_id)
            .field("epoch", &self.epoch)
            .field("sequence", &self.sequence)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl Record {
    /// Creates a bootstrap record. Bootstrap records never consume a sequence.
    #[cfg_attr(not(test), expect(dead_code, reason = "used by protocol tests"))]
    pub fn bootstrap(record_type: RecordType, payload: Vec<u8>) -> Self {
        Self {
            record_type,
            instance_id: [0; 16],
            epoch: 0,
            sequence: 0,
            payload,
        }
    }

    /// Creates a post-bootstrap record.
    pub fn session(
        record_type: RecordType,
        instance_id: [u8; 16],
        epoch: u64,
        sequence: u64,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            record_type,
            instance_id,
            epoch,
            sequence,
            payload,
        }
    }
}

/// Protocol decoding and validation failures.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProtocolError {
    #[error("invalid control-session magic")]
    InvalidMagic,
    #[error("unsupported control-session version {0}")]
    InvalidVersion(u16),
    #[error("unknown control-session record type {0}")]
    InvalidType(u8),
    #[error("control-session flags must be zero, got {0}")]
    InvalidFlags(u8),
    #[error("invalid payload length {length} for {record_type:?}")]
    InvalidPayloadLength {
        record_type: RecordType,
        length: u32,
    },
    #[error("invalid control-session error code {0}")]
    InvalidErrorCode(u32),
    #[error("invalid control-session receive credit {0}")]
    InvalidReceiveCredit(u32),
    #[error("bootstrap record contains session identity")]
    InvalidBootstrapIdentity,
    #[error("invalid parser snapshot: {0}")]
    InvalidSnapshot(&'static str),
    #[error("encoded record length overflows the platform size")]
    LengthOverflow,
}

/// Serializable-neutral state for an incremental parser.
#[derive(Clone, Eq, PartialEq)]
pub struct ParserSnapshot {
    pub header_bytes: Vec<u8>,
    pub header_count: usize,
    pub body_bytes: Vec<u8>,
    pub declared_body_len: Option<u32>,
}

impl fmt::Debug for ParserSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParserSnapshot")
            .field("header_count", &self.header_count)
            .field("body_len", &self.body_bytes.len())
            .field("declared_body_len", &self.declared_body_len)
            .finish()
    }
}

/// The result of accepting bytes into a parser.
#[derive(Debug, Eq, PartialEq)]
pub struct ParseProgress {
    pub consumed: usize,
    pub record: Option<Record>,
}

#[derive(Clone, Copy)]
struct ParsedHeader {
    record_type: RecordType,
    instance_id: [u8; 16],
    epoch: u64,
    sequence: u64,
    payload_len: usize,
}

/// An aligned, bounded incremental protocol parser.
#[derive(Clone)]
pub struct Parser {
    header: [u8; HEADER_LEN],
    header_count: usize,
    body: Vec<u8>,
    declared_body_len: Option<usize>,
}

impl fmt::Debug for Parser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parser")
            .field("header_count", &self.header_count)
            .field("body_len", &self.body.len())
            .field("declared_body_len", &self.declared_body_len)
            .finish()
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            header: [0; HEADER_LEN],
            header_count: 0,
            body: Vec::new(),
            declared_body_len: None,
        }
    }

    /// Accepts bytes up to and including one complete record.
    ///
    /// Any bytes after the returned record remain unconsumed with the caller.
    pub fn accept(&mut self, input: &[u8]) -> Result<ParseProgress, ProtocolError> {
        let mut consumed = 0;
        if self.header_count < HEADER_LEN {
            let take = (HEADER_LEN - self.header_count).min(input.len());
            self.header[self.header_count..self.header_count + take]
                .copy_from_slice(&input[..take]);
            self.header_count += take;
            consumed += take;
            if self.header_count < HEADER_LEN {
                return Ok(ParseProgress {
                    consumed,
                    record: None,
                });
            }

            let header = parse_header(&self.header)?;
            self.declared_body_len = Some(header.payload_len);
            if header.payload_len == 0 {
                let record = record_from_parts(header, Vec::new())?;
                self.reset();
                return Ok(ParseProgress {
                    consumed,
                    record: Some(record),
                });
            }
            self.body = Vec::with_capacity(header.payload_len);
        }

        let declared = self
            .declared_body_len
            .ok_or(ProtocolError::InvalidSnapshot(
                "missing declared body length",
            ))?;
        let remaining =
            declared
                .checked_sub(self.body.len())
                .ok_or(ProtocolError::InvalidSnapshot(
                    "body exceeds declared length",
                ))?;
        let available = &input[consumed..];
        let take = remaining.min(available.len());
        self.body.extend_from_slice(&available[..take]);
        consumed += take;
        if self.body.len() != declared {
            return Ok(ParseProgress {
                consumed,
                record: None,
            });
        }

        let header = parse_header(&self.header)?;
        let body = core::mem::take(&mut self.body);
        let record = record_from_parts(header, body)?;
        self.reset();
        Ok(ParseProgress {
            consumed,
            record: Some(record),
        })
    }

    pub fn snapshot(&self) -> ParserSnapshot {
        ParserSnapshot {
            header_bytes: self.header.to_vec(),
            header_count: self.header_count,
            body_bytes: self.body.clone(),
            declared_body_len: self.declared_body_len.map(|len| len as u32),
        }
    }

    pub fn validate_snapshot(snapshot: &ParserSnapshot) -> Result<(), ProtocolError> {
        if snapshot.header_bytes.len() != HEADER_LEN {
            return Err(ProtocolError::InvalidSnapshot(
                "header storage must be exactly 44 bytes",
            ));
        }
        if snapshot.header_count > HEADER_LEN {
            return Err(ProtocolError::InvalidSnapshot(
                "header count exceeds header length",
            ));
        }
        if snapshot.header_count < HEADER_LEN {
            if snapshot.declared_body_len.is_some() || !snapshot.body_bytes.is_empty() {
                return Err(ProtocolError::InvalidSnapshot(
                    "partial header has body state",
                ));
            }
        } else {
            let mut header = [0; HEADER_LEN];
            header.copy_from_slice(&snapshot.header_bytes);
            let parsed = parse_header(&header)?;
            let declared = snapshot
                .declared_body_len
                .ok_or(ProtocolError::InvalidSnapshot(
                    "complete header has no declared body length",
                ))?;
            if declared as usize != parsed.payload_len {
                return Err(ProtocolError::InvalidSnapshot(
                    "declared body length disagrees with header",
                ));
            }
            if parsed.payload_len == 0 || snapshot.body_bytes.len() >= parsed.payload_len {
                return Err(ProtocolError::InvalidSnapshot(
                    "complete record cannot remain in parser state",
                ));
            }
        }
        Ok(())
    }

    /// Restores parser state after validating all bounds before allocation.
    pub fn restore(snapshot: ParserSnapshot) -> Result<Self, ProtocolError> {
        Self::validate_snapshot(&snapshot)?;

        let mut header = [0; HEADER_LEN];
        header.copy_from_slice(&snapshot.header_bytes);
        let body_capacity = snapshot.declared_body_len.unwrap_or(0) as usize;
        let mut body = Vec::with_capacity(body_capacity);
        body.extend_from_slice(&snapshot.body_bytes);
        Ok(Self {
            header,
            header_count: snapshot.header_count,
            body,
            declared_body_len: snapshot.declared_body_len.map(|len| len as usize),
        })
    }

    pub fn is_aligned(&self) -> bool {
        self.header_count == 0
    }

    fn reset(&mut self) {
        self.header = [0; HEADER_LEN];
        self.header_count = 0;
        self.body.clear();
        self.declared_body_len = None;
    }
}

/// Encodes a validated record using the frozen 44-byte header.
pub fn encode(record: &Record) -> Result<Vec<u8>, ProtocolError> {
    validate_record(record)?;
    let payload_len =
        u32::try_from(record.payload.len()).map_err(|_| ProtocolError::LengthOverflow)?;
    let total_len = HEADER_LEN
        .checked_add(record.payload.len())
        .ok_or(ProtocolError::LengthOverflow)?;
    let mut bytes = Vec::with_capacity(total_len);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.push(record.record_type as u8);
    bytes.push(0);
    bytes.extend_from_slice(&record.instance_id);
    bytes.extend_from_slice(&record.epoch.to_le_bytes());
    bytes.extend_from_slice(&record.sequence.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&record.payload);
    Ok(bytes)
}

/// Decodes exactly one complete record.
pub fn decode_exact(bytes: &[u8]) -> Result<Record, ProtocolError> {
    let mut parser = Parser::new();
    let progress = parser.accept(bytes)?;
    if progress.consumed != bytes.len() || !parser.is_aligned() {
        return Err(ProtocolError::InvalidSnapshot(
            "encoded record is truncated or contains trailing bytes",
        ));
    }
    progress.record.ok_or(ProtocolError::InvalidSnapshot(
        "encoded record is incomplete",
    ))
}

fn parse_header(bytes: &[u8; HEADER_LEN]) -> Result<ParsedHeader, ProtocolError> {
    if bytes[..4] != MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(ProtocolError::InvalidVersion(version));
    }
    let record_type = RecordType::try_from(bytes[6])?;
    if bytes[7] != 0 {
        return Err(ProtocolError::InvalidFlags(bytes[7]));
    }
    let mut instance_id = [0; 16];
    instance_id.copy_from_slice(&bytes[8..24]);
    let epoch = u64::from_le_bytes(bytes[24..32].try_into().map_err(|_| {
        ProtocolError::InvalidSnapshot("epoch field has an invalid encoded length")
    })?);
    let sequence = u64::from_le_bytes(bytes[32..40].try_into().map_err(|_| {
        ProtocolError::InvalidSnapshot("sequence field has an invalid encoded length")
    })?);
    let payload_len = u32::from_le_bytes(bytes[40..44].try_into().map_err(|_| {
        ProtocolError::InvalidSnapshot("payload field has an invalid encoded length")
    })?);
    validate_payload_length(record_type, payload_len)?;
    if matches!(
        record_type,
        RecordType::GuestAttach | RecordType::HostAttach
    ) && (instance_id != [0; 16] || epoch != 0 || sequence != 0)
    {
        return Err(ProtocolError::InvalidBootstrapIdentity);
    }
    Ok(ParsedHeader {
        record_type,
        instance_id,
        epoch,
        sequence,
        payload_len: payload_len as usize,
    })
}

fn record_from_parts(header: ParsedHeader, payload: Vec<u8>) -> Result<Record, ProtocolError> {
    let record = Record {
        record_type: header.record_type,
        instance_id: header.instance_id,
        epoch: header.epoch,
        sequence: header.sequence,
        payload,
    };
    validate_record(&record)?;
    Ok(record)
}

fn validate_record(record: &Record) -> Result<(), ProtocolError> {
    let payload_len =
        u32::try_from(record.payload.len()).map_err(|_| ProtocolError::LengthOverflow)?;
    validate_payload_length(record.record_type, payload_len)?;
    if matches!(
        record.record_type,
        RecordType::GuestAttach | RecordType::HostAttach
    ) && (record.instance_id != [0; 16] || record.epoch != 0 || record.sequence != 0)
    {
        return Err(ProtocolError::InvalidBootstrapIdentity);
    }
    if record.record_type == RecordType::Error {
        let code_bytes: [u8; 4] = record.payload.as_slice().try_into().map_err(|_| {
            ProtocolError::InvalidPayloadLength {
                record_type: RecordType::Error,
                length: payload_len,
            }
        })?;
        ErrorCode::try_from(u32::from_le_bytes(code_bytes))?;
    }
    match record.record_type {
        RecordType::Ack => {
            let credit = decode_credit_payload(&record.payload)?;
            if !(MIN_RECEIVE_CREDIT..=MAX_RECEIVE_CREDIT).contains(&credit) {
                return Err(ProtocolError::InvalidReceiveCredit(credit));
            }
        }
        RecordType::Credit => {
            let credit = decode_credit_payload(&record.payload)?;
            if !(1..=MAX_RECEIVE_CREDIT).contains(&credit) {
                return Err(ProtocolError::InvalidReceiveCredit(credit));
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_payload_length(record_type: RecordType, payload_len: u32) -> Result<(), ProtocolError> {
    let valid = match record_type {
        RecordType::Ack | RecordType::Credit => payload_len == 4,
        RecordType::GuestAttach | RecordType::Reset | RecordType::Wait | RecordType::Ready => {
            payload_len == 0
        }
        RecordType::HostAttach => payload_len == 32,
        RecordType::Data => (1..=MAX_DATA_LEN as u32).contains(&payload_len),
        RecordType::Error => payload_len == 4,
    };
    if valid {
        Ok(())
    } else {
        Err(ProtocolError::InvalidPayloadLength {
            record_type,
            length: payload_len,
        })
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by broker tests and guest codec")
)]
pub fn credit_payload(credit: u32) -> Vec<u8> {
    credit.to_le_bytes().to_vec()
}

pub fn decode_credit_payload(payload: &[u8]) -> Result<u32, ProtocolError> {
    let bytes: [u8; 4] = payload
        .try_into()
        .map_err(|_| ProtocolError::InvalidPayloadLength {
            record_type: RecordType::Credit,
            length: u32::try_from(payload.len()).unwrap_or(u32::MAX),
        })?;
    Ok(u32::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn sample(record_type: RecordType, payload: Vec<u8>) -> Record {
        if matches!(
            record_type,
            RecordType::GuestAttach | RecordType::HostAttach
        ) {
            Record::bootstrap(record_type, payload)
        } else {
            Record::session(record_type, [0x11; 16], 7, 9, payload)
        }
    }

    fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
        if !hex.len().is_multiple_of(2) {
            return Err("odd hex length".into());
        }
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = core::str::from_utf8(pair).map_err(|error| error.to_string())?;
                u8::from_str_radix(text, 16).map_err(|error| error.to_string())
            })
            .collect()
    }

    #[test]
    fn golden_vectors_round_trip() -> Result<(), Box<dyn std::error::Error>> {
        let vectors = include_str!("../test_data/control_session_protocol_vectors.txt");
        let mut count = 0;
        for line in vectors.lines().filter(|line| !line.is_empty()) {
            let (name, hex) = line
                .split_once('|')
                .ok_or_else(|| format!("invalid vector line: {line}"))?;
            let bytes = decode_hex(hex)?;
            let record = decode_exact(&bytes)?;
            let expected_type = match name {
                "GUEST_ATTACH" => RecordType::GuestAttach,
                "HOST_ATTACH" => RecordType::HostAttach,
                "RESET" => RecordType::Reset,
                "ACK_INITIAL_CREDIT" => RecordType::Ack,
                "CREDIT" => RecordType::Credit,
                "DATA_EMBEDDED_MAGIC" => RecordType::Data,
                "WAIT" => RecordType::Wait,
                "READY" => RecordType::Ready,
                "ERROR_AUTHENTICATION" => RecordType::Error,
                _ => return Err(format!("unknown vector name: {name}").into()),
            };
            assert_eq!(record.record_type, expected_type, "{name}");
            if matches!(
                expected_type,
                RecordType::GuestAttach | RecordType::HostAttach
            ) {
                assert_eq!(record.instance_id, [0; 16], "{name}");
                assert_eq!((record.epoch, record.sequence), (0, 0), "{name}");
            } else {
                assert_eq!(
                    record.instance_id,
                    core::array::from_fn(|index| index as u8),
                    "{name}"
                );
                assert_eq!((record.epoch, record.sequence), (7, 9), "{name}");
            }
            if expected_type == RecordType::HostAttach {
                assert_eq!(
                    record.payload,
                    (0..32).map(|value| value as u8).collect::<Vec<_>>()
                );
            }
            if expected_type == RecordType::Data {
                assert_eq!(record.payload, b"NVXSNVXS");
            }
            if expected_type == RecordType::Error {
                let code = u32::from_le_bytes(record.payload.as_slice().try_into()?);
                assert_eq!(ErrorCode::try_from(code)?, ErrorCode::Authentication);
            }
            if expected_type == RecordType::Ack {
                assert_eq!(decode_credit_payload(&record.payload)?, MAX_RECEIVE_CREDIT);
            }
            if expected_type == RecordType::Credit {
                assert_eq!(decode_credit_payload(&record.payload)?, MIN_RECEIVE_CREDIT);
            }
            assert_eq!(encode(&record)?, bytes, "{name}");
            count += 1;
        }
        assert_eq!(count, 9);
        assert!(vectors.contains("4e5658534e565853"));
        Ok(())
    }

    #[test]
    fn language_neutral_boundary_and_invalid_cases() -> Result<(), Box<dyn std::error::Error>> {
        const EXPECTED_CASE_NAMES: [&str; 37] = [
            "VALID_MIN",
            "VALID_MAX",
            "INVALID_EMPTY",
            "INVALID_OVERSIZE",
            "VALID_MIN",
            "VALID_MAX",
            "INVALID_BELOW_MIN",
            "INVALID_ABOVE_MAX",
            "VALID_MIN",
            "VALID_MAX",
            "INVALID_ZERO",
            "INVALID_ABOVE_MAX",
            "INVALID_MAGIC",
            "INVALID_VERSION",
            "INVALID_TYPE",
            "INVALID_FLAGS",
            "INVALID_DATA_LENGTH",
            "AUTHENTICATION",
            "INVALID_GUEST_ATTACH_PAYLOAD",
            "INVALID_HOST_ATTACH_SHORT",
            "INVALID_HOST_ATTACH_LONG",
            "INVALID_RESET_PAYLOAD",
            "INVALID_ACK_PAYLOAD",
            "INVALID_WAIT_PAYLOAD",
            "INVALID_READY_PAYLOAD",
            "INVALID_ERROR_SHORT",
            "INVALID_ERROR_LONG",
            "INVALID_GUEST_ATTACH_INSTANCE",
            "INVALID_HOST_ATTACH_EPOCH",
            "INVALID_HOST_ATTACH_SEQUENCE",
            "INVALID_ERROR_CODE_0",
            "INVALID_ERROR_CODE_2",
            "INVALID_ERROR_CODE_3",
            "INVALID_ERROR_CODE_4",
            "INVALID_ERROR_CODE_5",
            "INVALID_ERROR_CODE_6",
            "INVALID_ERROR_CODE_MAX",
        ];

        let cases = include_str!("../test_data/control_session_protocol_cases.txt");
        let instance_id = core::array::from_fn(|index| index as u8);
        let mut names = Vec::new();
        for line in cases.lines().filter(|line| !line.is_empty()) {
            let fields = line.split('|').collect::<Vec<_>>();
            match fields.as_slice() {
                ["DATA_LENGTH", name, length] => {
                    names.push(*name);
                    let length = length.parse::<usize>()?;
                    let result = encode(&Record::session(
                        RecordType::Data,
                        instance_id,
                        7,
                        9,
                        vec![0x5a; length],
                    ));
                    match *name {
                        "VALID_MIN" | "VALID_MAX" => assert!(result.is_ok(), "{name}"),
                        "INVALID_EMPTY" => assert!(matches!(
                            result,
                            Err(ProtocolError::InvalidPayloadLength { length: 0, .. })
                        )),
                        "INVALID_OVERSIZE" => assert!(matches!(
                            result,
                            Err(ProtocolError::InvalidPayloadLength { length: 65_537, .. })
                        )),
                        _ => return Err(format!("unknown data-length case: {name}").into()),
                    }
                }
                ["INVALID_HEADER", name, hex] => {
                    names.push(*name);
                    let bytes = decode_hex(hex)?;
                    assert_eq!(bytes.len(), HEADER_LEN, "{name}");
                    let mut parser = Parser::new();
                    let error = parser.accept(&bytes).expect_err(name);
                    let intended = matches!(
                        (*name, &error),
                        ("INVALID_MAGIC", ProtocolError::InvalidMagic)
                            | ("INVALID_VERSION", ProtocolError::InvalidVersion(2))
                            | ("INVALID_TYPE", ProtocolError::InvalidType(99))
                            | ("INVALID_FLAGS", ProtocolError::InvalidFlags(1))
                            | (
                                "INVALID_DATA_LENGTH",
                                ProtocolError::InvalidPayloadLength {
                                    record_type: RecordType::Data,
                                    length: 65_537,
                                },
                            )
                    );
                    assert!(intended, "{name}: unexpected error {error:?}");
                }
                ["ERROR_CODE", name, value] => {
                    names.push(*name);
                    let value = value.parse::<u32>()?;
                    match *name {
                        "AUTHENTICATION" => {
                            assert_eq!(ErrorCode::try_from(value)?, ErrorCode::Authentication);
                        }
                        _ => return Err(format!("unknown error-code mapping: {name}").into()),
                    }
                }
                ["INVALID_RECORD", name, expected_error, hex] => {
                    names.push(*name);
                    let bytes = decode_hex(hex)?;
                    let error = decode_exact(&bytes).expect_err(name);
                    let intended = matches!(
                        (*expected_error, &error),
                        (
                            "INVALID_PAYLOAD_LENGTH",
                            ProtocolError::InvalidPayloadLength { .. },
                        ) | (
                            "INVALID_BOOTSTRAP_IDENTITY",
                            ProtocolError::InvalidBootstrapIdentity,
                        ) | ("INVALID_ERROR_CODE", ProtocolError::InvalidErrorCode(_))
                    );
                    assert!(intended, "{name}: unexpected error {error:?}");
                }
                ["ACK_CREDIT", name, value] => {
                    names.push(*name);
                    let value = value.parse::<u32>()?;
                    let result = encode(&Record::session(
                        RecordType::Ack,
                        instance_id,
                        7,
                        9,
                        credit_payload(value),
                    ));
                    assert_eq!(result.is_ok(), name.starts_with("VALID_"), "{name}");
                }
                ["CREDIT_INCREMENT", name, value] => {
                    names.push(*name);
                    let value = value.parse::<u32>()?;
                    let result = encode(&Record::session(
                        RecordType::Credit,
                        instance_id,
                        7,
                        9,
                        credit_payload(value),
                    ));
                    assert_eq!(result.is_ok(), name.starts_with("VALID_"), "{name}");
                }
                _ => return Err(format!("invalid language-neutral case: {line}").into()),
            }
        }
        assert_eq!(names, EXPECTED_CASE_NAMES);
        Ok(())
    }

    #[test]
    fn every_byte_fragmentation_preserves_state() -> Result<(), ProtocolError> {
        for record in [
            sample(RecordType::Ack, credit_payload(MIN_RECEIVE_CREDIT)),
            sample(RecordType::Credit, credit_payload(17)),
            sample(RecordType::Data, vec![0x33; 257]),
        ] {
            let encoded = encode(&record)?;
            for split in 0..encoded.len() {
                let mut parser = Parser::new();
                let first = parser.accept(&encoded[..split])?;
                assert!(first.record.is_none());
                let restored = Parser::restore(parser.snapshot())?;
                parser = restored;
                let second = parser.accept(&encoded[split..])?;
                assert_eq!(second.record, Some(record.clone()));
            }
        }
        Ok(())
    }

    #[test]
    fn coalesced_records_stop_at_exact_boundary() -> Result<(), ProtocolError> {
        let first = sample(RecordType::Credit, credit_payload(17));
        let second = sample(RecordType::Data, b"xNVXSy".to_vec());
        let mut bytes = encode(&first)?;
        bytes.extend_from_slice(&encode(&second)?);
        let mut parser = Parser::new();
        let progress = parser.accept(&bytes)?;
        assert_eq!(progress.record, Some(first));
        assert_eq!(progress.consumed, HEADER_LEN + 4);
        let progress = parser.accept(&bytes[progress.consumed..])?;
        assert_eq!(progress.record, Some(second));
        Ok(())
    }

    #[test]
    fn payload_boundaries() -> Result<(), ProtocolError> {
        assert!(encode(&sample(RecordType::Data, vec![1])).is_ok());
        assert!(encode(&sample(RecordType::Data, vec![1; MAX_DATA_LEN])).is_ok());
        assert!(matches!(
            encode(&sample(RecordType::Data, Vec::new())),
            Err(ProtocolError::InvalidPayloadLength { length: 0, .. })
        ));
        assert!(matches!(
            encode(&sample(RecordType::Data, vec![1; MAX_DATA_LEN + 1])),
            Err(ProtocolError::InvalidPayloadLength { length: 65_537, .. })
        ));
        Ok(())
    }

    #[test]
    fn malformed_headers_are_rejected_before_body_allocation() -> Result<(), ProtocolError> {
        let valid = encode(&sample(RecordType::Data, vec![1]))?;
        for (offset, value) in [(0, b'X'), (4, 2), (6, 99), (7, 1)] {
            let mut malformed = valid.clone();
            malformed[offset] = value;
            let mut parser = Parser::new();
            assert!(parser.accept(&malformed[..HEADER_LEN]).is_err());
            assert!(parser.body.capacity() <= MAX_DATA_LEN);
        }
        let mut oversized = valid;
        oversized[40..44].copy_from_slice(&65_537u32.to_le_bytes());
        let mut parser = Parser::new();
        assert!(matches!(
            parser.accept(&oversized[..HEADER_LEN]),
            Err(ProtocolError::InvalidPayloadLength { length: 65_537, .. })
        ));
        assert_eq!(parser.body.capacity(), 0);
        Ok(())
    }

    #[test]
    fn malformed_magic_is_not_resynchronized() -> Result<(), ProtocolError> {
        let valid = encode(&sample(RecordType::Data, b"NVXS".to_vec()))?;
        let mut malformed = vec![0; HEADER_LEN];
        malformed.extend_from_slice(&valid);
        let mut parser = Parser::new();
        assert_eq!(parser.accept(&malformed), Err(ProtocolError::InvalidMagic));
        Ok(())
    }

    #[test]
    fn payload_rules_and_error_codes_are_enforced() {
        assert!(encode(&sample(RecordType::GuestAttach, vec![1])).is_err());
        assert!(encode(&sample(RecordType::HostAttach, vec![0; 31])).is_err());
        assert!(encode(&sample(RecordType::Reset, vec![1])).is_err());
        assert!(encode(&sample(RecordType::Ack, Vec::new())).is_err());
        assert!(encode(&sample(RecordType::Ack, credit_payload(MIN_RECEIVE_CREDIT))).is_ok());
        assert!(encode(&sample(RecordType::Wait, vec![1])).is_err());
        assert!(encode(&sample(RecordType::Ready, vec![1])).is_err());
        assert_eq!(
            ErrorCode::try_from(1),
            Ok(ErrorCode::Authentication),
            "the frozen authentication code must remain stable"
        );
        for code in [0, 2, 3, 4, 5, 6, u32::MAX] {
            assert_eq!(
                ErrorCode::try_from(code),
                Err(ProtocolError::InvalidErrorCode(code))
            );
            assert!(encode(&sample(RecordType::Error, code.to_le_bytes().to_vec())).is_err());
        }
        assert!(encode(&sample(RecordType::Credit, credit_payload(1))).is_ok());
        assert!(encode(&sample(RecordType::Credit, credit_payload(0))).is_err());
    }

    fn assert_capability_redacted(label: &str, debug: &str, capability: &[u8; 32]) {
        assert!(
            !debug.contains(&format!("{capability:?}")),
            "{label} contains the complete capability: {debug}"
        );
        for window in capability.windows(8) {
            let recognizable_sequence = window
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            assert!(
                !debug.contains(&recognizable_sequence),
                "{label} contains capability bytes: {debug}"
            );
        }
        let marker = core::str::from_utf8(capability).unwrap();
        assert!(
            !debug.contains(marker),
            "{label} contains the capability marker: {debug}"
        );
    }

    #[test]
    fn debug_output_redacts_capability_storage_transitively() -> Result<(), ProtocolError> {
        let capability = *b"NVX-CAPABILITY-SECRET-0123456789";
        let record = Record::bootstrap(RecordType::HostAttach, capability.to_vec());
        let encoded = encode(&record)?;

        let mut parser = Parser::new();
        let partial = parser.accept(&encoded[..HEADER_LEN + 16])?;
        assert_eq!(partial.record, None);
        let snapshot = parser.snapshot();

        let mut complete_parser = Parser::new();
        let progress = complete_parser.accept(&encoded)?;
        assert_eq!(progress.record, Some(record.clone()));

        let debug_outputs = [
            ("record", format!("{record:?}")),
            ("partial parser", format!("{parser:?}")),
            ("parser snapshot", format!("{snapshot:?}")),
            ("parse progress", format!("{progress:?}")),
        ];
        for (label, debug) in &debug_outputs {
            assert_capability_redacted(label, debug, &capability);
        }

        assert!(debug_outputs[0].1.contains("payload_len: 32"));
        assert!(debug_outputs[1].1.contains("body_len: 16"));
        assert!(debug_outputs[2].1.contains("body_len: 16"));
        assert!(debug_outputs[3].1.contains("payload_len: 32"));
        Ok(())
    }

    #[test]
    fn invalid_parser_snapshots_are_rejected() {
        let mut snapshot = Parser::new().snapshot();
        snapshot.header_count = HEADER_LEN + 1;
        assert!(Parser::restore(snapshot).is_err());

        let mut snapshot = Parser::new().snapshot();
        snapshot.body_bytes.push(1);
        assert!(Parser::restore(snapshot).is_err());

        let mut snapshot = Parser::new().snapshot();
        snapshot.header_bytes.pop();
        assert!(Parser::restore(snapshot).is_err());
    }
}
