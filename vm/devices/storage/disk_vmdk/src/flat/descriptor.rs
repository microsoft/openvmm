// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parses flat VMDK text descriptors into the ordered `FLAT` extents that
//! make up the virtual disk.

use crate::ParseError;

/// Access mode declared for an extent.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ExtentAccess {
    /// The descriptor declares the extent readable and writable.
    ReadWrite,
    /// The descriptor declares the extent read-only.
    ReadOnly,
}

/// One `FLAT` extent from the descriptor.
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

/// Parses the supported subset of a VMDK text descriptor.
///
/// The first line must be the descriptor signature. Other `#` lines are
/// comments, lines starting with an access mode are extents, and all remaining
/// lines are `key=value` fields, which may appear in any order.
pub(crate) fn parse_descriptor(bytes: &[u8]) -> Result<Vec<ParsedExtent>, ParseError> {
    let text = std::str::from_utf8(bytes)?;
    let mut lines = text.lines().map(str::trim);

    // Accept the standard signature spellings.
    let signature = lines.next().unwrap_or_default();
    if !matches!(
        signature,
        "# Disk DescriptorFile"
            | "# Disk Descriptor File"
            | "#Disk DescriptorFile"
            | "#Disk Descriptor File"
    ) {
        return Err(ParseError::Invalid(format!(
            "expected a VMDK descriptor signature: {signature:?}"
        )));
    }

    let mut has_version = false;
    let mut create_type = None;
    let mut extents = Vec::new();

    // Classify each remaining line as a comment, an extent, or a field.
    // Extents are collected in order, and recognized fields are validated.
    for line in lines {
        // Skip blank lines and comments, including section markers.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Lines starting with an access mode are extents, kept in disk order.
        if matches!(
            line.split_ascii_whitespace().next(),
            Some("RW" | "RDONLY" | "NOACCESS")
        ) {
            extents.push(parse_extent(line)?);
            continue;
        }

        // Every other line must be a `key=value` field.
        let (key, value) = line.split_once('=').ok_or_else(|| {
            ParseError::Invalid(format!("expected an extent or key/value field: {line:?}"))
        })?;

        let value = value.trim().trim_matches('"');

        // Validate the fields that affect the layout and ignore the rest.
        match key.trim() {
            // Only version 1 descriptors are supported.
            "version" if value == "1" => has_version = true,

            // Only flat disk types are supported.
            "createType" if matches!(value, "monolithicFlat" | "twoGbMaxExtentFlat") => {
                create_type = Some(value);
            }

            // `ffffffff` means the disk has no parent.
            "parentCID" if value.eq_ignore_ascii_case("ffffffff") => {}

            // The descriptor text is already known to be UTF-8.
            "encoding" if value.eq_ignore_ascii_case("UTF-8") => {}

            // Any other value of a recognized field, or any parent file hint,
            // describes a disk this backend cannot open.
            "version" | "createType" | "parentCID" | "parentFileNameHint" | "encoding" => {
                return Err(ParseError::Invalid(format!("unsupported field: {line:?}")));
            }

            // Other fields, do not affect the layout.
            _ => {}
        }
    }

    if !has_version {
        return Err(ParseError::MissingField("version"));
    }

    let create_type = create_type.ok_or(ParseError::MissingField("createType"))?;

    // A disk must contain at least one extent.
    if extents.is_empty() {
        return Err(ParseError::MissingField("FLAT extent"));
    }

    // A monolithicFlat disk stores all of its data in a single extent.
    if create_type == "monolithicFlat" && extents.len() != 1 {
        return Err(ParseError::Invalid(
            "monolithicFlat descriptors must contain exactly one extent".into(),
        ));
    }

    Ok(extents)
}

/// Parses an extent line of the form `<access> <sectors> FLAT "<path>" [<offset>]`.
fn parse_extent(line: &str) -> Result<ParsedExtent, ParseError> {
    let malformed = || ParseError::Invalid(format!("malformed extent: {line:?}"));

    // Split around the quoted path so spaces within it are preserved.
    let (fields, rest) = line.split_once('"').ok_or_else(malformed)?;
    let (path, offset) = rest.split_once('"').ok_or_else(malformed)?;

    // Exactly three fields must precede the path.
    let [access, sector_count, extent_type] =
        fields.split_ascii_whitespace().collect::<Vec<_>>()[..]
    else {
        return Err(malformed());
    };

    // The path may be followed by an optional starting sector, which
    // defaults to 0.
    let source_start_sector = match offset.split_ascii_whitespace().collect::<Vec<_>>()[..] {
        [] => 0,
        [offset] => offset.parse().map_err(|_| malformed())?,
        _ => return Err(malformed()),
    };

    // Only readable access modes are supported.
    let access = match access {
        "RW" => ExtentAccess::ReadWrite,
        "RDONLY" => ExtentAccess::ReadOnly,
        _ => {
            return Err(ParseError::Invalid(format!(
                "unsupported extent access mode: {line:?}"
            )));
        }
    };

    // Only flat extents are supported.
    if extent_type != "FLAT" {
        return Err(ParseError::Invalid(format!(
            "unsupported extent type: {line:?}"
        )));
    }

    // An extent must name a source file.
    if path.is_empty() {
        return Err(malformed());
    }

    Ok(ParsedExtent {
        access,
        path: path.to_owned(),
        sector_count: sector_count.parse().map_err(|_| malformed())?,
        source_start_sector,
    })
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

    // Verifies that parsing reports the expected required field as missing.
    fn assert_missing_field(contents: &str, expected: &'static str) {
        match parse_descriptor(contents.as_bytes()) {
            Err(ParseError::MissingField(field)) => assert_eq!(field, expected),
            Err(error) => panic!("expected missing field '{expected}', got {error}"),
            Ok(_) => panic!("descriptor unexpectedly accepted:\n{contents}"),
        }
    }

    // Verifies accepted signature spellings, line endings, access modes,
    // quoted paths, and default offsets.
    #[test]
    fn accepts_monolithic_flat_and_crlf() {
        // Exercise CRLF input, a quoted path containing spaces, read-only
        // access, and the default source offset.
        let body = concat!(
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

        // The signature line may be padded and use any standard spelling.
        for signature in [
            "# Disk DescriptorFile",
            " # Disk DescriptorFile ",
            "# Disk Descriptor File",
            "#Disk DescriptorFile",
            "#Disk Descriptor File",
        ] {
            let parsed_extents =
                parse_descriptor(format!("{signature}\r\n{body}").as_bytes()).unwrap();
            assert_eq!(parsed_extents.len(), 1);
            assert_eq!(parsed_extents[0].access, ExtentAccess::ReadOnly);
            assert_eq!(parsed_extents[0].source_start_sector, 0);
            assert_eq!(parsed_extents[0].path, "disk data.bin");
        }
    }

    // Verifies that section markers are ordinary comments. Markers may be
    // absent or spelled differently, fields may appear before, between, or
    // after extents, and extents are classified by their access mode even when
    // their paths contain `=`.
    #[test]
    fn treats_section_markers_as_comments() {
        let descriptor = concat!(
            "# Disk DescriptorFile\n",
            "version=1\n",
            "RW 1 FLAT \"first=1.bin\" 0\n",
            "# The disk Data Base \n",
            "ddb.adapterType = \"ide\"\n",
            "RDONLY 2 FLAT \"second.bin\" 4\n",
            "createType=\"twoGbMaxExtentFlat\"\n",
        );
        let parsed_extents = parse_descriptor(descriptor.as_bytes()).unwrap();
        let layout = parsed_extents
            .iter()
            .map(|extent| {
                (
                    extent.path.as_str(),
                    extent.sector_count,
                    extent.source_start_sector,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(layout, [("first=1.bin", 1, 0), ("second.bin", 2, 4)]);
    }

    // Verifies accepted field forms and rejects missing fields and unsupported
    // values of recognized fields.
    #[test]
    fn validates_header_fields() {
        // Supported values may be quoted or case-insensitive as appropriate.
        parse_descriptor(
            descriptor_with_header(
                "version=\"1\"\n\
                 parentCID=FFFFFFFF\n\
                 createType=\"monolithicFlat\"\n\
                 encoding=\"utf-8\"",
            )
            .as_bytes(),
        )
        .unwrap();

        // Version and createType are the two required fields.
        for (header, field) in [
            ("createType=\"monolithicFlat\"", "version"),
            ("version=1", "createType"),
        ] {
            assert_missing_field(&descriptor_with_header(header), field);
        }

        // Every occurrence of a recognized field must have a supported value.
        for field in [
            "version=2",
            "createType=\"monolithicSparse\"",
            "parentCID=12345678",
            "parentFileNameHint=\"base.vmdk\"",
            "encoding=\"UTF-16\"",
        ] {
            assert_invalid(
                &descriptor_with_header(&format!(
                    "version=1\ncreateType=\"monolithicFlat\"\n{field}"
                )),
                &format!("unsupported field: {field:?}"),
            );
        }
    }

    // Verifies the descriptor signature, line syntax, extent presence, and
    // text encoding.
    #[test]
    fn validates_descriptor_structure() {
        assert_missing_field(&descriptor("twoGbMaxExtentFlat", ""), "FLAT extent");

        // Errors identify the offending line.
        let unrecognized_line = descriptor_with_header(
            "version=1\n\
             not a key/value field\n\
             createType=\"twoGbMaxExtentFlat\"",
        );
        for (contents, expected) in [
            ("", "expected a VMDK descriptor signature: \"\""),
            (
                "not a descriptor",
                "expected a VMDK descriptor signature: \"not a descriptor\"",
            ),
            (
                unrecognized_line.as_str(),
                "expected an extent or key/value field: \"not a key/value field\"",
            ),
        ] {
            assert_invalid(contents, expected);
        }

        // Non-UTF-8 byte input is rejected before parsing text fields.
        assert!(matches!(
            parse_descriptor(&[0xff]),
            Err(ParseError::InvalidEncoding(_))
        ));
    }

    // Verifies rejection of malformed or unsupported extents and of
    // multi-extent monolithicFlat descriptors. Extent errors identify the
    // offending line.
    #[test]
    fn rejects_invalid_extent_entries_and_layouts() {
        for (extent, reason) in [
            (
                "NOACCESS 1 FLAT \"disk.bin\" 0",
                "unsupported extent access mode",
            ),
            ("RW 1 ZERO \"disk.bin\" 0", "unsupported extent type"),
            ("RW 1 FLAT disk.bin 0", "malformed extent"),
            ("RW 1 FLAT \"\" 0", "malformed extent"),
            ("RW one FLAT \"disk.bin\" 0", "malformed extent"),
            ("RW 1 FLAT \"disk.bin\" 0 trailing", "malformed extent"),
        ] {
            assert_invalid(
                &descriptor("twoGbMaxExtentFlat", extent),
                &format!("{reason}: {extent:?}"),
            );
        }

        assert_invalid(
            &descriptor(
                "monolithicFlat",
                "RW 1 FLAT \"a.bin\" 0\nRW 1 FLAT \"b.bin\" 0",
            ),
            "monolithicFlat descriptors must contain exactly one extent",
        );
    }

    // Verifies that a large real-world Kata layout preserves all ordered
    // extents and their aggregate size.
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
