// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use thiserror::Error;

/// DNS record type for a static record.
///
/// Only [`StaticDnsRecordType::A`] is currently supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticDnsRecordType {
    /// IPv4 host address; RDATA is a 4-byte address.
    A,
}

/// An error adding a [`StaticDnsRecords`] entry.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StaticDnsRecordError {
    /// The query name is empty, too long, or malformed.
    #[error("the query name is empty, too long, or malformed")]
    InvalidName,
    /// The record data has the wrong length for the record type.
    #[error("record data has the wrong length for the record type")]
    InvalidData,
}

/// DNS `TYPE` value for an `A` record.
const DNS_TYPE_A: u16 = 1;
/// DNS `CLASS` value for the Internet (`IN`) class.
const DNS_CLASS_IN: u16 = 1;
/// Length in bytes of the RDATA for an `A` record.
const A_RDATA_LEN: usize = 4;
/// TTL advertised for static records.
const DEFAULT_TTL: u32 = 60;
/// Fixed size of a DNS message header.
const DNS_HEADER_SIZE: usize = 12;
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

        let question = parse_question(query)?;
        if question.qtype != DNS_TYPE_A || question.qclass != DNS_CLASS_IN {
            return None;
        }

        let answers: Vec<&[u8]> = self
            .records
            .iter()
            .filter(|rec| rec.record_type == StaticDnsRecordType::A && rec.name == question.qname)
            .map(|rec| rec.rdata.as_slice())
            .collect();

        if answers.is_empty() {
            return None;
        }

        Some(build_a_response(query, question.question_end, &answers))
    }
}

fn normalize_name(name: &str) -> Option<String> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return None;
    }
    Some(name.to_ascii_lowercase())
}

struct ParsedQuestion {
    /// Lowercased presentation-form name (no trailing dot).
    qname: String,
    qtype: u16,
    qclass: u16,
    /// Offset one past the end of the question section.
    question_end: usize,
}

/// Parses the question in a DNS query.
///
/// Returns `None` on any malformed input or if multiple questions are found.
fn parse_question(query: &[u8]) -> Option<ParsedQuestion> {
    if query.len() < DNS_HEADER_SIZE {
        return None;
    }
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if qdcount != 1 {
        // Multiple or no question found. Let the query go through.
        return None;
    }

    // Parse QNAME.
    let mut offset = DNS_HEADER_SIZE;
    let mut qname = String::new();
    loop {
        let &label_len = query.get(offset)?;
        offset += 1;
        if label_len == 0 {
            break;
        }

        if label_len & 0xc0 != 0 {
            // Compression pointer or reserved bits; not expected in a question.
            return None;
        }

        let label_len = label_len as usize;
        let label = query.get(offset..offset.checked_add(label_len)?)?;
        offset += label_len;
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

    let qtype_bytes = query.get(offset..offset.checked_add(2)?)?;
    let qtype = u16::from_be_bytes([qtype_bytes[0], qtype_bytes[1]]);
    offset += 2;
    let qclass_bytes = query.get(offset..offset.checked_add(2)?)?;
    let qclass = u16::from_be_bytes([qclass_bytes[0], qclass_bytes[1]]);
    offset += 2;

    Some(ParsedQuestion {
        qname,
        qtype,
        qclass,
        question_end: offset,
    })
}

/// Builds a DNS response message containing one `A` answer per entry in
/// `answers`, echoing the question section from `query[..question_end]`.
///
/// `question_end` is guaranteed by [`parse_question`] to be within `query`.
fn build_a_response(query: &[u8], question_end: usize, answers: &[&[u8]]) -> Vec<u8> {
    // Byte offsets of the DNS header fields we read from the query
    // (RFC 1035 §4.1.1). Each of these fields is 2 bytes wide.
    const ID_OFFSET: usize = 0;
    const FLAGS_OFFSET: usize = 2;
    const QDCOUNT_OFFSET: usize = 4;
    const U16_LEN: usize = 2;

    const FLAG_QR: u8 = 0x80; // Response.
    const FLAG_AA: u8 = 0x04; // Authoritative answer.
    const FLAG_RD: u8 = 0x01; // Recursion desired.
    const FLAG_RA: u8 = 0x80; // Recursion available.

    // 14 bits hold the byte offset of the pointed-to name. Our answers reuse
    // the question name, which begins immediately after the header.
    const NAME_POINTER_FLAG: u16 = 0xc000;
    const QNAME_POINTER: u16 = NAME_POINTER_FLAG | DNS_HEADER_SIZE as u16;

    let ancount = answers.len().min(u16::MAX as usize) as u16;
    let mut response = Vec::new();

    // Transaction ID, copied from the query.
    response.extend_from_slice(&query[ID_OFFSET..ID_OFFSET + U16_LEN]);

    // Flags: QR=1, AA=1, RD copied from query, RA=1, RCODE=0.
    let rd = query[FLAGS_OFFSET] & FLAG_RD;
    response.push(FLAG_QR | FLAG_AA | rd);
    response.push(FLAG_RA);

    // QDCOUNT (copied), ANCOUNT, NSCOUNT=0, ARCOUNT=0.
    response.extend_from_slice(&query[QDCOUNT_OFFSET..QDCOUNT_OFFSET + U16_LEN]);
    response.extend_from_slice(&ancount.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    response.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT

    // Question section, copied from the query.
    response.extend_from_slice(&query[DNS_HEADER_SIZE..question_end]);

    // One answer per record, using a compression pointer to the question name.
    for rdata in answers.iter().take(ancount as usize) {
        // NAME: compression pointer to the question name after the header.
        response.extend_from_slice(&QNAME_POINTER.to_be_bytes());
        // TYPE = A, CLASS = IN.
        response.extend_from_slice(&DNS_TYPE_A.to_be_bytes());
        response.extend_from_slice(&DNS_CLASS_IN.to_be_bytes());
        // TTL.
        response.extend_from_slice(&DEFAULT_TTL.to_be_bytes());
        // RDLENGTH + RDATA.
        response.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        response.extend_from_slice(rdata);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal DNS query for `name` with the given qtype/qclass.
    fn build_query(id: u16, name: &str, qtype: u16, qclass: u16) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&id.to_be_bytes());
        q.extend_from_slice(&[0x01, 0x00]); // flags: RD=1
        q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
        q.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN/NS/AR = 0
        for label in name.split('.').filter(|l| !l.is_empty()) {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&qclass.to_be_bytes());
        q
    }

    #[test]
    fn add_and_match_a_record() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "Example.com", &[10, 0, 0, 5])
            .unwrap();

        let query = build_query(0x1234, "example.com", DNS_TYPE_A, DNS_CLASS_IN);
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
        let query = build_query(1, "HOST.LOCAL", DNS_TYPE_A, DNS_CLASS_IN);
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
        let query = build_query(1, "many.test", DNS_TYPE_A, DNS_CLASS_IN);
        let response = records.build_response(&query).unwrap();
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 2);
    }

    #[test]
    fn non_matching_name_returns_none() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "known.test", &[1, 2, 3, 4])
            .unwrap();
        let query = build_query(1, "unknown.test", DNS_TYPE_A, DNS_CLASS_IN);
        assert!(records.build_response(&query).is_none());
    }

    #[test]
    fn non_a_query_returns_none() {
        let mut records = StaticDnsRecords::default();
        records
            .add(StaticDnsRecordType::A, "known.test", &[1, 2, 3, 4])
            .unwrap();
        // AAAA (type 28) for the same name should not be answered.
        let query = build_query(1, "known.test", 28, DNS_CLASS_IN);
        assert!(records.build_response(&query).is_none());
    }

    #[test]
    fn empty_store_returns_none() {
        let records = StaticDnsRecords::default();
        let query = build_query(1, "known.test", DNS_TYPE_A, DNS_CLASS_IN);
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
        let mut truncated = build_query(1, "known.test", DNS_TYPE_A, DNS_CLASS_IN);
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
}
