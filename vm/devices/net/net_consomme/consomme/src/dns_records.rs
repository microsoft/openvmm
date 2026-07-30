// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use smoltcp::wire::DnsFlags;
use smoltcp::wire::DnsPacket;
use smoltcp::wire::DnsQueryType;
use smoltcp::wire::DnsQuestion;
use thiserror::Error;

/// DNS record type for a static record.
///
/// Only [`StaticDnsRecordType::A`] is currently supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticDnsRecordType {
    /// IPv4 host address; RDATA is a 4-byte address.
    A,
}

/// An error adding a static DNS record.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StaticDnsRecordError {
    /// The query name is empty, too long, or malformed.
    #[error("the query name is empty, too long, or malformed")]
    InvalidName,
    /// The record data has the wrong length for the record type.
    #[error("record data has the wrong length for the record type")]
    InvalidData,
}

/// DNS `CLASS` value for the Internet (`IN`) class.
const DNS_CLASS_IN: u16 = 1;

/// Length in bytes of the RDATA for an `A` record.
const A_RDATA_LEN: usize = 4;

/// TTL advertised for static records.
const DEFAULT_TTL: u32 = 60;

/// Maximum length of a DNS name in presentation form we will store/compare.
const MAX_NAME_LEN: usize = 255;

/// A single static DNS record.
struct StaticDnsRecord {
    /// Lowercased presentation-form domain name (no trailing dot).
    name: String,
    record_type: StaticDnsRecordType,
    /// Raw RDATA.
    rdata: Vec<u8>,
}

#[derive(Default)]
pub struct StaticDnsRecords {
    records: Vec<StaticDnsRecord>,
}

impl StaticDnsRecords {
    /// Adds a static record.
    ///
    /// `name` is the query name in presentation form (e.g. `"example.com"`),
    /// stored lowercased and compared case-insensitively.
    ///
    /// Returns [`StaticDnsRecordError::InvalidName`] if `name` is empty or too
    /// long, or [`StaticDnsRecordError::InvalidData`] if `rdata` has the wrong
    /// length for `record_type`.
    pub fn add(
        &mut self,
        record_type: StaticDnsRecordType,
        name: &str,
        rdata: &[u8],
    ) -> Result<(), StaticDnsRecordError> {
        let name = normalize_name(name).ok_or(StaticDnsRecordError::InvalidName)?;
        match record_type {
            StaticDnsRecordType::A => {
                if rdata.len() != A_RDATA_LEN {
                    return Err(StaticDnsRecordError::InvalidData);
                }
            }
        }
        self.records.push(StaticDnsRecord {
            name,
            record_type,
            rdata: rdata.to_vec(),
        });
        Ok(())
    }

    /// Builds a DNS response for `query` if it matches one of the static
    /// records, otherwise returns `None`.
    pub fn build_response(&self, query: &[u8]) -> Option<Vec<u8>> {
        if self.records.is_empty() {
            return None;
        }

        let packet = DnsPacket::new_checked(query).ok()?;
        if packet.question_count() != 1 {
            // Multiple or no question; let the query go through.
            return None;
        }

        // `Question::parse` also validates that the class is `IN`.
        let (_, question) = DnsQuestion::parse(packet.payload()).ok()?;
        if question.type_ != DnsQueryType::A {
            return None;
        }

        let qname = decode_name(&packet, question.name)?;
        let answers: Vec<&[u8]> = self
            .records
            .iter()
            .filter(|rec| rec.record_type == StaticDnsRecordType::A && rec.name == qname)
            .map(|rec| rec.rdata.as_slice())
            .collect();

        if answers.is_empty() {
            return None;
        }

        Some(build_a_response(&packet, &question, &answers))
    }
}

fn normalize_name(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return None;
    }

    // Reject empty labels ("..").
    if name.split('.').any(|label| label.is_empty()) {
        return None;
    }

    Some(name.to_ascii_lowercase())
}

/// Decodes a DNS name into lowercased presentation form (no trailing dot),
///
/// Returns `None` on malformed input or if the name exceeds [`MAX_NAME_LEN`].
fn decode_name(packet: &DnsPacket<&[u8]>, name: &[u8]) -> Option<String> {
    let mut qname = String::new();
    for label in packet.parse_name(name) {
        let label = label.ok()?;
        if !qname.is_empty() {
            qname.push('.');
        }
        for &b in label {
            qname.push(b.to_ascii_lowercase() as char);
        }
        if qname.len() > MAX_NAME_LEN {
            return None;
        }
    }
    Some(qname)
}

/// Builds a DNS response message containing one `A` answer per entry in
/// `answers`, echoing the query's `question` section after the header.
fn build_a_response(
    query: &DnsPacket<&[u8]>,
    question: &DnsQuestion<'_>,
    answers: &[&[u8]],
) -> Vec<u8> {
    const DNS_HEADER_LEN: usize = 12;

    // Compression pointer (top two bits set) to the echoed question name.
    const QNAME_POINTER: u16 = 0xc000 | DNS_HEADER_LEN as u16;

    let ancount = answers.len().min(u16::MAX as usize) as u16;

    // Response flags: QR=1, AA=1, RA=1, RD echoed from the query.
    let mut flags = DnsFlags::RESPONSE | DnsFlags::AUTHORITATIVE | DnsFlags::RECURSION_AVAILABLE;
    flags |= query.flags() & DnsFlags::RECURSION_DESIRED;

    // Header + echoed question section, written via smoltcp.
    let mut response = vec![0u8; DNS_HEADER_LEN + question.buffer_len()];
    {
        let mut packet = DnsPacket::new_unchecked(&mut response[..]);
        packet.set_transaction_id(query.transaction_id());
        packet.set_flags(flags);
        packet.set_opcode(query.opcode());
        packet.set_question_count(query.question_count());
        packet.set_answer_record_count(ancount);
        packet.set_authority_record_count(0);
        packet.set_additional_record_count(0);
        question.emit(packet.payload_mut());
    }

    // One answer per record, using a compression pointer to the question name.
    for rdata in answers.iter().take(ancount as usize) {
        response.extend_from_slice(&QNAME_POINTER.to_be_bytes());
        response.extend_from_slice(&u16::from(DnsQueryType::A).to_be_bytes());
        response.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
        response.extend_from_slice(&DEFAULT_TTL.to_be_bytes());
        response.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        response.extend_from_slice(rdata);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::wire::DnsOpcode;
    use smoltcp::wire::DnsRepr;

    /// Builds a DNS query for `name` with the given qtype.
    ///
    /// Uses smoltcp's [`DnsRepr`] emitter, which always encodes the `IN` class.
    fn build_query(id: u16, name: &str, qtype: DnsQueryType) -> Vec<u8> {
        // Encode the query name into DNS wire format (length-prefixed labels).
        let mut name_wire = Vec::new();
        for label in name.split('.').filter(|l| !l.is_empty()) {
            name_wire.push(label.len() as u8);
            name_wire.extend_from_slice(label.as_bytes());
        }

        name_wire.push(0);

        let repr = DnsRepr {
            transaction_id: id,
            opcode: DnsOpcode::Query,
            flags: DnsFlags::RECURSION_DESIRED,
            question: DnsQuestion {
                name: &name_wire,
                type_: qtype,
            },
        };
        let mut buffer = vec![0u8; repr.buffer_len()];
        repr.emit(&mut DnsPacket::new_unchecked(&mut buffer[..]));
        buffer
    }

    #[test]
    fn add_and_match_a_record() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "Example.com", &[10, 0, 0, 5])
            .unwrap();

        let query = build_query(0x1234, "example.com", DnsQueryType::A);
        let response = records.build_response(&query).expect("should match");

        // Transaction ID preserved.
        assert_eq!(&response[0..2], &[0x12, 0x34]);

        // QR + AA set, RD preserved, RA set, RCODE 0.
        assert_eq!(response[2], 0x85);
        assert_eq!(response[3], 0x80);

        // ANCOUNT == 1.
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 1);

        // Final 4 RDATA bytes are the address we registered.
        assert_eq!(&response[response.len() - 4..], &[10, 0, 0, 5]);

        // RDATA is preceded by RDLENGTH == 4.
        assert_eq!(
            u16::from_be_bytes([response[response.len() - 6], response[response.len() - 5]]),
            4
        );
    }

    #[test]
    fn case_insensitive_match() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "host.local", &[1, 2, 3, 4])
            .unwrap();
        let query = build_query(1, "HOST.LOCAL", DnsQueryType::A);
        assert!(records.build_response(&query).is_some());
    }

    #[test]
    fn multiple_records_same_name() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "many.test", &[1, 1, 1, 1])
            .unwrap();
        records
            .add(StaticDnsRecordType::A, "many.test", &[2, 2, 2, 2])
            .unwrap();
        let query = build_query(1, "many.test", DnsQueryType::A);
        let response = records.build_response(&query).unwrap();
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 2);
    }

    #[test]
    fn non_matching_name_returns_none() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "known.test", &[1, 2, 3, 4])
            .unwrap();
        let query = build_query(1, "unknown.test", DnsQueryType::A);
        assert!(records.build_response(&query).is_none());
    }

    #[test]
    fn non_a_query_returns_none() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "known.test", &[1, 2, 3, 4])
            .unwrap();
        // AAAA for the same name should not be answered.
        let query = build_query(1, "known.test", DnsQueryType::Aaaa);
        assert!(records.build_response(&query).is_none());
    }

    #[test]
    fn empty_store_returns_none() {
        let records = StaticDnsRecords::default();
        let query = build_query(1, "known.test", DnsQueryType::A);
        assert!(records.build_response(&query).is_none());
    }

    #[test]
    fn malformed_queries_do_not_panic() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "known.test", &[1, 2, 3, 4])
            .unwrap();

        // Too short, truncated label, unterminated name, compression pointer.
        assert!(records.build_response(&[]).is_none());
        assert!(records.build_response(&[0; 5]).is_none());

        let mut truncated = build_query(1, "known.test", DnsQueryType::A);
        truncated.truncate(15);

        assert!(records.build_response(&truncated).is_none());

        // A label length that runs off the end of the buffer.
        let bad = [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 63, b'x'];
        assert!(records.build_response(&bad).is_none());

        // Compression pointer in the question.
        let ptr = [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0xc0, 0x0c];
        assert!(records.build_response(&ptr).is_none());
    }

    #[test]
    fn add_validation() {
        let mut records = StaticDnsRecords::default();

        // Wrong RDATA length for an A record.
        assert_eq!(
            records.add(StaticDnsRecordType::A, "a.test", &[1, 2, 3]),
            Err(StaticDnsRecordError::InvalidData)
        );

        // Empty name.
        assert_eq!(
            records.add(StaticDnsRecordType::A, "", &[1, 2, 3, 4]),
            Err(StaticDnsRecordError::InvalidName)
        );
    }

    #[test]
    fn add_rejects_malformed_names() {
        let mut records = StaticDnsRecords::default();

        // Consecutive dots ("..") produce an empty label.
        assert_eq!(
            records.add(StaticDnsRecordType::A, "a..b", &[1, 2, 3, 4]),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A leading dot is also an empty label.
        assert_eq!(
            records.add(StaticDnsRecordType::A, ".example.com", &[1, 2, 3, 4]),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A name longer than the maximum permitted length is rejected.
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        assert_eq!(
            records.add(StaticDnsRecordType::A, &too_long, &[1, 2, 3, 4]),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A well-formed name (with an optional trailing dot) succeeds.
        assert!(
            records
                .add(StaticDnsRecordType::A, "valid.example.com.", &[1, 2, 3, 4])
                .is_ok()
        );
    }
}
