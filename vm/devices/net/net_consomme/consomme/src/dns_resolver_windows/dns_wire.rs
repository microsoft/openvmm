// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DNS wire format parsing and building utilities.
//!
//! This module provides implementations for parsing DNS queries
//! and building DNS responses in wire format (RFC 1035).
//!
//! Uses types from smoltcp where possible for consistency with the rest
//! of the networking stack.

// UNSAFETY: Accessing Windows DNS_RECORDA linked list structures.
#![expect(unsafe_code)]

use smoltcp::wire::DnsOpcode;
use smoltcp::wire::DnsPacket;
use smoltcp::wire::DnsQueryType;
use smoltcp::wire::DnsQuestion;
use smoltcp::wire::DnsRcode;
use thiserror::Error;
use windows_sys::Win32::NetworkManagement::Dns::DNS_RECORDA;
use windows_sys::Win32::NetworkManagement::Dns::DNS_RECORDW;

/// Maximum length for a single DNS label (RFC 1035 Section 2.3.4).
pub const MAX_DNS_LABEL_LENGTH: usize = 63;

/// Maximum total length for a DNS name (RFC 1035 Section 2.3.4).
/// This is 253 characters for the text representation (255 bytes in wire format
/// including length bytes and null terminator).
pub const MAX_DNS_NAME_LENGTH: usize = 253;

/// Maximum reasonable size for a DNS response to prevent excessive memory allocation.
/// This is set to the maximum UDP DNS message size (65535 bytes).
pub const MAX_DNS_RESPONSE_SIZE: usize = 65535;

/// Errors that can occur during DNS wire format operations.
#[derive(Debug, Error)]
pub enum DnsWireError {
    /// The DNS query data is too short to contain a valid header.
    #[error("DNS query too short: need at least 12 bytes, got {0}")]
    QueryTooShort(usize),

    /// The DNS query has an invalid or unsupported opcode.
    #[error("invalid DNS opcode: {0:?}")]
    InvalidOpcode(DnsOpcode),

    /// The DNS query has an unexpected number of questions.
    #[error("invalid question count: expected 1, got {0}")]
    InvalidQuestionCount(u16),

    /// Failed to parse the DNS question section.
    #[error("failed to parse DNS question section")]
    QuestionParseError,

    /// Failed to parse the DNS name from the query.
    #[error("failed to parse DNS name")]
    NameParseError,

    /// A DNS label exceeds the maximum length of 63 bytes.
    #[error("DNS label exceeds maximum length of {MAX_DNS_LABEL_LENGTH} bytes: got {0}")]
    LabelTooLong(usize),

    /// The total DNS name exceeds the maximum length of 253 bytes.
    #[error("DNS name exceeds maximum length of {MAX_DNS_NAME_LENGTH} bytes: got {0}")]
    NameTooLong(usize),
}

/// DNS record types supported by this implementation.
///
/// This enum provides a type-safe representation of DNS record types,
/// including types not directly supported by smoltcp's `DnsQueryType`.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum DnsRecordType {
    A,
    Ns,
    Cname,
    Soa,
    Ptr,
    Mx,
    Txt,
    Aaaa,
    Srv,
    Svcb,
    Https,
    Unsupported(u16),
}

impl DnsRecordType {
    /// Convert the record type to its raw u16 value.
    pub fn as_u16(&self) -> u16 {
        match self {
            Self::A => 1,
            Self::Ns => 2,
            Self::Cname => 5,
            Self::Soa => 6,
            Self::Ptr => 12,
            Self::Mx => 15,
            Self::Txt => 16,
            Self::Aaaa => 28,
            Self::Srv => 33,
            Self::Svcb => 64,
            Self::Https => 65,
            Self::Unsupported(x) => *x,
        }
    }
}

impl From<u16> for DnsRecordType {
    fn from(value: u16) -> Self {
        match value {
            1 => Self::A,
            2 => Self::Ns,
            5 => Self::Cname,
            6 => Self::Soa,
            12 => Self::Ptr,
            15 => Self::Mx,
            16 => Self::Txt,
            28 => Self::Aaaa,
            33 => Self::Srv,
            64 => Self::Svcb,
            65 => Self::Https,
            x => Self::Unsupported(x),
        }
    }
}

impl From<DnsQueryType> for DnsRecordType {
    fn from(qtype: DnsQueryType) -> Self {
        match qtype {
            DnsQueryType::A => Self::A,
            DnsQueryType::Ns => Self::Ns,
            DnsQueryType::Cname => Self::Cname,
            DnsQueryType::Soa => Self::Soa,
            DnsQueryType::Aaaa => Self::Aaaa,
            DnsQueryType::Unknown(x) => Self::from(x),
        }
    }
}

/// Parsed DNS query information extracted from wire format.
#[derive(Debug, Clone)]
pub struct ParsedDnsQuery {
    /// Transaction ID from the query.
    pub transaction_id: u16,
    /// Original flags from the query.
    pub flags: u16,
    /// Query name (e.g., "www.example.com").
    pub name: String,
    /// Query type (e.g., A, AAAA, CNAME).
    pub qtype: DnsQueryType,
    /// The raw question section bytes (for rebuilding responses).
    pub question_section: Vec<u8>,
}

/// Parse a DNS query from wire format.
///
/// Returns the parsed query or `None` if parsing fails.
/// For more detailed error information, use [`try_parse_dns_query`].
/// TODO: remove this and use try_parse_dns_query directly
pub fn parse_dns_query(data: &[u8]) -> Option<ParsedDnsQuery> {
    try_parse_dns_query(data).ok()
}

/// Parse a DNS query from wire format with detailed error reporting.
///
/// Returns the parsed query or a specific error indicating what went wrong.
pub fn try_parse_dns_query(data: &[u8]) -> Result<ParsedDnsQuery, DnsWireError> {
    // Check minimum size for DNS header
    if data.len() < 12 {
        return Err(DnsWireError::QueryTooShort(data.len()));
    }

    // Use smoltcp's DnsPacket for initial validation and header parsing
    let packet =
        DnsPacket::new_checked(data).map_err(|_| DnsWireError::QueryTooShort(data.len()))?;

    if packet.opcode() != DnsOpcode::Query {
        tracing::warn!(opcode = ?packet.opcode(), "DNS query with unexpected opcode");
        return Err(DnsWireError::InvalidOpcode(packet.opcode()));
    }

    let transaction_id = packet.transaction_id();
    // Get raw flags from the packet bytes (bytes 2-3)
    let flags = u16::from_be_bytes([data[2], data[3]]);
    let question_count = packet.question_count();

    // We only handle queries with exactly one question
    if question_count != 1 {
        tracing::warn!(question_count, "DNS query with unexpected question count");
        return Err(DnsWireError::InvalidQuestionCount(question_count));
    }

    // Use smoltcp's DnsQuestion::parse to parse the question section
    let payload = &data[12..]; // After DNS header
    let (rest, question) =
        DnsQuestion::parse(payload).map_err(|_| DnsWireError::QuestionParseError)?;

    // Calculate where the question section ends
    let question_section_len = payload.len() - rest.len();
    let question_section = payload[..question_section_len].to_vec();

    // Convert the raw name to a String using DnsPacket::parse_name
    let labels: Vec<&[u8]> = packet
        .parse_name(question.name)
        .map(|r| r.map_err(|_| DnsWireError::NameParseError))
        .collect::<Result<Vec<_>, _>>()?;

    // Validate label lengths per RFC 1035
    for label in &labels {
        if label.len() > MAX_DNS_LABEL_LENGTH {
            return Err(DnsWireError::LabelTooLong(label.len()));
        }
    }

    let name = labels
        .into_iter()
        .filter_map(|label| std::str::from_utf8(label).ok())
        .collect::<Vec<_>>()
        .join(".");

    // Validate total name length per RFC 1035
    if name.len() > MAX_DNS_NAME_LENGTH {
        return Err(DnsWireError::NameTooLong(name.len()));
    }

    let qtype = question.type_;

    // Note: qclass is validated by DnsQuestion::parse (must be CLASS_IN = 1)

    Ok(ParsedDnsQuery {
        transaction_id,
        flags,
        name,
        qtype,
        question_section,
    })
}

/// Encode a DNS name to wire format (label encoding).
///
/// Converts "www.example.com" to the wire format with length-prefixed labels.
/// This function does not validate RFC 1035 length limits; use
/// [`try_encode_dns_name`] for validation.
pub fn encode_dns_name(name: &str) -> Vec<u8> {
    try_encode_dns_name(name).unwrap_or_else(|_| {
        // Fallback: encode anyway for backward compatibility, but this
        // may produce invalid DNS wire format
        let mut result = Vec::new();
        for label in name.split('.') {
            if label.is_empty() {
                continue;
            }
            // Truncate label if too long (should not happen with valid input)
            let len = label.len().min(MAX_DNS_LABEL_LENGTH);
            result.push(len as u8);
            result.extend_from_slice(&label.as_bytes()[..len]);
        }
        result.push(0);
        result
    })
}

/// Encode a DNS name to wire format with RFC 1035 validation.
///
/// Validates that:
/// - Each label is at most 63 bytes
/// - The total name is at most 253 bytes
///
/// Returns an error if validation fails.
pub fn try_encode_dns_name(name: &str) -> Result<Vec<u8>, DnsWireError> {
    // Validate total name length
    if name.len() > MAX_DNS_NAME_LENGTH {
        return Err(DnsWireError::NameTooLong(name.len()));
    }

    let mut result = Vec::new();

    for label in name.split('.') {
        if label.is_empty() {
            continue;
        }

        // Validate label length
        if label.len() > MAX_DNS_LABEL_LENGTH {
            return Err(DnsWireError::LabelTooLong(label.len()));
        }

        result.push(label.len() as u8);
        result.extend_from_slice(label.as_bytes());
    }
    result.push(0); // Null terminator

    Ok(result)
}

/// Build a DNS response from Windows DNS record structures.
///
/// Converts the Windows DNS record linked list back to wire format.
/// Note: DnsQueryEx with wide string input returns DNS_RECORDW, but the
/// pQueryRecords field is typed as *mut DNS_RECORDA in some bindings.
/// We cast to DNS_RECORDW since the records contain wide strings.
///
/// If the response exceeds `MAX_DNS_RESPONSE_SIZE`, it will be truncated
/// and the TC (truncation) flag will be set.
///
/// # Safety
///
/// The `records` pointer must be a valid linked list from Windows DNS APIs,
/// or null.
pub fn build_dns_response(
    query: &ParsedDnsQuery,
    records: *const DNS_RECORDA,
    rcode: DnsRcode,
) -> Vec<u8> {
    // Cast to DNS_RECORDW since DnsQueryEx returns wide string records
    let records_w = records as *const DNS_RECORDW;

    let mut response = Vec::with_capacity(512);

    // Header (12 bytes)
    response.extend_from_slice(&query.transaction_id.to_be_bytes());

    // Flags: QR=1 (response), preserve RD from query, set RA=1, RCODE from parameter
    let rd = (query.flags >> 8) & 0x01;
    let rcode_val: u8 = rcode.into();
    let response_flags: u16 = 0x8000 | (rd << 8) | 0x0080 | (rcode_val as u16);
    response.extend_from_slice(&response_flags.to_be_bytes());

    // QDCOUNT = 1
    response.extend_from_slice(&1u16.to_be_bytes());

    // Count answer records (section 1 = Answer)
    // SAFETY: records_w is either null or a valid linked list from Windows DNS APIs
    let answer_count = unsafe { count_answer_records_w(records_w) };
    response.extend_from_slice(&answer_count.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes()); // NSCOUNT
    response.extend_from_slice(&0u16.to_be_bytes()); // ARCOUNT

    // Question section (copy from query)
    response.extend_from_slice(&query.question_section);

    // Answer section
    // SAFETY: records_w is either null or a valid linked list from Windows DNS APIs
    unsafe { encode_answer_records_w(records_w, &mut response, MAX_DNS_RESPONSE_SIZE) };

    // If response exceeded max size, set truncation flag
    if response.len() > MAX_DNS_RESPONSE_SIZE {
        response.truncate(MAX_DNS_RESPONSE_SIZE);
        // Set TC bit (bit 1 of byte 2)
        if response.len() >= 3 {
            response[2] |= 0x02;
        }
        tracing::warn!(
            original_size = response.len(),
            max_size = MAX_DNS_RESPONSE_SIZE,
            "DNS response truncated due to size limit"
        );
    }

    response
}

/// Count answer records in the DNS_RECORDW linked list.
///
/// # Safety
///
/// The `records` pointer must be a valid linked list from Windows DNS APIs,
/// or null.
unsafe fn count_answer_records_w(records: *const DNS_RECORDW) -> u16 {
    let mut count = 0u16;
    let mut current = records;

    while !current.is_null() {
        let record = unsafe { &*current };
        // Section field is bits 0-1 of Flags.DW; section 1 = Answer
        let section = unsafe { record.Flags.DW & 0x3 };
        if section == 1 {
            count += 1;
        }
        current = record.pNext;
    }

    count
}

/// Encode answer records from DNS_RECORDW linked list to wire format.
///
/// # Arguments
///
/// * `records` - Pointer to the first record in the linked list
/// * `response` - Buffer to append encoded records to
/// * `max_size` - Maximum total response size; stops encoding if exceeded
///
/// # Safety
///
/// The `records` pointer must be a valid linked list from Windows DNS APIs,
/// or null.
unsafe fn encode_answer_records_w(
    records: *const DNS_RECORDW,
    response: &mut Vec<u8>,
    max_size: usize,
) {
    let mut current = records;

    while !current.is_null() {
        // Check if we're approaching the size limit
        if response.len() >= max_size {
            tracing::debug!(
                current_size = response.len(),
                max_size,
                "Stopping record encoding due to size limit"
            );
            break;
        }

        let record = unsafe { &*current };

        // Only include answer section records (section 1)
        let section = unsafe { record.Flags.DW & 0x3 };
        if section == 1 {
            // SAFETY: record is a valid reference from the linked list
            if let Some(rr_data) = unsafe { encode_dns_record_w(record) } {
                // Check if adding this record would exceed the limit
                if response.len() + rr_data.len() > max_size {
                    tracing::debug!(
                        current_size = response.len(),
                        record_size = rr_data.len(),
                        max_size,
                        "Skipping record to avoid exceeding size limit"
                    );
                    break;
                }
                response.extend_from_slice(&rr_data);
            }
        }

        current = record.pNext;
    }
}

/// Build a minimal DNS error response (SERVFAIL, NXDOMAIN, etc.).
pub fn build_dns_error_response(query: &ParsedDnsQuery, rcode: DnsRcode) -> Vec<u8> {
    let mut response = Vec::with_capacity(12 + query.question_section.len());

    // Header
    response.extend_from_slice(&query.transaction_id.to_be_bytes());

    // Flags: QR=1 (response), preserve RD from query, set RA=1, RCODE
    let rd = (query.flags >> 8) & 0x01;
    let rcode_val: u8 = rcode.into();
    let response_flags: u16 = 0x8000 | (rd << 8) | 0x0080 | (rcode_val as u16);
    response.extend_from_slice(&response_flags.to_be_bytes());

    // QDCOUNT = 1, ANCOUNT = 0, NSCOUNT = 0, ARCOUNT = 0
    response.extend_from_slice(&1u16.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());
    response.extend_from_slice(&0u16.to_be_bytes());

    // Question section
    response.extend_from_slice(&query.question_section);

    response
}

/// Encode a single DNS_RECORDW (wide string version) to wire format.
///
/// # Safety
///
/// The `record` must be a valid DNS_RECORDW structure from Windows DNS APIs,
/// with valid pointers in its fields (e.g., pName).
unsafe fn encode_dns_record_w(record: &DNS_RECORDW) -> Option<Vec<u8>> {
    let mut rr = Vec::new();

    // Name - get from pName field (wide string)
    if record.pName.is_null() {
        return None;
    }

    // SAFETY: pName is a valid null-terminated wide string from Windows API
    let name = unsafe { wstr_to_string(record.pName)? };
    rr.extend_from_slice(&encode_dns_name(&name));

    // TYPE (2 bytes)
    rr.extend_from_slice(&record.wType.to_be_bytes());

    // CLASS (2 bytes) - IN = 1
    rr.extend_from_slice(&1u16.to_be_bytes());

    // TTL (4 bytes)
    rr.extend_from_slice(&record.dwTtl.to_be_bytes());

    // RDATA - depends on record type
    // SAFETY: record is a valid DNS_RECORDW structure
    let rdata = unsafe { encode_rdata_w(record)? };
    rr.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    rr.extend_from_slice(&rdata);

    Some(rr)
}

/// Encode the RDATA portion of a DNS_RECORDW based on its type.
///
/// # Safety
///
/// The `record` must be a valid DNS_RECORDW structure from Windows DNS APIs,
/// with the correct union variant populated based on `wType`.
unsafe fn encode_rdata_w(record: &DNS_RECORDW) -> Option<Vec<u8>> {
    let record_type = DnsRecordType::from(record.wType);

    // Accessing union fields based on record type - safety guaranteed by caller
    unsafe {
        match record_type {
            DnsRecordType::A => {
                // A record: 4-byte IPv4 address
                // Windows stores IpAddress as a u32 in network byte order.
                // Use to_ne_bytes() to get the bytes in their stored order.
                let ip_bytes = record.Data.A.IpAddress.to_ne_bytes();
                Some(ip_bytes.to_vec())
            }
            DnsRecordType::Aaaa => {
                // AAAA record: 16-byte IPv6 address
                Some(record.Data.AAAA.Ip6Address.IP6Byte.to_vec())
            }
            DnsRecordType::Cname | DnsRecordType::Ns => {
                // These have a single name in the data (wide string)
                let name_ptr = record.Data.CNAME.pNameHost;
                if name_ptr.is_null() {
                    return None;
                }
                let name = wstr_to_string(name_ptr)?;
                Some(encode_dns_name(&name))
            }
            DnsRecordType::Soa => {
                let mut rdata = Vec::new();
                let soa = &record.Data.SOA;

                let mname = wstr_to_string(soa.pNamePrimaryServer)?;
                rdata.extend_from_slice(&encode_dns_name(&mname));

                let rname = wstr_to_string(soa.pNameAdministrator)?;
                rdata.extend_from_slice(&encode_dns_name(&rname));

                rdata.extend_from_slice(&soa.dwSerialNo.to_be_bytes());
                rdata.extend_from_slice(&soa.dwRefresh.to_be_bytes());
                rdata.extend_from_slice(&soa.dwRetry.to_be_bytes());
                rdata.extend_from_slice(&soa.dwExpire.to_be_bytes());
                rdata.extend_from_slice(&soa.dwDefaultTtl.to_be_bytes());

                Some(rdata)
            }
            DnsRecordType::Ptr => {
                let name_ptr = record.Data.PTR.pNameHost;
                if name_ptr.is_null() {
                    return None;
                }
                let name = wstr_to_string(name_ptr)?;
                Some(encode_dns_name(&name))
            }
            DnsRecordType::Mx => {
                let mut rdata = Vec::new();
                rdata.extend_from_slice(&record.Data.MX.wPreference.to_be_bytes());
                let name_ptr = record.Data.MX.pNameExchange;
                if name_ptr.is_null() {
                    return None;
                }
                let name = wstr_to_string(name_ptr)?;
                rdata.extend_from_slice(&encode_dns_name(&name));
                Some(rdata)
            }
            DnsRecordType::Txt => {
                let txt = &record.Data.TXT;
                let mut rdata = Vec::new();
                for i in 0..txt.dwStringCount as usize {
                    let str_ptr_ptr = txt.pStringArray.as_ptr().add(i);
                    let str_ptr = *str_ptr_ptr;
                    if str_ptr.is_null() {
                        continue;
                    }
                    let s = wstr_to_string(str_ptr)?;
                    let bytes = s.as_bytes();
                    if bytes.len() > 255 {
                        for chunk in bytes.chunks(255) {
                            rdata.push(chunk.len() as u8);
                            rdata.extend_from_slice(chunk);
                        }
                    } else {
                        rdata.push(bytes.len() as u8);
                        rdata.extend_from_slice(bytes);
                    }
                }
                Some(rdata)
            }
            DnsRecordType::Srv => {
                let mut rdata = Vec::new();
                rdata.extend_from_slice(&record.Data.SRV.wPriority.to_be_bytes());
                rdata.extend_from_slice(&record.Data.SRV.wWeight.to_be_bytes());
                rdata.extend_from_slice(&record.Data.SRV.wPort.to_be_bytes());
                let name_ptr = record.Data.SRV.pNameTarget;
                if name_ptr.is_null() {
                    return None;
                }
                let name = wstr_to_string(name_ptr)?;
                rdata.extend_from_slice(&encode_dns_name(&name));
                Some(rdata)
            }
            DnsRecordType::Svcb | DnsRecordType::Https => encode_svcb_rdata_w(record),
            DnsRecordType::Unsupported(type_code) => {
                tracing::debug!(
                    record_type = type_code,
                    "Skipping unsupported DNS record type"
                );
                None
            }
        }
    }
}

/// Encode SVCB/HTTPS record RDATA (wide string version).
unsafe fn encode_svcb_rdata_w(record: &DNS_RECORDW) -> Option<Vec<u8>> {
    use windows_sys::Win32::NetworkManagement::Dns::DNS_SVCB_PARAM;

    let svcb = unsafe { &record.Data.SVCB };
    let mut rdata = Vec::new();

    rdata.extend_from_slice(&svcb.wSvcPriority.to_be_bytes());

    if svcb.pszTargetName.is_null() {
        rdata.push(0);
    } else {
        // Note: pszTargetName in SVCB is PSTR (narrow) even in DNS_RECORDW
        let target_name = unsafe { cstr_to_string(svcb.pszTargetName as *const i8) };
        match target_name {
            Some(name) if !name.is_empty() => {
                rdata.extend_from_slice(&encode_dns_name(&name));
            }
            _ => {
                rdata.push(0);
            }
        }
    }

    if svcb.cSvcParams > 0 && !svcb.pSvcParams.is_null() {
        for i in 0..svcb.cSvcParams as usize {
            let param: &DNS_SVCB_PARAM = unsafe { &*svcb.pSvcParams.add(i) };
            if let Some(param_data) = unsafe { encode_svcb_param(param) } {
                rdata.extend_from_slice(&param_data);
            }
        }
    }

    Some(rdata)
}

/// Encode a single SVCB parameter to wire format.
///
/// # Safety
///
/// The param must be a valid DNS_SVCB_PARAM structure.
unsafe fn encode_svcb_param(
    param: &windows_sys::Win32::NetworkManagement::Dns::DNS_SVCB_PARAM,
) -> Option<Vec<u8>> {
    use windows_sys::Win32::NetworkManagement::Dns::{
        DnsSvcbParamAlpn, DnsSvcbParamIpv4Hint, DnsSvcbParamIpv6Hint, DnsSvcbParamMandatory,
        DnsSvcbParamPort,
    };

    let key = param.wSvcParamKey;
    let mut result = Vec::new();

    // SvcParamKey (2 bytes)
    result.extend_from_slice(&key.to_be_bytes());

    // Encode value based on parameter type
    let value_bytes: Vec<u8> = unsafe {
        match key as i32 {
            k if k == DnsSvcbParamMandatory => {
                // Mandatory keys: list of u16 keys
                let mandatory = param.Anonymous.pMandatory;
                if mandatory.is_null() {
                    Vec::new()
                } else {
                    let m = &*mandatory;
                    let mut bytes = Vec::new();
                    for j in 0..m.cMandatoryKeys as usize {
                        let key_ptr = m.rgwMandatoryKeys.as_ptr().add(j);
                        bytes.extend_from_slice(&(*key_ptr).to_be_bytes());
                    }
                    bytes
                }
            }
            k if k == DnsSvcbParamAlpn => {
                // ALPN: list of length-prefixed protocol IDs
                let alpn = param.Anonymous.pAlpn;
                if alpn.is_null() {
                    Vec::new()
                } else {
                    let a = &*alpn;
                    let mut bytes = Vec::new();
                    for j in 0..a.cIds as usize {
                        let id_ptr = a.rgIds.as_ptr().add(j);
                        let id = &*id_ptr;
                        if !id.pbId.is_null() && id.cBytes > 0 {
                            bytes.push(id.cBytes);
                            let id_slice = std::slice::from_raw_parts(id.pbId, id.cBytes as usize);
                            bytes.extend_from_slice(id_slice);
                        }
                    }
                    bytes
                }
            }
            k if k == DnsSvcbParamPort => {
                // Port: u16
                param.Anonymous.wPort.to_be_bytes().to_vec()
            }
            k if k == DnsSvcbParamIpv4Hint => {
                // IPv4 hints: list of 4-byte addresses
                // Windows stores IP addresses in network byte order (big-endian),
                // so we use to_ne_bytes() to preserve the byte order as-is.
                let ipv4 = param.Anonymous.pIpv4Hints;
                if ipv4.is_null() {
                    Vec::new()
                } else {
                    let hints = &*ipv4;
                    let mut bytes = Vec::new();
                    for j in 0..hints.cIps as usize {
                        let ip_ptr = hints.rgIps.as_ptr().add(j);
                        bytes.extend_from_slice(&(*ip_ptr).to_ne_bytes());
                    }
                    bytes
                }
            }
            k if k == DnsSvcbParamIpv6Hint => {
                // IPv6 hints: list of 16-byte addresses
                let ipv6 = param.Anonymous.pIpv6Hints;
                if ipv6.is_null() {
                    Vec::new()
                } else {
                    let hints = &*ipv6;
                    let mut bytes = Vec::new();
                    for j in 0..hints.cIps as usize {
                        let ip_ptr = hints.rgIps.as_ptr().add(j);
                        bytes.extend_from_slice(&(*ip_ptr).IP6Byte);
                    }
                    bytes
                }
            }
            _ => {
                // Unknown parameter type - use the raw unknown data
                let unknown = param.Anonymous.pUnknown;
                if unknown.is_null() {
                    Vec::new()
                } else {
                    let u = &*unknown;
                    if u.cBytes > 0 {
                        std::slice::from_raw_parts(u.pbSvcParamValue.as_ptr(), u.cBytes as usize)
                            .to_vec()
                    } else {
                        Vec::new()
                    }
                }
            }
        }
    };

    // SvcParamValue length (2 bytes)
    result.extend_from_slice(&(value_bytes.len() as u16).to_be_bytes());

    // SvcParamValue
    result.extend_from_slice(&value_bytes);

    Some(result)
}

/// Convert an ANSI C string pointer to a Rust String.
///
/// # Safety
///
/// The `ptr` must be a valid null-terminated ANSI string or null.
pub unsafe fn cstr_to_string(ptr: *const i8) -> Option<String> {
    if ptr.is_null() {
        return None;
    }

    // SAFETY: Caller guarantees ptr is valid null-terminated string
    unsafe {
        let mut len = 0;
        let mut p = ptr;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
        let slice = std::slice::from_raw_parts(ptr as *const u8, len);
        String::from_utf8(slice.to_vec()).ok()
    }
}

/// Convert a wide (UTF-16) C string pointer to a Rust String.
///
/// # Safety
///
/// The `ptr` must be a valid null-terminated wide string or null.
pub unsafe fn wstr_to_string(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }

    // SAFETY: Caller guarantees ptr is valid null-terminated wide string
    unsafe {
        let mut len = 0;
        let mut p = ptr;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        String::from_utf16(slice).ok()
    }
}

/// Convert Windows DNS error code to DNS RCODE.
pub fn dns_error_to_rcode(error: u32) -> DnsRcode {
    // Windows DNS error codes (from winerror.h)
    const DNS_ERROR_RCODE_NAME_ERROR: u32 = 9003;
    const DNS_ERROR_RCODE_REFUSED: u32 = 9005;
    const DNS_INFO_NO_RECORDS: u32 = 9501;

    match error {
        DNS_ERROR_RCODE_NAME_ERROR => DnsRcode::NXDomain,
        DNS_ERROR_RCODE_REFUSED => DnsRcode::Refused,
        DNS_INFO_NO_RECORDS => DnsRcode::NoError, // NOERROR with no answers
        _ => DnsRcode::ServFail,
    }
}
