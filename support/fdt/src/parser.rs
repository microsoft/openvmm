// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to parse a Flattened DeviceTree binary blob.

use super::spec;
use super::spec::U32b;
use super::spec::U64b;
use core::fmt::Display;
use core::mem::size_of;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

/// Errors returned when parsing a FDT.
#[derive(Debug)]
pub struct Error<'a>(ErrorKind<'a>);

impl Display for Error<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

// TODO: Once core::error::Error is stablized, we can remove this feature gate.
impl core::error::Error for Error<'_> {}

/// Types of errors when parsing a FDT.
#[derive(Debug)]
enum ErrorKind<'a> {
    /// Buffer is not aligned to u32
    BufferAlignment,
    /// Buffer too small for fixed header
    NoHeader,
    /// Fixed header magic invalid
    HeaderMagic,
    /// Total size described in the fixed header is greater than buffer provided
    HeaderTotalSize,
    /// Header version is invalid
    HeaderVersion,
    /// Structure block not contained within buffer
    StructureBlock,
    /// Structure block not aligned to u32
    StructureBlockAlignment,
    /// Memory reservation block not contained within buffer
    MemoryReservationBlock,
    /// Memory reservation block did not end with an empty entry
    MemoryReservationBlockEnd,
    /// Strings block not contained within buffer
    StringsBlock,
    /// No root node present
    RootNode,
    /// More than one node at the root
    MultipleRootNodes,
    /// Unable to parse FDT token when parsing nodes
    NodeToken(ParseTokenError),
    /// Unexpected token when parsing begin node
    NodeBegin(u32),
    /// Unexpected token when parsing node properties
    NodeProp(u32),
    /// Unexpected token when parsing children nodes
    NodeChildren(u32),
    /// Property data buffer len is not a multiple of requested type size
    PropertyDataTypeBuffer {
        node_name: &'a str,
        prop_name: &'a str,
    },
    /// Property requested at offset is larger than data buffer
    PropertyOffset {
        node_name: &'a str,
        prop_name: &'a str,
    },
    /// Property data is not a a valid string
    PropertyStr {
        node_name: &'a str,
        error: StringError,
    },
    /// Unable to parse FDT token when parsing properties
    PropertyTokenParse {
        node_name: &'a str,
        error: ParseTokenError,
    },
    /// Unexpected FDT token when parsing properties
    PropertyToken { node_name: &'a str, token: u32 },
    /// Property name string is not a valid string
    PropertyNameStr {
        node_name: &'a str,
        error: StringError,
    },
    /// FDT end token not present at end of structure block
    FdtEnd,
}

impl Display for ErrorKind<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ErrorKind::BufferAlignment => f.write_str("Buffer is not aligned to u32"),
            ErrorKind::NoHeader => f.write_str("Buffer too small for fixed FDT header"),
            ErrorKind::HeaderMagic => f.write_str("FDT header magic field invalid"),
            ErrorKind::HeaderTotalSize => {
                f.write_str("FDT header total size greater than provided buffer")
            }
            ErrorKind::HeaderVersion => f.write_str("FDT header version invalid"),
            ErrorKind::StructureBlock => f.write_str("Structure block not contained within buffer"),
            ErrorKind::StructureBlockAlignment => {
                f.write_str("Structure block offset is not aligned to u32")
            }
            ErrorKind::MemoryReservationBlock => {
                f.write_str("Memory reservation block not contained within buffer")
            }
            ErrorKind::MemoryReservationBlockEnd => {
                f.write_str("Memory reservation block did not end with an empty entry")
            }
            ErrorKind::StringsBlock => f.write_str("Strings block not contained within buffer"),
            ErrorKind::RootNode => f.write_str("No root node present"),
            ErrorKind::MultipleRootNodes => f.write_str("More than one node at the root"),
            ErrorKind::NodeToken(e) => f.write_fmt(format_args!(
                "Unable to parse FDT token when parsing nodes {}",
                e
            )),
            ErrorKind::NodeBegin(token) => f.write_fmt(format_args!(
                "Unexpected token when parsing begin node {}",
                token
            )),
            ErrorKind::NodeProp(token) => f.write_fmt(format_args!(
                "Unexpected token when parsing node properties {}",
                token
            )),
            ErrorKind::NodeChildren(token) => f.write_fmt(format_args!(
                "Unexpected token when parsing children nodes {}",
                token
            )),
            ErrorKind::PropertyDataTypeBuffer { node_name, prop_name } => f.write_fmt(format_args!(
                "Property {prop_name} data buffer len is not multiple of type size for node {node_name}"
            )),
            ErrorKind::PropertyOffset { node_name, prop_name } => f.write_fmt(format_args!(
                "Property {prop_name} requested at offset is larger than data buffer for node {node_name}"
            )),
            ErrorKind::PropertyStr { node_name, error } => f.write_fmt(format_args!(
                "Property data is not a a valid string for node {node_name}: {error}"
            )),
            ErrorKind::PropertyTokenParse { node_name, error } => f.write_fmt(format_args!(
                "Unable to parse FDT token when parsing properties for node {node_name}: {error}",
            )),
            ErrorKind::PropertyToken { node_name, token } => f.write_fmt(format_args!(
                "Unexpected FDT token when parsing properties for node {node_name}: {}",
                token
            )),
            ErrorKind::PropertyNameStr { node_name, error } => f.write_fmt(format_args!(
                "Property name string is not a valid string for node {node_name}: {error}",
            )),
            ErrorKind::FdtEnd => f.write_str("FDT end token not present at end of structure block"),
        }
    }
}

/// A parser used to parse a FDT.
pub struct Parser<'a> {
    /// The total size used by the dt.
    pub total_size: usize,
    /// The strings block.
    strings_block: &'a [u8],
    /// The structure block.
    structure_block: &'a [u8],
    /// The bsp reg field
    pub boot_cpuid_phys: u32,
    /// The memory reservations blocks without the final empty entry.
    memory_reservations: &'a [u8],
}

impl<'a> Parser<'a> {
    /// Read just the `totalsize` field of a FDT header. This is useful when
    /// attempting to determine the overall size of a device tree.
    pub fn read_total_size(buf: &[u8]) -> Result<usize, Error<'a>> {
        let header = spec::Header::read_from_prefix(buf)
            .map_err(|_| Error(ErrorKind::NoHeader))?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        if u32::from(header.magic) != spec::MAGIC {
            Err(Error(ErrorKind::HeaderMagic))
        } else {
            Ok(u32::from(header.totalsize) as usize)
        }
    }

    /// Create a new instance of a FDT parser.
    pub fn new(buf: &'a [u8]) -> Result<Self, Error<'a>> {
        if !(buf.as_ptr() as usize).is_multiple_of(size_of::<u32>()) {
            return Err(Error(ErrorKind::BufferAlignment));
        }

        let header = spec::Header::read_from_prefix(buf)
            .map_err(|_| Error(ErrorKind::NoHeader))?
            .0; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        if u32::from(header.magic) != spec::MAGIC {
            return Err(Error(ErrorKind::HeaderMagic));
        }

        // Validate total size within buf.
        let total_size = u32::from(header.totalsize) as usize;
        if total_size > buf.len() {
            return Err(Error(ErrorKind::HeaderTotalSize));
        }

        if u32::from(header.version) < spec::CURRENT_VERSION
            || u32::from(header.last_comp_version) > spec::COMPAT_VERSION
        {
            return Err(Error(ErrorKind::HeaderVersion));
        }

        // Validate the mem_rsvmap region ends with an empty entry. Currently
        // the parser does not make these values accessible.
        let mem_rsvmap_offset = u32::from(header.off_mem_rsvmap) as usize;
        let mut memory_reservations_len = 0;
        let mut mem_rsvmap = buf
            .get(mem_rsvmap_offset..)
            .ok_or(Error(ErrorKind::MemoryReservationBlock))?;
        loop {
            let (entry, rest) = spec::ReserveEntry::read_from_prefix(mem_rsvmap)
                .map_err(|_| Error(ErrorKind::MemoryReservationBlockEnd))?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

            if u64::from(entry.address) == 0 && u64::from(entry.size) == 0 {
                break;
            }

            mem_rsvmap = rest;
            memory_reservations_len += size_of::<spec::ReserveEntry>();
        }

        let memory_reservations = buf
            .get(mem_rsvmap_offset..(mem_rsvmap_offset + memory_reservations_len))
            .ok_or(Error(ErrorKind::MemoryReservationBlock))?;

        let struct_offset = u32::from(header.off_dt_struct) as usize;
        let struct_len = u32::from(header.size_dt_struct) as usize;

        if !struct_offset.is_multiple_of(size_of::<u32>()) {
            return Err(Error(ErrorKind::StructureBlockAlignment));
        }

        let structure_block = buf
            .get(struct_offset..(struct_offset + struct_len))
            .ok_or(Error(ErrorKind::StructureBlock))?;

        // FDT_END must be the last token in the structure block. Ignore it once
        // checked.
        let structure_block = structure_block
            .strip_suffix(&spec::END.to_be_bytes())
            .ok_or(Error(ErrorKind::FdtEnd))?;

        let strings_offset = u32::from(header.off_dt_strings) as usize;
        let strings_len = u32::from(header.size_dt_strings) as usize;
        let strings_block = buf
            .get(strings_offset..(strings_offset + strings_len))
            .ok_or(Error(ErrorKind::StringsBlock))?;

        Ok(Self {
            total_size,
            strings_block,
            structure_block,
            memory_reservations,
            boot_cpuid_phys: header.boot_cpuid_phys.into(),
        })
    }

    /// Returns the root node of this FDT.
    pub fn root<'b>(&'b self) -> Result<Node<'a>, Error<'a>> {
        let mut iter = NodeIter {
            strings_block: self.strings_block,
            nodes: self.structure_block,
        };

        let root = iter.next().ok_or(Error(ErrorKind::RootNode))??;

        if iter.next().is_some() {
            Err(Error(ErrorKind::MultipleRootNodes))
        } else {
            Ok(root)
        }
    }

    /// Returns an iterator to parse through memory reservations.
    pub fn memory_reservations(&self) -> MemoryReserveIter<'a> {
        MemoryReserveIter {
            memory_reservations: self.memory_reservations,
        }
    }
}

/// Get a string from the strings block at the given offset.
fn string_from_offset(strings_block: &[u8], offset: U32b) -> Result<&str, StringError> {
    let offset = u32::from(offset) as usize;

    extract_str_from_bytes(strings_block.get(offset..).ok_or(StringError::Offset)?)
}

/// An iterator to parse through FDT nodes.
pub struct NodeIter<'a> {
    strings_block: &'a [u8],
    nodes: &'a [u8],
}

enum ParsedToken<'a> {
    BeginNode { name: &'a str },
    Property { name_offset: U32b, data: &'a [u8] },
    EndNode,
    Nop,
    End,
}

impl ParsedToken<'_> {
    fn raw(&self) -> u32 {
        match self {
            ParsedToken::BeginNode { .. } => spec::BEGIN_NODE,
            ParsedToken::Property { .. } => spec::PROP,
            ParsedToken::EndNode => spec::END_NODE,
            ParsedToken::Nop => spec::NOP,
            ParsedToken::End => spec::END,
        }
    }
}

/// Errors returned when parsing FDT tokens.
#[derive(Debug)]
enum ParseTokenError {
    /// Unknown token
    Unknown(u32),
    /// Buf too small
    BufLen,
    /// Buf too small for prop header
    PropHeader,
    /// Buf too small for prop data described in prop header
    PropData,
    /// Begin node name is not valid
    BeginName(StringError),
    /// Buf too small for begin node name alignment
    BeginNameAlignment,
}

impl Display for ParseTokenError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ParseTokenError::Unknown(token) => {
                f.write_fmt(format_args!("Unknown FDT token {}", token))
            }
            ParseTokenError::BufLen => f.write_str("Buffer too small to read token"),
            ParseTokenError::PropHeader => f.write_str("Buffer too small to read property header"),
            ParseTokenError::PropData => {
                f.write_str("Buffer too small to read property data encoded in property header")
            }
            ParseTokenError::BeginName(e) => {
                f.write_fmt(format_args!("Node name is not valid {}", e))
            }
            ParseTokenError::BeginNameAlignment => {
                f.write_str("Buffer is too small for begin node name alignment")
            }
        }
    }
}

/// Read to the next token from `buf`, returning `(token, remaining_buffer)`.
fn read_token(buf: &[u8]) -> Result<(ParsedToken<'_>, &[u8]), ParseTokenError> {
    let (token, rest) = U32b::read_from_prefix(buf).map_err(|_| ParseTokenError::BufLen)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
    let token = u32::from(token);
    match token {
        spec::BEGIN_NODE => {
            // Extract the node's name.
            let name = extract_str_from_bytes(rest).map_err(ParseTokenError::BeginName)?;

            // The string extracted does not contain the null terminator. Add
            // the length and align up the total size.
            let aligned_str_len = ((name.len() + 1) + 4 - 1) & !(4 - 1);

            // Attempt to extract the remainder of the slice, not including the
            // aligned padding bytes.
            let rest = rest
                .get(aligned_str_len..)
                .ok_or(ParseTokenError::BeginNameAlignment)?;

            Ok((ParsedToken::BeginNode { name }, rest))
        }
        spec::PROP => {
            // Read the property header
            let (header, rest) = spec::PropHeader::read_from_prefix(rest)
                .map_err(|_| ParseTokenError::PropHeader)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
            let len = u32::from(header.len) as usize;
            let align_up_len = (len + 4 - 1) & !(4 - 1);

            if align_up_len > rest.len() {
                return Err(ParseTokenError::PropData);
            }

            // Only return the non-aligned data buf
            let data = &rest[..len];
            let (_, rest) = rest.split_at(align_up_len);

            Ok((
                ParsedToken::Property {
                    name_offset: header.nameoff,
                    data,
                },
                rest,
            ))
        }
        spec::END_NODE => Ok((ParsedToken::EndNode, rest)),
        spec::NOP => Ok((ParsedToken::Nop, rest)),
        spec::END => Ok((ParsedToken::End, rest)),
        _ => Err(ParseTokenError::Unknown(token)),
    }
}

impl<'a> NodeIter<'a> {
    fn parse(&mut self) -> Result<Option<Node<'a>>, ErrorKind<'a>> {
        while !self.nodes.is_empty() {
            // Parse the next token.
            let (token, rest) = read_token(self.nodes).map_err(ErrorKind::NodeToken)?;
            debug_assert!(rest.len() % size_of::<U32b>() == 0);

            let name = match token {
                ParsedToken::Nop => {
                    self.nodes = rest;
                    continue;
                }
                ParsedToken::BeginNode { name } => name,
                _ => return Err(ErrorKind::NodeBegin(token.raw())),
            };

            self.nodes = rest;

            // Find if there is a properties section, which comes before children.
            let mut prop = self.nodes;
            'prop: loop {
                let (token, rest) = read_token(prop).map_err(ErrorKind::NodeToken)?;
                match token {
                    ParsedToken::BeginNode { .. } => {
                        // Begin node means move to parsing children nodes.
                        break 'prop;
                    }
                    ParsedToken::EndNode => {
                        // End node means this node had no properties.
                        break 'prop;
                    }
                    ParsedToken::Property { .. } | ParsedToken::Nop => {}
                    token => return Err(ErrorKind::NodeProp(token.raw())),
                };

                prop = rest;
            }

            let (prop, rest) = self.nodes.split_at(self.nodes.len() - prop.len());
            self.nodes = rest;

            // Discover if there are any children, which are signified
            // by other BEGIN_NODE tokens.
            let mut children = self.nodes;
            let mut begin_node_count = 0;
            'children: loop {
                let (token, rest) = read_token(children).map_err(ErrorKind::NodeToken)?;
                match token {
                    ParsedToken::EndNode => {
                        if begin_node_count == 0 {
                            // End of current node
                            break 'children;
                        } else {
                            // Parsing child node, pop node count
                            begin_node_count -= 1;
                        }
                    }
                    ParsedToken::BeginNode { .. } => {
                        begin_node_count += 1;
                    }
                    ParsedToken::Property { .. } | ParsedToken::Nop => {}
                    token => return Err(ErrorKind::NodeChildren(token.raw())),
                };

                children = rest;
            }

            let (children, rest) = self.nodes.split_at(self.nodes.len() - children.len());
            self.nodes = rest;

            // Consume END_NODE and return the parsed node
            let (end_node, rest) = read_token(self.nodes).expect("should be end node");
            assert!(matches!(end_node, ParsedToken::EndNode));
            self.nodes = rest;

            return Ok(Some(Node {
                name,
                strings_block: self.strings_block,
                properties: prop,
                children,
            }));
        }

        Ok(None)
    }
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = Result<Node<'a>, Error<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.parse().map_err(Error).transpose()
    }
}

/// A parsed FDT node.
pub struct Node<'a> {
    /// The name for this node.
    pub name: &'a str,
    strings_block: &'a [u8],
    properties: &'a [u8],
    children: &'a [u8],
}

impl<'a> Node<'a> {
    /// Returns an iterator to parse through children of this node.
    pub fn children(&self) -> NodeIter<'a> {
        NodeIter {
            strings_block: self.strings_block,
            nodes: self.children,
        }
    }

    /// Returns an iterator to parse through properties of this node.
    pub fn properties(&self) -> PropertyIter<'a> {
        PropertyIter {
            node_name: self.name,
            strings_block: self.strings_block,
            properties: self.properties,
        }
    }

    /// Find a property with a given name.
    ///
    /// Returns `Ok(None)` if the property does not exist.
    ///
    /// Returns an error if this node's properties are unable to be parsed.
    ///
    /// This method is O(n) for the number of properties on this node, as the
    /// [`Self::properties`] is used to perform a linear search.
    pub fn find_property(&self, name: &str) -> Result<Option<Property<'a>>, Error<'a>> {
        for prop in self.properties() {
            let prop = prop?;

            if name == prop.name {
                return Ok(Some(prop));
            }
        }

        Ok(None)
    }
}

/// An iterator for FDT node properties.
pub struct PropertyIter<'a> {
    node_name: &'a str,
    strings_block: &'a [u8],
    properties: &'a [u8],
}

impl<'a> PropertyIter<'a> {
    fn parse(&mut self) -> Result<Option<Property<'a>>, ErrorKind<'a>> {
        while !self.properties.is_empty() {
            // Parse the next token.
            let (token, rest) =
                read_token(self.properties).map_err(|error| ErrorKind::PropertyTokenParse {
                    node_name: self.node_name,
                    error,
                })?;

            let (name_off, data, rest) = match token {
                ParsedToken::Nop => {
                    self.properties = rest;
                    continue;
                }
                ParsedToken::Property { name_offset, data } => (name_offset, data, rest),
                _ => {
                    return Err(ErrorKind::PropertyToken {
                        node_name: self.node_name,
                        token: token.raw(),
                    });
                }
            };

            // Read the property name
            let name = string_from_offset(self.strings_block, name_off).map_err(|error| {
                ErrorKind::PropertyNameStr {
                    node_name: self.node_name,
                    error,
                }
            })?;

            self.properties = rest;
            return Ok(Some(Property {
                node_name: self.node_name,
                name,
                data,
            }));
        }

        Ok(None)
    }
}

impl<'a> Iterator for PropertyIter<'a> {
    type Item = Result<Property<'a>, Error<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.parse().map_err(Error).transpose()
    }
}

/// A parsed FDT node property.
pub struct Property<'a> {
    node_name: &'a str,
    /// The name for this property.
    pub name: &'a str,
    /// Raw data for this property.
    pub data: &'a [u8],
}

impl<'a> Property<'a> {
    /// Read a value at a given offset, indexed by `size_of::<T>() * index`.
    /// T must be BigEndian.
    fn read_val<T: FromBytes + Copy + zerocopy::Unaligned + Immutable + KnownLayout>(
        &self,
        index: usize,
    ) -> Result<T, Error<'a>> {
        // self.data must be:
        //  - len must be multiple of size_of(T)
        //  - index must be within the constructed slice of T
        //
        // NOTE: The unaligned bound on T is due to the fact that FDT properties
        // are only guaranteed to sit on a 4 byte alignment boundary. Thus, to
        // read types that are greater than 4 bytes, we must bound T to accept
        // unaligned types so LayoutVerified does not apply alignment and read
        // incorrect values.
        <[T]>::ref_from_bytes(self.data)
            .map_err(|_| {
                // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                Error(ErrorKind::PropertyDataTypeBuffer {
                    node_name: self.node_name,
                    prop_name: self.name,
                })
            })?
            .get(index)
            .ok_or(Error(ErrorKind::PropertyOffset {
                node_name: self.node_name,
                prop_name: self.name,
            }))
            .copied()
    }

    /// Read a u32 from this property, at a given u32 index.
    pub fn read_u32(&self, index: usize) -> Result<u32, Error<'a>> {
        let val: u32 = self.read_val::<U32b>(index)?.into();

        Ok(val)
    }

    /// Read a u64 from this property, at a given u64 index.
    pub fn read_u64(&self, index: usize) -> Result<u64, Error<'a>> {
        let val: u64 = self.read_val::<U64b>(index)?.into();

        Ok(val)
    }

    /// Read the data as a `&str`.
    pub fn read_str(&self) -> Result<&'a str, Error<'a>> {
        extract_str_from_bytes(self.data).map_err(|error| {
            Error(ErrorKind::PropertyStr {
                node_name: self.node_name,
                error,
            })
        })
    }

    /// Read data as an iterator of u64 values.
    pub fn as_64_list(&self) -> Result<impl Iterator<Item = u64> + use<'a>, Error<'a>> {
        Ok(<[U64b]>::ref_from_bytes(self.data)
            .map_err(|_| {
                // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
                Error(ErrorKind::PropertyDataTypeBuffer {
                    node_name: self.node_name,
                    prop_name: self.name,
                })
            })?
            .iter()
            .map(|v| v.get()))
    }
}

/// Errors when reading a string from the FDT.
#[derive(Debug)]
enum StringError {
    /// Invalid string block offset
    Offset,
    /// No null terminator found
    Null,
    /// String is not utf8
    Utf8(core::str::Utf8Error),
}

impl Display for StringError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StringError::Offset => f.write_str("Invalid string block offset"),
            StringError::Null => f.write_str("No null terminator found"),
            StringError::Utf8(e) => f.write_fmt(format_args!("String is not utf8 {}", e)),
        }
    }
}

/// An iterator to parse through memory reservations.
pub struct MemoryReserveIter<'a> {
    memory_reservations: &'a [u8],
}

impl<'a> MemoryReserveIter<'a> {
    fn parse(&mut self) -> Result<Option<spec::ReserveEntry>, ErrorKind<'a>> {
        if self.memory_reservations.is_empty() {
            return Ok(None);
        }

        let (entry, rest) = spec::ReserveEntry::read_from_prefix(self.memory_reservations)
            .map_err(|_| ErrorKind::MemoryReservationBlock)?; // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)

        if u64::from(entry.address) == 0 && u64::from(entry.size) == 0 {
            return Ok(None);
        }

        self.memory_reservations = rest;

        Ok(Some(entry))
    }
}

impl<'a> Iterator for MemoryReserveIter<'a> {
    type Item = Result<spec::ReserveEntry, Error<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.parse().map_err(Error).transpose()
    }
}

impl core::error::Error for StringError {}

/// Extract a string from bytes treated as a C String, stopping at the first null terminator.
fn extract_str_from_bytes(bytes: &[u8]) -> Result<&str, StringError> {
    // Find the null terminator.
    // TODO: unstable CStr::from_bytes_until_nul would be nice here.
    let null_index = bytes
        .iter()
        .position(|char| *char == 0)
        .ok_or(StringError::Null)?;

    core::str::from_utf8(&bytes[..null_index]).map_err(StringError::Utf8)
}

#[cfg(test)]
mod test {
    extern crate alloc;

    use super::*;
    use crate::builder::Builder;
    use crate::builder::BuilderConfig;
    use crate::builder::StringId;
    use crate::spec::ReserveEntry;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use zerocopy::IntoBytes;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DtProp {
        PropA(u64),
        PropB(Vec<u8>),
        Reg(u32),
        SuperAwesomeProp(String),
        PropList(Vec<u64>),
    }

    #[derive(Debug, PartialEq, Eq)]
    struct DtNode {
        name: String,
        children: Vec<DtNode>,
        properties: Vec<DtProp>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Dt {
        boot_cpuid_phys: u32,
        root: DtNode,
        memory_reservations: Vec<ReserveEntry>,
    }

    struct PropIds {
        propa: StringId,
        propb: StringId,
        reg: StringId,
        saprop: StringId,
        proplist: StringId,
    }

    macro_rules! build_fdt_props {
        ($ids:expr, $node:expr, $builder:expr) => {{
            let mut new_builder = $builder.start_node(&$node.name).unwrap();

            for prop in &$node.properties {
                new_builder = match &prop {
                    DtProp::PropA(val) => new_builder.add_u64($ids.propa, *val).unwrap(),
                    DtProp::PropB(val) => new_builder.add_prop_array($ids.propb, &[&val]).unwrap(),
                    DtProp::Reg(val) => new_builder.add_u32($ids.reg, *val).unwrap(),
                    DtProp::SuperAwesomeProp(val) => new_builder.add_str($ids.saprop, val).unwrap(),
                    DtProp::PropList(val) => {
                        // convert to BE first, since the underlying routines require BE data
                        let big_endians = val
                            .iter()
                            .map(|v| {
                                zerocopy::byteorder::U64::<zerocopy::byteorder::BigEndian>::new(*v)
                            })
                            .collect::<Vec<_>>();

                        new_builder
                            .add_prop_array(
                                $ids.proplist,
                                big_endians
                                    .iter()
                                    .map(|v| v.as_bytes())
                                    .collect::<Vec<_>>()
                                    .as_slice(),
                            )
                            .unwrap()
                    }
                };
            }

            new_builder
        }};
    }

    impl Dt {
        fn build_fdt(&self) -> Vec<u8> {
            let mut buf = vec![0; 4096 * 256];
            let memory_reservations = vec![ReserveEntry {
                address: 1024.into(),
                size: 2048.into(),
            }];
            let mut builder = Builder::new(BuilderConfig {
                blob_buffer: buf.as_mut_slice(),
                string_table_cap: 1024,
                memory_reservations: &memory_reservations,
            })
            .unwrap();

            let ids = PropIds {
                propa: builder.add_string("prop-a").unwrap(),
                propb: builder.add_string("test,prop-b").unwrap(),
                reg: builder.add_string("reg").unwrap(),
                saprop: builder.add_string("Awesome,super-prop").unwrap(),
                proplist: builder.add_string("prop-list").unwrap(),
            };

            // build root
            let root = &self.root;
            let mut root_builder = build_fdt_props!(&ids, root, builder);

            // build L1 nodes
            for child in &root.children {
                let mut child_builder = build_fdt_props!(&ids, child, root_builder);

                // Build L2 nodes
                for child_l2 in &child.children {
                    child_builder = build_fdt_props!(&ids, child_l2, child_builder)
                        .end_node()
                        .unwrap();

                    assert!(child_l2.children.is_empty());
                }

                root_builder = child_builder.end_node().unwrap();
            }

            let builder = root_builder.end_node().unwrap();

            let len = builder.build(self.boot_cpuid_phys).unwrap();
            buf.truncate(len);
            buf
        }

        fn from_fdt(buf: &[u8]) -> Self {
            let parser = Parser::new(buf).unwrap();

            let parse_props = |parser: &Node<'_>, node: &mut DtNode| {
                for prop in parser.properties() {
                    let prop = prop.unwrap();
                    let name = prop.name;

                    let dt_prop = match name {
                        "prop-a" => DtProp::PropA(prop.read_u64(0).unwrap()),
                        "test,prop-b" => DtProp::PropB(prop.data.into()),
                        "reg" => DtProp::Reg(prop.read_u32(0).unwrap()),
                        "Awesome,super-prop" => {
                            DtProp::SuperAwesomeProp(prop.read_str().unwrap().into())
                        }
                        "prop-list" => {
                            let mut list = vec![];
                            for val in prop.as_64_list().unwrap() {
                                list.push(val);
                            }
                            DtProp::PropList(list)
                        }
                        _ => panic!("unexpected name {}", name),
                    };

                    node.properties.push(dt_prop);
                }
            };

            let root = parser.root().unwrap();
            let mut p_root = DtNode {
                name: root.name.into(),
                children: vec![],
                properties: vec![],
            };

            parse_props(&root, &mut p_root);

            for child in root.children() {
                let child = child.unwrap();

                let mut p_child = DtNode {
                    name: child.name.into(),
                    children: vec![],
                    properties: vec![],
                };

                parse_props(&child, &mut p_child);

                for child_l2 in child.children() {
                    let child_l2 = child_l2.unwrap();

                    let mut p_child_l2 = DtNode {
                        name: child_l2.name.into(),
                        children: vec![],
                        properties: vec![],
                    };

                    parse_props(&child_l2, &mut p_child_l2);

                    assert!(child_l2.children().next().is_none());

                    p_child.children.push(p_child_l2);
                }

                p_root.children.push(p_child);
            }

            let mut memory_reservations = vec![];
            parser.memory_reservations().for_each(|entry| {
                memory_reservations.push(entry.unwrap());
            });

            Dt {
                boot_cpuid_phys: parser.boot_cpuid_phys,
                root: p_root,
                memory_reservations,
            }
        }
    }

    fn cpu_node(num: usize, apic_id: u32) -> DtNode {
        DtNode {
            name: format!("cpu@{}", num),
            properties: vec![DtProp::Reg(apic_id)],
            children: vec![],
        }
    }

    #[test]
    fn test_simple_dt() {
        let dt = Dt {
            boot_cpuid_phys: 0,
            root: DtNode {
                name: "".into(),
                children: vec![DtNode {
                    name: "cpus".into(),
                    children: (0..10).map(|i| cpu_node(i, (i + 10) as u32)).collect(),
                    properties: vec![DtProp::SuperAwesomeProp("super".into())],
                }],
                properties: vec![
                    DtProp::PropA(0x123456789abcdef),
                    DtProp::PropB(vec![]),
                    DtProp::PropB(vec![1]),
                    DtProp::Reg(0xabcdef),
                    DtProp::SuperAwesomeProp("this is a string!".into()),
                    DtProp::PropList(vec![1, 2, 3, 4, 5]),
                    DtProp::PropA(0x223456789abcdef),
                ],
            },
            memory_reservations: vec![ReserveEntry {
                address: 1024.into(),
                size: 2048.into(),
            }],
        };

        let fdt = dt.build_fdt();
        let parsed_dt = Dt::from_fdt(&fdt);
        assert_eq!(dt, parsed_dt);
    }
}
