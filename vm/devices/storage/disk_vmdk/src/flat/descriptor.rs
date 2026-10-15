// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parses flat VMDK text descriptors.
//!
//! The parser extracts the ordered `FLAT` extents needed to construct the
//! virtual disk. Disk database fields are ignored because they do not affect
//! the virtual layout exposed by this backend.

use crate::ParseError;

// Required first line identifying a VMDK text descriptor.
const DESCRIPTOR_HEADER: &str = "# Disk DescriptorFile";
// Section marker separating descriptor fields from extent records.
const EXTENT_MARKER: &str = "# Extent description";
// Accepted DDB section markers; either one terminates the extent list.
const DDB_MARKERS: [&str; 2] = ["# The Disk Data Base", "#DDB"];
// Prefix used by key/value entries in the DDB section.
const DDB_ENTRY_PREFIX: &str = "ddb.";

/// Access mode declared for an extent.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ExtentAccess {
    /// The descriptor declares the extent readable and writable.
    ReadWrite,
    /// The descriptor declares the extent read-only.
    ReadOnly,
}

/// One validated `FLAT` extent from the descriptor.
#[derive(Debug)]
pub(crate) struct ParsedExtent {
    /// Access mode declared by the descriptor.
    pub access: ExtentAccess,
    /// Host path as written in the descriptor.
    pub path: String,
    /// Length of the extent in 512-byte sectors.
    pub sector_count: u64,
    /// First 512-byte sector read from the source file.
    pub source_start_sector: u64,
}

/// Supported flat VMDK disk types.
enum CreateType {
    /// Preallocated data stored in a single extent file.
    MonolithicFlat,
    /// Preallocated data split across extent files no larger than 2 GB each.
    TwoGbMaxExtentFlat,
}

/// Parses and validates the supported subset of a VMDK text descriptor.
pub(crate) fn parse_descriptor(bytes: &[u8]) -> Result<Vec<ParsedExtent>, ParseError> {
    // The descriptor is expected to be valid UTF-8.
    let text = std::str::from_utf8(bytes)?;
    // UTF-8 permits NUL characters, but VMDK text fields and host paths do not.
    if let Some(line) = text.lines().find(|line| line.contains('\0')) {
        return Err(ParseError::Invalid(format!(
            "NUL bytes are not allowed in {line:?}"
        )));
    }

    let mut lines = text.lines();

    // Parse the header and advance the iterator to the extent section.
    let create_type = parse_header(&mut lines)?;

    // Parse the extents from the extent section in the descriptor.
    let mut extents = Vec::new();
    for line in lines {
        let line = line.trim();

        // Ignore blank lines between extent records.
        if line.is_empty() {
            continue;
        }

        // If the DDB section marker was found then we can stop parsing extents.
        if starts_ddb_section(line) {
            break;
        }

        // Ignore ordinary comments within the extent section.
        if line.starts_with('#') {
            continue;
        }

        // Preserve descriptor order when constructing the virtual extent map.
        extents.push(parse_extent(line)?);
    }

    if extents.is_empty() {
        return Err(ParseError::MissingField("FLAT extent"));
    }

    if matches!(create_type, CreateType::MonolithicFlat) && extents.len() != 1 {
        return Err(ParseError::Invalid(
            "monolithicFlat descriptors must contain exactly one extent".into(),
        ));
    }

    Ok(extents)
}

/// Parses the descriptor header and leaves `lines` positioned after the extent marker.
fn parse_header<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Result<CreateType, ParseError> {
    // The canonical descriptor header must be the first line in the file.
    let first_line = lines
        .next()
        .ok_or(ParseError::MissingField(DESCRIPTOR_HEADER))?;
    if first_line.trim_end() != DESCRIPTOR_HEADER {
        return Err(ParseError::Invalid(format!(
            "expected '{DESCRIPTOR_HEADER}', found {first_line:?}"
        )));
    }

    let mut seen_version = false;
    let mut seen_parent_cid = false;
    let mut create_type = None;
    let mut seen_extent_marker = false;

    // Unknown fields are allowed because descriptor producers may include
    // metadata this backend does not need to interpret.
    for line in lines {
        let line = line.trim();

        // If extent marker was seen then we are finished with header section.
        if line == EXTENT_MARKER {
            seen_extent_marker = true;
            break;
        }
        // DDB belongs after the extents, so reaching it here means the extent
        // section is missing.
        if starts_ddb_section(line) {
            break;
        }
        // Blank lines and ordinary comments are allowed between header fields.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Parse recognized header fields and ignore unrecognized metadata.
        let Some((key, value)) = line.split_once('=') else {
            return Err(ParseError::Invalid(format!(
                "expected a key/value field, found {line:?}"
            )));
        };
        let key = key.trim();
        match key {
            "version" => {
                if seen_version {
                    return Err(ParseError::Invalid(format!(
                        "duplicate 'version' field: {line:?}"
                    )));
                }
                seen_version = true;

                let value = value.trim().trim_matches('"');
                if value != "1" {
                    return Err(ParseError::Invalid(format!(
                        "unsupported descriptor version '{value}'"
                    )));
                }
            }
            "createType" => {
                if create_type.is_some() {
                    return Err(ParseError::Invalid(format!(
                        "duplicate 'createType' field: {line:?}"
                    )));
                }

                let value = value.trim().trim_matches('"');
                create_type = Some(match value {
                    "monolithicFlat" => CreateType::MonolithicFlat,
                    "twoGbMaxExtentFlat" => CreateType::TwoGbMaxExtentFlat,
                    value => {
                        return Err(ParseError::Invalid(format!(
                            "unsupported createType '{value}'"
                        )));
                    }
                });
            }
            "parentCID" => {
                if seen_parent_cid {
                    return Err(ParseError::Invalid(format!(
                        "duplicate 'parentCID' field: {line:?}"
                    )));
                }
                seen_parent_cid = true;

                let value = value.trim().trim_matches('"');
                if !value.eq_ignore_ascii_case("ffffffff") {
                    return Err(ParseError::Invalid(format!(
                        "unsupported parentCID '{value}': parent VMDK disks are not supported"
                    )));
                }
            }
            "parentFileNameHint" => {
                return Err(ParseError::Invalid(format!(
                    "parent VMDK disks are not supported: {line:?}"
                )));
            }
            "encoding" => {
                let value = value.trim().trim_matches('"');
                if !value.eq_ignore_ascii_case("UTF-8") {
                    return Err(ParseError::Invalid(format!(
                        "unsupported descriptor encoding '{value}': only UTF-8 is supported"
                    )));
                }
            }
            _ => {}
        }
    }

    // Ensure that the version was specified in the header.
    if !seen_version {
        return Err(ParseError::MissingField("version"));
    }

    // Ensure that the create type was specified in the header.
    let create_type = create_type.ok_or(ParseError::MissingField("createType"))?;

    // EOF or DDB before the marker means the extent section is missing.
    if !seen_extent_marker {
        return Err(ParseError::MissingField(EXTENT_MARKER));
    }

    Ok(create_type)
}

/// Parses one extent entry of the form `<access> <sectors> FLAT "<path>" [offset]`.
fn parse_extent(line: &str) -> Result<ParsedExtent, ParseError> {
    // Isolate the quoted path so spaces within it are preserved.
    let (prefix, path_and_remaining) = line
        .split_once('"')
        .ok_or_else(|| ParseError::Invalid(format!("extent path must be quoted: {line:?}")))?;
    let (path, remaining) = path_and_remaining
        .split_once('"')
        .ok_or_else(|| ParseError::Invalid(format!("unterminated extent path: {line:?}")))?;

    // Parse one of the access modes supported for source-backed extents.
    let mut fields = prefix.split_ascii_whitespace();
    let access = match fields
        .next()
        .ok_or_else(|| ParseError::Invalid(format!("missing extent access mode: {line:?}")))?
    {
        "RW" => ExtentAccess::ReadWrite,
        "RDONLY" => ExtentAccess::ReadOnly,
        access => {
            return Err(ParseError::Invalid(format!(
                "unsupported extent access mode '{access}'"
            )));
        }
    };

    // Parse the extent's virtual length and reject empty extents.
    let sector_count = fields
        .next()
        .ok_or_else(|| ParseError::Invalid(format!("missing extent sector count: {line:?}")))?;
    let sector_count = sector_count
        .parse::<u64>()
        .map_err(|_| ParseError::Invalid(format!("invalid sector count '{sector_count}'")))?;
    if sector_count == 0 {
        return Err(ParseError::Invalid(format!(
            "extent sector count must be nonzero: {line:?}"
        )));
    }

    // Reject sparse and other extent types.
    let extent_type = fields
        .next()
        .ok_or_else(|| ParseError::Invalid(format!("missing extent type: {line:?}")))?;
    if extent_type != "FLAT" {
        return Err(ParseError::Invalid(format!(
            "unsupported extent type '{extent_type}'"
        )));
    }

    // Require exactly three fields and whitespace before the opening path quote.
    if fields.next().is_some() || prefix.trim_ascii_end() == prefix {
        return Err(ParseError::Invalid(format!(
            "extent path must be quoted: {line:?}"
        )));
    }

    // Reject empty paths and paths containing control characters.
    if path.is_empty() || path.chars().any(char::is_control) {
        return Err(ParseError::Invalid(format!(
            "invalid extent path in {line:?}"
        )));
    }

    // Parse the optional starting sector in the source file and reject extra fields.
    let mut fields = remaining.split_ascii_whitespace();
    let source_start_sector = match fields.next() {
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| ParseError::Invalid(format!("invalid source start sector '{value}'")))?,
        None => 0,
    };

    // If there are additional fields then we should reject.
    if fields.next().is_some() {
        return Err(ParseError::Invalid(format!(
            "unexpected trailing extent fields: {line:?}"
        )));
    }

    Ok(ParsedExtent {
        access,
        path: path.to_owned(),
        sector_count,
        source_start_sector,
    })
}

/// Returns whether `line` starts or belongs to the DDB section.
fn starts_ddb_section(line: &str) -> bool {
    let is_marker = DDB_MARKERS
        .iter()
        .any(|marker| line.eq_ignore_ascii_case(marker));

    // Some descriptors omit the marker and begin with `ddb.adapterType = ...`.
    let is_entry = line
        .get(..DDB_ENTRY_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(DDB_ENTRY_PREFIX));

    is_marker || is_entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    // Builds a complete descriptor around the requested create type and
    // extent entries.
    fn descriptor(create_type: &str, extents: &str) -> String {
        format!(
            "# Disk DescriptorFile\n\
             version=1\n\
             CID=fffffffe\n\
             parentCID=ffffffff\n\
             createType=\"{create_type}\"\n\
             \n\
             # Extent description\n\
             {extents}\n\
             \n\
             # The Disk Data Base\n\
             #DDB\n\
             ddb.adapterType = \"ide\"\n"
        )
    }

    // Builds a minimal descriptor around caller-supplied header fields.
    fn descriptor_with_header(header: &str) -> String {
        format!(
            "# Disk DescriptorFile\n\
             {header}\n\
             # Extent description\n\
             RW 1 FLAT \"disk.bin\" 0\n\
             # The Disk Data Base\n"
        )
    }

    // Verifies that parsing fails with an Invalid error containing the
    // expected diagnostic text.
    fn assert_invalid(contents: &str, expected: &str) {
        match parse_descriptor(contents.as_bytes()) {
            Err(ParseError::Invalid(message)) => {
                assert!(message.contains(expected), "{message}");
            }
            Err(error) => panic!("expected an invalid descriptor error, got {error}"),
            Ok(_) => panic!("descriptor unexpectedly accepted:\n{contents}"),
        }
    }

    // Verifies that an Invalid error contains the escaped offending line.
    fn assert_invalid_line(contents: &str, line: &str) {
        assert_invalid(contents, &format!("{line:?}"));
    }

    // Verifies that parsing reports the expected required field as missing.
    fn assert_missing_field(contents: &str, expected: &'static str) {
        match parse_descriptor(contents.as_bytes()) {
            Err(ParseError::MissingField(field)) => assert_eq!(field, expected),
            Err(error) => panic!("expected missing field '{expected}', got {error}"),
            Ok(_) => panic!("descriptor unexpectedly accepted:\n{contents}"),
        }
    }

    // Verifies accepted line endings, access modes, quoted paths, default
    // offsets, and optional metadata.
    #[test]
    fn accepts_monolithic_flat_and_crlf() {
        // Exercise CRLF input, a quoted path containing spaces, read-only
        // access, and the default source offset.
        let descriptor = concat!(
            "# Disk DescriptorFile\r\n",
            "version=1\r\n",
            "CID=fffffffe\r\n",
            "parentCID=ffffffff\r\n",
            "createType=\"monolithicFlat\"\r\n",
            "\r\n",
            "# Extent description\r\n",
            "RDONLY 1 FLAT \"disk data.bin\"\r\n",
            "\r\n",
            "# The Disk Data Base\r\n",
            "#DDB\r\n",
            "ddb.adapterType = \"ide\"\r\n",
        );
        let parsed_extents = parse_descriptor(descriptor.as_bytes()).unwrap();
        assert_eq!(parsed_extents.len(), 1);
        assert_eq!(parsed_extents[0].access, ExtentAccess::ReadOnly);
        assert_eq!(parsed_extents[0].source_start_sector, 0);
        assert_eq!(parsed_extents[0].path, "disk data.bin");

        // parentCID and the DDB section are both optional.
        let descriptor = concat!(
            "# Disk DescriptorFile\n",
            "version=1\n",
            "createType=\"monolithicFlat\"\n",
            "# Extent description\n",
            "RDONLY 1 FLAT \"disk data.bin\"\n",
        );
        parse_descriptor(descriptor.as_bytes()).unwrap();
    }

    // Verifies supported header forms and rejects missing, duplicate, or
    // unsupported recognized fields.
    #[test]
    fn validates_header_fields() {
        // Supported values may be quoted or case-insensitive as appropriate,
        // while unknown header fields are ignored.
        parse_descriptor(
            descriptor_with_header(
                "version=\"1\"\n\
                 parentCID=FFFFFFFF\n\
                 createType=\"monolithicFlat\"\n\
                 encoding=\"utf-8\"\n\
                 unknownField=\"ignored\"",
            )
            .as_bytes(),
        )
        .unwrap();

        // Version and createType are the two required header fields.
        for (header, field) in [
            ("createType=\"monolithicFlat\"", "version"),
            ("version=1", "createType"),
        ] {
            assert_missing_field(&descriptor_with_header(header), field);
        }

        // Recognized fields reject duplicate or unsupported values.
        for (header, expected) in [
            (
                "version=2\n\
                 createType=\"monolithicFlat\"",
                "unsupported descriptor version '2'",
            ),
            (
                "version=1\n\
                 version=1\n\
                 createType=\"monolithicFlat\"",
                "duplicate 'version' field",
            ),
            (
                "version=1\n\
                 createType=\"monolithicFlat\"\n\
                 createType=\"monolithicFlat\"",
                "duplicate 'createType' field",
            ),
            (
                "version=1\n\
                 createType=\"monolithicSparse\"",
                "unsupported createType 'monolithicSparse'",
            ),
            (
                "version=1\n\
                 parentCID=ffffffff\n\
                 parentCID=ffffffff\n\
                 createType=\"monolithicFlat\"",
                "duplicate 'parentCID' field",
            ),
            (
                "version=1\n\
                 parentCID=12345678\n\
                 createType=\"monolithicFlat\"",
                "unsupported parentCID '12345678'",
            ),
            (
                "version=1\n\
                 parentCID=ffffffff\n\
                 parentFileNameHint=\"base.vmdk\"\n\
                 createType=\"monolithicFlat\"",
                "parent VMDK disks are not supported",
            ),
            (
                "version=1\n\
                 createType=\"monolithicFlat\"\n\
                 encoding=\"UTF-16\"",
                "unsupported descriptor encoding 'UTF-16'",
            ),
        ] {
            assert_invalid(&descriptor_with_header(header), expected);
        }
    }

    // Verifies the required descriptor signature, field syntax, section
    // ordering, extent presence, and text encoding.
    #[test]
    fn validates_descriptor_structure() {
        // Reject a missing descriptor header, extent marker, or extent record.
        for (contents, field) in [
            (String::new(), DESCRIPTOR_HEADER),
            (
                String::from(
                    "# Disk DescriptorFile\n\
                     version=1\n\
                     createType=\"twoGbMaxExtentFlat\"\n",
                ),
                EXTENT_MARKER,
            ),
            (
                String::from(
                    "# Disk DescriptorFile\n\
                     version=1\n\
                     createType=\"twoGbMaxExtentFlat\"\n\
                     #DDB\n\
                     # Extent description\n\
                     RW 1 FLAT \"disk.bin\" 0\n",
                ),
                EXTENT_MARKER,
            ),
            (descriptor("twoGbMaxExtentFlat", ""), "FLAT extent"),
        ] {
            assert_missing_field(&contents, field);
        }

        // Present structural elements must have valid contents and ordering.
        for (contents, expected) in [
            (
                String::from("not a descriptor"),
                "expected '# Disk DescriptorFile'",
            ),
            (
                descriptor_with_header(
                    "version=1\n\
                     not a key/value field\n\
                     createType=\"twoGbMaxExtentFlat\"",
                ),
                "expected a key/value field",
            ),
            (
                descriptor_with_header(
                    "version=1\n\
                     CID=ffff\0fffe\n\
                     createType=\"twoGbMaxExtentFlat\"",
                ),
                "NUL bytes are not allowed",
            ),
        ] {
            assert_invalid(&contents, expected);
        }

        // Non-UTF-8 byte input is rejected before parsing text fields.
        assert!(matches!(
            parse_descriptor(&[0xff]),
            Err(ParseError::InvalidEncoding(_))
        ));
    }

    // Verifies that a malformed header error identifies its source line.
    #[test]
    fn header_error_includes_source_line() {
        assert_invalid_line(
            &descriptor_with_header(
                "version=1\n\
                 not a key/value field\n\
                 createType=\"monolithicFlat\"",
            ),
            "not a key/value field",
        );
    }

    // Verifies rejection of invalid extent fields and incompatible layout
    // combinations.
    #[test]
    fn rejects_invalid_extent_entries_and_layouts() {
        // Cover unsupported access and extent types, invalid sizes and paths,
        // trailing fields, and the single-extent monolithic requirement.
        for descriptor in [
            descriptor("twoGbMaxExtentFlat", "NOACCESS 1 FLAT \"disk.bin\" 0"),
            descriptor("twoGbMaxExtentFlat", "RW 1 ZERO \"disk.bin\" 0"),
            descriptor("twoGbMaxExtentFlat", "RW 0 FLAT \"disk.bin\" 0"),
            descriptor("twoGbMaxExtentFlat", "RW 1 FLAT \"\" 0"),
            descriptor("twoGbMaxExtentFlat", "RW 1 FLAT \"bad\tpath\" 0"),
            descriptor("twoGbMaxExtentFlat", "RW 1 FLAT \"disk.bin\" 0 trailing"),
            descriptor(
                "monolithicFlat",
                "RW 1 FLAT \"a.bin\" 0\nRW 1 FLAT \"b.bin\" 0",
            ),
        ] {
            assert!(
                parse_descriptor(descriptor.as_bytes()).is_err(),
                "descriptor unexpectedly accepted:\n{descriptor}"
            );
        }
    }

    // Verifies that a malformed extent error identifies its source line.
    #[test]
    fn extent_error_includes_source_line() {
        assert_invalid_line(
            &descriptor("monolithicFlat", "RW 1 FLAT \"disk.bin\" 0 trailing"),
            "RW 1 FLAT \"disk.bin\" 0 trailing",
        );
    }

    // Verifies that a large real-world Kata layout preserves all ordered
    // extents and their aggregate size.
    #[cfg(unix)]
    #[test]
    fn parses_kata_32_layer_shape() {
        // Reproduce Kata's 32-layer layout to verify that all 64 ordered
        // extents are retained without imposing an artificial count limit.
        let mut extents =
            String::from("RW 2048 FLAT \"/run/kata-containers/sandbox/gpt_meta_head.img\" 0\n");
        for layer in (2..=32).rev() {
            extents.push_str(&format!(
                "RW 16 FLAT \"/var/lib/containerd/erofs/snapshots/{layer}/layer.erofs\" 0\n"
            ));
            extents.push_str(&format!(
                "RW 2032 FLAT \"/run/kata-containers/sandbox/pad-{}.img\" 0\n",
                33 - layer
            ));
        }
        extents.push_str("RW 16832 FLAT \"/var/lib/containerd/erofs/snapshots/1/layer.erofs\" 0");

        let parsed_extents =
            parse_descriptor(descriptor("twoGbMaxExtentFlat", &extents).as_bytes()).unwrap();
        assert_eq!(parsed_extents.len(), 64);
        assert_eq!(parsed_extents[0].access, ExtentAccess::ReadWrite);
        assert_eq!(
            parsed_extents
                .iter()
                .map(|extent| extent.sector_count)
                .sum::<u64>(),
            82368
        );
        assert_eq!(
            parsed_extents.first().unwrap().path,
            "/run/kata-containers/sandbox/gpt_meta_head.img"
        );
        assert_eq!(
            parsed_extents.last().unwrap().path,
            "/var/lib/containerd/erofs/snapshots/1/layer.erofs"
        );
    }
}
