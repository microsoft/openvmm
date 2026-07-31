// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use smoltcp::wire::DnsFlags;
use smoltcp::wire::DnsPacket;
use smoltcp::wire::DnsQueryType;
use smoltcp::wire::DnsQuestion;
use thiserror::Error;

/// DNS record type and data for a static record.
///
/// Only [`StaticDnsRecord::A`] is currently supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticDnsRecord {
    /// IPv4 host address.
    A([u8; 4]),
}

/// An error adding a static DNS record.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StaticDnsRecordError {
    /// The query name is empty, too long, or malformed.
    #[error("the query name is empty, too long, or malformed")]
    InvalidName,
}

/// DNS `CLASS` value for the Internet (`IN`) class.
const DNS_CLASS_IN: u16 = 1;

/// TTL advertised for static records.
const DEFAULT_TTL: u32 = 60;

/// Maximum length of a single DNS label, in bytes (RFC 1035 §2.3.4).
const MAX_LABEL_LEN: usize = 63;

/// Length of the fixed DNS message header, in bytes.
const DNS_HEADER_LEN: usize = 12;

/// Fixed per-answer overhead for a compression-pointer `A` record: name
/// pointer (2) + TYPE (2) + CLASS (2) + TTL (4) + RDLENGTH (2).
const ANSWER_FIXED_LEN: usize = 12;

/// Maximum size of a DNS response over UDP.
pub(crate) const MAX_DNS_UDP_RESPONSE_LEN: usize = 512;

/// A single static DNS record.
struct StaticDnsRecordEntry {
    /// Lowercased presentation-form domain name (no trailing dot).
    name: String,
    record: StaticDnsRecord,
}

#[derive(Default)]
pub struct StaticDnsRecords {
    records: Vec<StaticDnsRecordEntry>,
}

impl StaticDnsRecords {
    /// Adds a static record.
    ///
    /// `name` is the query name in presentation form (e.g. `"example.com"`),
    /// stored lowercased and compared case-insensitively. It must be ASCII.
    ///
    /// Returns [`StaticDnsRecordError::InvalidName`] if `name` is empty, too
    /// long, non-ASCII, or otherwise malformed.
    pub fn add(
        &mut self,
        record: StaticDnsRecord,
        name: &str,
    ) -> Result<(), StaticDnsRecordError> {
        let name = normalize_name(name).ok_or(StaticDnsRecordError::InvalidName)?;
        self.records.push(StaticDnsRecordEntry { name, record });
        Ok(())
    }

    /// Builds a DNS response for `query` if it matches one of the static
    /// records, otherwise returns `None`.
    ///
    /// `max_len` bounds the size of the returned DNS message (in bytes).
    pub fn build_response(&self, query: &[u8], max_len: usize) -> Option<Vec<u8>> {
        if self.records.is_empty() {
            return None;
        }

        let packet = DnsPacket::new_checked(query).ok()?;

        // Ignore packets with the response flag set.
        if packet.flags().contains(DnsFlags::RESPONSE) {
            return None;
        }

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
            .filter_map(|rec| match &rec.record {
                StaticDnsRecord::A(address) if rec.name == qname => Some(address.as_slice()),
                _ => None,
            })
            .collect();

        if answers.is_empty() {
            return None;
        }

        let mut total = DNS_HEADER_LEN + question.buffer_len();

        if total > max_len {
            return None;
        }

        let mut fit = 0;
        for rdata in &answers {
            let answer_len = ANSWER_FIXED_LEN + rdata.len();
            if total + answer_len > max_len {
                break;
            }

            total += answer_len;
            fit += 1;
        }

        let truncated = fit < answers.len();
        Some(build_a_response(
            &packet,
            &question,
            &answers[..fit],
            truncated,
        ))
    }
}

fn normalize_name(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > smoltcp::config::DNS_MAX_NAME_SIZE {
        return None;
    }

    // Reject non-ASCII names.
    if !name.is_ascii() {
        return None;
    }

    // Reject empty labels ("..") and labels longer than the DNS maximum (63).
    if name
        .split('.')
        .any(|label| label.is_empty() || label.len() > MAX_LABEL_LEN)
    {
        return None;
    }

    Some(name.to_ascii_lowercase())
}

/// Decodes a DNS name into lowercased presentation form (no trailing dot).
///
/// Returns `None` on malformed input, or if the name exceeds
/// [`smoltcp::config::DNS_MAX_NAME_SIZE`].
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
        if qname.len() > smoltcp::config::DNS_MAX_NAME_SIZE {
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
    truncated: bool,
) -> Vec<u8> {
    // Compression pointer (top two bits set) to the echoed question name.
    const QNAME_POINTER: u16 = 0xc000 | DNS_HEADER_LEN as u16;

    let ancount = answers.len().min(u16::MAX as usize) as u16;

    // Response flags: QR=1, AA=1, RA=1, RD echoed from the query. TC is set
    // when answers were dropped to fit the response-size budget.
    let mut flags = DnsFlags::RESPONSE | DnsFlags::AUTHORITATIVE | DnsFlags::RECURSION_AVAILABLE;
    flags |= query.flags() & DnsFlags::RECURSION_DESIRED;
    if truncated {
        flags |= DnsFlags::TRUNCATED;
    }

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

/// Builds a DNS query for `name` with the given qtype, in wire format.
///
/// Uses smoltcp's [`DnsRepr`] emitter, which always encodes the `IN` class.
/// Shared by the UDP (`dns_records`) and TCP (`dns_resolver::dns_tcp`) unit
/// tests.
#[cfg(test)]
pub(crate) fn build_query(id: u16, name: &str, qtype: DnsQueryType) -> Vec<u8> {
    use smoltcp::wire::DnsOpcode;
    use smoltcp::wire::DnsRepr;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_match_a_record() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecord::A([10, 0, 0, 5]), "Example.com")
            .unwrap();

        let query = build_query(0x1234, "example.com", DnsQueryType::A);
        let response = records
            .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
            .expect("should match");

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
            .add(StaticDnsRecord::A([1, 2, 3, 4]), "host.local")
            .unwrap();
        let query = build_query(1, "HOST.LOCAL", DnsQueryType::A);
        assert!(
            records
                .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
                .is_some()
        );
    }

    #[test]
    fn multiple_records_same_name() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecord::A([1, 1, 1, 1]), "many.test")
            .unwrap();
        records
            .add(StaticDnsRecord::A([2, 2, 2, 2]), "many.test")
            .unwrap();
        let query = build_query(1, "many.test", DnsQueryType::A);
        let response = records
            .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
            .unwrap();
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 2);
    }

    #[test]
    fn non_matching_name_returns_none() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecord::A([1, 2, 3, 4]), "known.test")
            .unwrap();
        let query = build_query(1, "unknown.test", DnsQueryType::A);
        assert!(
            records
                .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );
    }

    #[test]
    fn non_a_query_returns_none() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecord::A([1, 2, 3, 4]), "known.test")
            .unwrap();
        // AAAA for the same name should not be answered.
        let query = build_query(1, "known.test", DnsQueryType::Aaaa);
        assert!(
            records
                .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );
    }

    #[test]
    fn empty_store_returns_none() {
        let records = StaticDnsRecords::default();
        let query = build_query(1, "known.test", DnsQueryType::A);
        assert!(
            records
                .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );
    }

    #[test]
    fn malformed_queries_do_not_panic() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecord::A([1, 2, 3, 4]), "known.test")
            .unwrap();

        // Too short, truncated label, unterminated name, compression pointer.
        assert!(
            records
                .build_response(&[], MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );
        assert!(
            records
                .build_response(&[0; 5], MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );

        let mut truncated = build_query(1, "known.test", DnsQueryType::A);
        truncated.truncate(15);

        assert!(
            records
                .build_response(&truncated, MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );

        // A label length that runs off the end of the buffer.
        let bad = [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 63, b'x'];
        assert!(
            records
                .build_response(&bad, MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );

        // Compression pointer in the question.
        let ptr = [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0xc0, 0x0c];
        assert!(
            records
                .build_response(&ptr, MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );
    }

    #[test]
    fn response_packets_are_ignored() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecord::A([1, 2, 3, 4]), "known.test")
            .unwrap();

        // A matching query is answered...
        let mut query = build_query(1, "known.test", DnsQueryType::A);
        assert!(
            records
                .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
                .is_some()
        );

        // ...but the same message with the QR (response) bit set is ignored,
        // so we don't reply to a DNS response misrouted to port 53.
        query[2] |= 0x80;
        assert!(
            records
                .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
                .is_none()
        );
    }

    #[test]
    fn oversized_answer_set_is_truncated() {
        let mut records = StaticDnsRecords::default();
        // Register more answers than a small budget can hold.
        for i in 0..20u8 {
            records
                .add(StaticDnsRecord::A([10, 0, 0, i]), "many.test")
                .unwrap();
        }

        let query = build_query(1, "many.test", DnsQueryType::A);

        // With a generous budget all answers fit and TC is clear.
        let full = records
            .build_response(&query, MAX_DNS_UDP_RESPONSE_LEN)
            .unwrap();
        let full_count = u16::from_be_bytes([full[6], full[7]]);
        assert_eq!(full_count, 20);
        assert_eq!(full[2] & 0x02, 0, "TC must be clear when nothing dropped");

        // Size the budget to hold exactly two answers.
        let answer_len = ANSWER_FIXED_LEN + 4;
        let base = full.len() - full_count as usize * answer_len;
        let budget = base + answer_len * 2;
        let response = records.build_response(&query, budget).unwrap();

        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 2);
        assert_ne!(response[2] & 0x02, 0, "TC must be set when answers dropped");
        assert!(response.len() <= budget);
    }

    #[test]
    fn budget_too_small_for_header_returns_none() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecord::A([1, 2, 3, 4]), "known.test")
            .unwrap();
        let query = build_query(1, "known.test", DnsQueryType::A);
        // Not even the header and question fit, so no response is synthesized.
        assert!(records.build_response(&query, 4).is_none());
    }

    #[test]
    fn add_validation() {
        let mut records = StaticDnsRecords::default();

        // Empty name.
        assert_eq!(
            records.add(StaticDnsRecord::A([1, 2, 3, 4]), ""),
            Err(StaticDnsRecordError::InvalidName)
        );
    }

    #[test]
    fn add_rejects_malformed_names() {
        let mut records = StaticDnsRecords::default();

        // Consecutive dots ("..") produce an empty label.
        assert_eq!(
            records.add(StaticDnsRecord::A([1, 2, 3, 4]), "a..b"),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A leading dot is also an empty label.
        assert_eq!(
            records.add(StaticDnsRecord::A([1, 2, 3, 4]), ".example.com"),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A name longer than the maximum permitted length is rejected.
        let too_long = "a".repeat(smoltcp::config::DNS_MAX_NAME_SIZE + 1);
        assert_eq!(
            records.add(StaticDnsRecord::A([1, 2, 3, 4]), &too_long),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A single label longer than the DNS maximum (63) is rejected, even
        // when the overall name is within the length limit.
        let long_label = "a".repeat(MAX_LABEL_LEN + 1);
        assert_eq!(
            records.add(StaticDnsRecord::A([1, 2, 3, 4]), &long_label),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A non-ASCII name is rejected.
        assert_eq!(
            records.add(StaticDnsRecord::A([1, 2, 3, 4]), "exämple.com"),
            Err(StaticDnsRecordError::InvalidName)
        );

        // A label of exactly the maximum length is accepted.
        let max_label = "a".repeat(MAX_LABEL_LEN);
        assert!(
            records
                .add(StaticDnsRecord::A([1, 2, 3, 4]), &max_label)
                .is_ok()
        );

        // A well-formed name (with an optional trailing dot) succeeds.
        assert!(
            records
                .add(StaticDnsRecord::A([1, 2, 3, 4]), "valid.example.com.")
                .is_ok()
        );
    }
}
