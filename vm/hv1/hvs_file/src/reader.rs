// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reader for HyperV Storage files.
//!
//! Opens existing `.vmrs` / `.vmcx` / `.vsv` files and provides access
//! to the key-value store. Read-only, current format version only.

use crate::defs::*;
use crate::struct_checksum;
use core::mem::offset_of;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Error type for reader operations.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// An I/O error occurred while reading the file.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// The file header signature is invalid.
    #[error("invalid header signature: {0:#x}")]
    BadHeaderSignature(u32),
    /// The file header checksum does not match.
    #[error("header checksum mismatch")]
    BadHeaderChecksum,
    /// The object table signature is invalid.
    #[error("invalid object table signature: {0:#x}")]
    BadObjectTableSignature(u32),
    /// The requested key was not found in the file.
    #[error("key not found: {0}")]
    KeyNotFound(String),
    /// The key exists but has an unexpected type.
    #[error("unexpected key type: expected {expected}, got {actual:?}")]
    WrongKeyType {
        /// The expected type name.
        expected: &'static str,
        /// The actual type.
        actual: ValueType,
    },
    /// A key table has an invalid signature.
    #[error("invalid key table signature: {0:#x}")]
    BadKeyTableSignature(u16),
}

/// The type of a value in the key-value store.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ValueType {
    /// Signed 64-bit integer.
    Int,
    /// Unsigned 64-bit integer.
    UInt,
    /// UTF-16LE string.
    String,
    /// Raw byte array.
    Array,
    /// Boolean.
    Bool,
}

/// A read-only view of a HyperV Storage file.
pub struct HvsFileReader<R: Read + Seek> {
    reader: R,
    _alignment: u64,
    /// Key entries indexed by full path, ordered for deterministic enumeration.
    keys: BTreeMap<String, KeyEntry>,
}

/// A parsed key entry.
#[derive(Debug)]
struct KeyEntry {
    value_type: ValueType,
    is_file_object: bool,
    /// Inline data bytes (for non-file-object entries).
    data: Vec<u8>,
    /// For file object references.
    file_object_offset: u64,
    file_object_size: u32,
}

impl<R: Read + Seek> HvsFileReader<R> {
    /// Opens a HyperV Storage file for reading.
    pub fn open(mut reader: R) -> Result<Self, ReadError> {
        // Read both header copies and pick the one with higher sequence
        let header = Self::read_best_header(&mut reader)?;
        let alignment = header.data_alignment_in_bytes as u64;

        // Read object table at offset 8192
        let object_table_offset = 2 * MIN_DATA_ALIGNMENT as u64;
        reader.seek(SeekFrom::Start(object_table_offset))?;

        let mut obj_header = ObjectTableHeader::new_zeroed();
        reader.read_exact(obj_header.as_mut_bytes())?;

        if obj_header.signature != OBJECT_TABLE_SIGNATURE {
            return Err(ReadError::BadObjectTableSignature(obj_header.signature));
        }

        // Read object table entries
        let mut entries = Vec::with_capacity(obj_header.entries_count as usize);
        for _ in 0..obj_header.entries_count {
            let mut entry = ObjectTableEntry::new_zeroed();
            reader.read_exact(entry.as_mut_bytes())?;
            entries.push(entry);
        }

        // Follow chain to additional object tables
        let mut all_entries = entries.clone();
        if let Some(last) = entries.last() {
            if last.object_type == ObjectType::OBJECT_TABLE {
                let chain_offset = last.file_offset_in_bytes;
                reader.seek(SeekFrom::Start(chain_offset))?;
                let mut chain_header = ObjectTableHeader::new_zeroed();
                reader.read_exact(chain_header.as_mut_bytes())?;
                if chain_header.signature == OBJECT_TABLE_SIGNATURE {
                    for _ in 0..chain_header.entries_count {
                        let mut entry = ObjectTableEntry::new_zeroed();
                        reader.read_exact(entry.as_mut_bytes())?;
                        all_entries.push(entry);
                    }
                }
            }
        }

        // Read all key tables
        let mut key_table_data: Vec<Vec<u8>> = Vec::new();
        for entry in &all_entries {
            if entry.object_type != ObjectType::KEY_TABLE {
                continue;
            }
            reader.seek(SeekFrom::Start(entry.file_offset_in_bytes))?;
            let mut data = vec![0u8; entry.size_in_bytes as usize];
            reader.read_exact(&mut data)?;
            key_table_data.push(data);
        }

        // Parse key entries from all key tables, building a path tree
        let mut keys = BTreeMap::new();

        // node_path_map: (table_index, offset) -> path
        let mut node_path_map: HashMap<(u16, u32), String> = HashMap::new();
        // Root node is virtual at sentinel (0, 0)
        let key_table_header_size = size_of::<KeyTableHeader>();
        node_path_map.insert((0, 0), String::new());

        for (table_idx, table_data) in key_table_data.iter().enumerate() {
            if table_data.len() < key_table_header_size {
                continue;
            }

            // Validate key table header
            let kt_header = KeyTableHeader::read_from_prefix(&table_data[..key_table_header_size])
                .map(|(h, _)| h)
                .ok();

            if let Some(ref h) = kt_header {
                if h.signature != KEY_TABLE_SIGNATURE {
                    continue;
                }
            }

            let mut pos = key_table_header_size;
            let entry_header_size = size_of::<KeyTableEntryHeader>();

            while pos + entry_header_size <= table_data.len() {
                let entry_header = match KeyTableEntryHeader::read_from_prefix(&table_data[pos..]) {
                    Ok((h, _)) => h,
                    Err(_) => break,
                };

                let total_size = entry_header.size_in_bytes as usize;
                if total_size == 0 || pos + total_size > table_data.len() {
                    break;
                }

                let name_start = pos + entry_header_size;
                let name_len = entry_header.name_size_in_symbols as usize;
                let data_start = name_start + name_len;
                let data_end = pos + total_size;

                if data_start > table_data.len() || data_end > table_data.len() {
                    break;
                }

                let name = if name_len > 0 {
                    let name_bytes = &table_data[name_start..name_start + name_len];
                    // Strip trailing NUL
                    let name_str = if name_bytes.last() == Some(&0) {
                        &name_bytes[..name_bytes.len() - 1]
                    } else {
                        name_bytes
                    };
                    String::from_utf8_lossy(name_str).to_string()
                } else {
                    String::new()
                };

                let data_bytes = table_data[data_start..data_end].to_vec();

                // Determine the full path
                let parent_key = (entry_header.parent_node_table, entry_header.parent_node_offset);
                let parent_path = node_path_map.get(&parent_key).cloned().unwrap_or_default();

                let full_path = if name.is_empty() && parent_path.is_empty() {
                    String::new() // root
                } else if parent_path.is_empty() {
                    format!("/{name}")
                } else {
                    format!("{parent_path}/{name}")
                };

                if entry_header.key_type == KeyType::NODE {
                    // Use the actual table index from the header, not vector index
                    let actual_table_idx = kt_header.as_ref().map(|h| h.table_index).unwrap_or(table_idx as u16);
                    let current_key = (actual_table_idx, pos as u32);
                    node_path_map.insert(current_key, full_path.clone());
                } else if entry_header.key_type != KeyType::FREE {
                    let value_type = match entry_header.key_type {
                        KeyType::INT => ValueType::Int,
                        KeyType::UINT => ValueType::UInt,
                        KeyType::STRING => ValueType::String,
                        KeyType::ARRAY => ValueType::Array,
                        KeyType::BOOL => ValueType::Bool,
                        other => panic!(
                            "unrecognized key type {other:?} for key {full_path:?}"
                        ),
                    };
                    let is_file_object =
                        entry_header.flags & KEY_FLAG_POINTS_TO_FILE_OBJECT != 0;
                    let (fo_offset, fo_size) = if is_file_object {
                        if let Ok((fo_data, _)) = FileObjectData::read_from_prefix(&data_bytes) {
                            (fo_data.offset_in_bytes, fo_data.size_in_bytes)
                        } else {
                            (0, 0)
                        }
                    } else {
                        (0, 0)
                    };

                    keys.insert(full_path, KeyEntry {
                        value_type,
                        is_file_object,
                        data: data_bytes,
                        file_object_offset: fo_offset,
                        file_object_size: fo_size,
                    });
                }

                pos += total_size;
            }
        }

        Ok(Self {
            reader,
            _alignment: alignment,
            keys,
        })
    }

    fn read_best_header(reader: &mut R) -> Result<FileHeader, ReadError> {
        // Read header copy 0
        reader.seek(SeekFrom::Start(0))?;
        let mut h0 = FileHeader::new_zeroed();
        reader.read_exact(h0.as_mut_bytes())?;

        // Read header copy 1
        reader.seek(SeekFrom::Start(MIN_DATA_ALIGNMENT as u64))?;
        let mut h1 = FileHeader::new_zeroed();
        reader.read_exact(h1.as_mut_bytes())?;

        // Pick the valid one with higher sequence
        let valid0 = (h0.signature == HEADER_SIGNATURE && Self::verify_header_checksum(&h0))
            .then_some(h0);
        let valid1 = (h1.signature == HEADER_SIGNATURE && Self::verify_header_checksum(&h1))
            .then_some(h1);

        match (valid0, valid1) {
            (Some(a), Some(b)) => {
                if b.sequence > a.sequence { Ok(b) } else { Ok(a) }
            }
            (Some(a), None) => Ok(a),
            (None, Some(b)) => Ok(b),
            (None, None) => {
                if h0.signature == HEADER_SIGNATURE {
                    Err(ReadError::BadHeaderChecksum)
                } else {
                    Err(ReadError::BadHeaderSignature(h0.signature))
                }
            }
        }
    }

    fn verify_header_checksum(header: &FileHeader) -> bool {
        // Checksum field is at byte offset 4 in the packed header.
        header.checksum == struct_checksum(header.as_bytes(), offset_of!(FileHeader, checksum))
    }

    /// Returns all key paths in the file.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(|s| s.as_str())
    }

    /// Reads an integer value.
    pub fn read_int(&self, path: &str) -> Result<i64, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.value_type != ValueType::Int {
            return Err(ReadError::WrongKeyType { expected: "Int", actual: entry.value_type });
        }
        Ok(i64::from_le_bytes(entry.data[..8].try_into().unwrap_or_default()))
    }

    /// Reads an unsigned integer value.
    pub fn read_uint(&self, path: &str) -> Result<u64, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.value_type != ValueType::UInt {
            return Err(ReadError::WrongKeyType { expected: "UInt", actual: entry.value_type });
        }
        Ok(u64::from_le_bytes(entry.data[..8].try_into().unwrap_or_default()))
    }

    /// Reads a string value (UTF-16LE → String).
    pub fn read_string(&self, path: &str) -> Result<String, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.value_type != ValueType::String {
            return Err(ReadError::WrongKeyType { expected: "String", actual: entry.value_type });
        }
        // Data format: u32 size_in_bytes, then UTF-16LE data
        if entry.data.len() < 4 {
            return Ok(String::new());
        }
        let byte_len = u32::from_le_bytes(entry.data[..4].try_into().unwrap_or_default()) as usize;
        let utf16_bytes = &entry.data[4..4 + byte_len.min(entry.data.len() - 4)];
        let utf16: Vec<u16> = utf16_bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16_lossy(&utf16);
        Ok(s.trim_end_matches('\0').to_string())
    }

    /// Reads a boolean value.
    pub fn read_bool(&self, path: &str) -> Result<bool, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.value_type != ValueType::Bool {
            return Err(ReadError::WrongKeyType { expected: "Bool", actual: entry.value_type });
        }
        Ok(i32::from_le_bytes(entry.data[..4].try_into().unwrap_or_default()) != 0)
    }

    /// Reads an array value, transparently handling file objects.
    pub fn read_array(&mut self, path: &str) -> Result<Vec<u8>, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.value_type != ValueType::Array {
            return Err(ReadError::WrongKeyType { expected: "Array", actual: entry.value_type });
        }
        if entry.is_file_object {
            let offset = entry.file_object_offset;
            let size = entry.file_object_size as usize;
            self.reader.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0u8; size];
            self.reader.read_exact(&mut data)?;
            return Ok(data);
        }
        // Inline: u32 size + data
        if entry.data.len() < 4 {
            return Ok(Vec::new());
        }
        let size = u32::from_le_bytes(entry.data[..4].try_into().unwrap_or_default()) as usize;
        Ok(entry.data[4..4 + size.min(entry.data.len() - 4)].to_vec())
    }

    /// Checks if a key exists.
    pub fn contains_key(&self, path: &str) -> bool {
        self.keys.contains_key(path)
    }

    /// Returns the value type for a given path, or `None` if the key
    /// doesn't exist.
    pub fn value_type(&self, path: &str) -> Option<ValueType> {
        self.keys.get(path).map(|e| e.value_type)
    }
}
