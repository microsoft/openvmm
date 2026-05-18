// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reader for HyperV Storage files.
//!
//! Opens existing `.vmrs` / `.vmcx` / `.vsv` files and provides access
//! to the key-value store. Read-only, current format version only.

use crate::defs::*;
use crate::writer::crc32;
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use zerocopy::FromBytes;

/// Error type for reader operations.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid header signature: {0:#x}")]
    BadHeaderSignature(u32),
    #[error("header checksum mismatch")]
    BadHeaderChecksum,
    #[error("invalid object table signature: {0:#x}")]
    BadObjectTableSignature(u32),
    #[error("key not found: {0}")]
    KeyNotFound(String),
    #[error("unexpected key type: expected {expected}, got {actual}")]
    WrongKeyType { expected: &'static str, actual: u8 },
    #[error("invalid key table signature: {0:#x}")]
    BadKeyTableSignature(u16),
}

/// A read-only view of a HyperV Storage file.
pub struct HvsFileReader<R: Read + Seek> {
    reader: R,
    _alignment: u64,
    /// Flattened key entries: full path -> KeyEntry
    keys: HashMap<String, KeyEntry>,
}

/// A parsed key entry.
#[derive(Clone, Debug)]
struct KeyEntry {
    key_type: KeyType,
    flags: u8,
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

        let mut obj_header_bytes = [0u8; size_of::<ObjectTableHeader>()];
        reader.read_exact(&mut obj_header_bytes)?;
        let obj_header = ObjectTableHeader::read_from_bytes(&obj_header_bytes)
            .map_err(|_| ReadError::BadObjectTableSignature(0))?;

        if obj_header.signature != OBJECT_TABLE_SIGNATURE {
            return Err(ReadError::BadObjectTableSignature(obj_header.signature));
        }

        // Read object table entries
        let mut entries = Vec::with_capacity(obj_header.entries_count as usize);
        for _ in 0..obj_header.entries_count {
            let mut entry_bytes = [0u8; 18]; // size_of::<ObjectTableEntry>()
            reader.read_exact(&mut entry_bytes)?;
            let entry = ObjectTableEntry::read_from_bytes(&entry_bytes)
                .map_err(|_| ReadError::BadObjectTableSignature(0))?;
            entries.push(entry);
        }

        // Follow chain to additional object tables
        let mut all_entries = entries.clone();
        if let Some(last) = entries.last() {
            if last.object_type == ObjectType::OBJECT_TABLE {
                // Follow the chain (simplified — one level only for now)
                let chain_offset = last.file_offset_in_bytes;
                reader.seek(SeekFrom::Start(chain_offset))?;
                let mut chain_header_bytes = [0u8; size_of::<ObjectTableHeader>()];
                reader.read_exact(&mut chain_header_bytes)?;
                if let Ok(chain_header) = ObjectTableHeader::read_from_bytes(&chain_header_bytes) {
                    if chain_header.signature == OBJECT_TABLE_SIGNATURE {
                        for _ in 0..chain_header.entries_count {
                            let mut entry_bytes = [0u8; 18];
                            reader.read_exact(&mut entry_bytes)?;
                            if let Ok(entry) = ObjectTableEntry::read_from_bytes(&entry_bytes) {
                                all_entries.push(entry);
                            }
                        }
                    }
                }
            }
        }

        // Read all key tables
        let key_tables: Vec<(u64, u32)> = all_entries
            .iter()
            .filter(|e| e.object_type == ObjectType::KEY_TABLE)
            .map(|e| (e.file_offset_in_bytes, e.size_in_bytes))
            .collect();

        let mut key_table_data: Vec<Vec<u8>> = Vec::new();
        for &(offset, size) in &key_tables {
            reader.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0u8; size as usize];
            reader.read_exact(&mut data)?;
            key_table_data.push(data);
        }

        // Parse key entries from all key tables, building a path tree
        let mut keys = HashMap::new();

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
                    let (fo_offset, fo_size) = if entry_header.flags & KEY_FLAG_POINTS_TO_FILE_OBJECT != 0 {
                        if let Ok((fo_data, _)) = FileObjectData::read_from_prefix(&data_bytes) {
                            (fo_data.offset_in_bytes, fo_data.size_in_bytes)
                        } else {
                            (0, 0)
                        }
                    } else {
                        (0, 0)
                    };

                    keys.insert(full_path, KeyEntry {
                        key_type: entry_header.key_type,
                        flags: entry_header.flags,
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
        let header_size = size_of::<FileHeader>();

        // Read header copy 0
        reader.seek(SeekFrom::Start(0))?;
        let mut buf0 = vec![0u8; header_size];
        reader.read_exact(&mut buf0)?;
        let h0 = FileHeader::read_from_prefix(&buf0).map(|(h, _)| h).ok();

        // Read header copy 1
        reader.seek(SeekFrom::Start(MIN_DATA_ALIGNMENT as u64))?;
        let mut buf1 = vec![0u8; header_size];
        reader.read_exact(&mut buf1)?;
        let h1 = FileHeader::read_from_prefix(&buf1).map(|(h, _)| h).ok();

        // Pick the valid one with higher sequence
        let valid0 = h0.filter(|h| h.signature == HEADER_SIGNATURE && Self::verify_header_checksum(&buf0));
        let valid1 = h1.filter(|h| h.signature == HEADER_SIGNATURE && Self::verify_header_checksum(&buf1));

        match (valid0, valid1) {
            (Some(a), Some(b)) => {
                if b.sequence > a.sequence { Ok(b) } else { Ok(a) }
            }
            (Some(a), None) => Ok(a),
            (None, Some(b)) => Ok(b),
            (None, None) => {
                if let Some(_h) = h0 {
                    Err(ReadError::BadHeaderChecksum)
                } else {
                    Err(ReadError::BadHeaderSignature(
                        u32::from_le_bytes(buf0[..4].try_into().unwrap_or_default()),
                    ))
                }
            }
        }
    }

    fn verify_header_checksum(buf: &[u8]) -> bool {
        let mut check_buf = buf.to_vec();
        // Checksum is at offset 4
        let expected = u32::from_le_bytes(check_buf[4..8].try_into().unwrap_or_default());
        check_buf[4..8].fill(0);
        crc32(&check_buf) == expected
    }

    /// Returns all key paths in the file.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(|s| s.as_str())
    }

    /// Reads an integer value.
    pub fn read_int(&self, path: &str) -> Result<i64, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.key_type != KeyType::INT {
            return Err(ReadError::WrongKeyType {
                expected: "Int",
                actual: entry.key_type.0,
            });
        }
        Ok(i64::from_le_bytes(entry.data[..8].try_into().unwrap_or_default()))
    }

    /// Reads an unsigned integer value.
    pub fn read_uint(&self, path: &str) -> Result<u64, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.key_type != KeyType::UINT {
            return Err(ReadError::WrongKeyType {
                expected: "UInt",
                actual: entry.key_type.0,
            });
        }
        Ok(u64::from_le_bytes(entry.data[..8].try_into().unwrap_or_default()))
    }

    /// Reads a string value (UTF-16LE → String).
    pub fn read_string(&self, path: &str) -> Result<String, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.key_type != KeyType::STRING {
            return Err(ReadError::WrongKeyType {
                expected: "String",
                actual: entry.key_type.0,
            });
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
        // Strip trailing NUL
        let s = String::from_utf16_lossy(&utf16);
        Ok(s.trim_end_matches('\0').to_string())
    }

    /// Reads a boolean value.
    pub fn read_bool(&self, path: &str) -> Result<bool, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.key_type != KeyType::BOOL {
            return Err(ReadError::WrongKeyType {
                expected: "Bool",
                actual: entry.key_type.0,
            });
        }
        Ok(i32::from_le_bytes(entry.data[..4].try_into().unwrap_or_default()) != 0)
    }

    /// Reads an array value (inline data without the length prefix).
    pub fn read_array(&self, path: &str) -> Result<Vec<u8>, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?;
        if entry.key_type != KeyType::ARRAY {
            return Err(ReadError::WrongKeyType {
                expected: "Array",
                actual: entry.key_type.0,
            });
        }
        if entry.flags & KEY_FLAG_POINTS_TO_FILE_OBJECT != 0 {
            // Read from file object
            return self.read_file_object_data(entry.file_object_offset, entry.file_object_size);
        }
        // Inline: u32 size + data
        if entry.data.len() < 4 {
            return Ok(Vec::new());
        }
        let size = u32::from_le_bytes(entry.data[..4].try_into().unwrap_or_default()) as usize;
        Ok(entry.data[4..4 + size.min(entry.data.len() - 4)].to_vec())
    }

    fn read_file_object_data(&self, _offset: u64, _size: u32) -> Result<Vec<u8>, ReadError> {
        // We need mutable access to the reader — use interior mutability
        // For now, this is a limitation: the caller must use read_file_object
        Err(ReadError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            "use read_file_object for file object data",
        )))
    }

    /// Reads a file object's raw data given its path.
    pub fn read_file_object(&mut self, path: &str) -> Result<Vec<u8>, ReadError> {
        let entry = self.keys.get(path).ok_or_else(|| ReadError::KeyNotFound(path.to_string()))?.clone();
        if entry.flags & KEY_FLAG_POINTS_TO_FILE_OBJECT == 0 {
            // Inline data
            if entry.data.len() < 4 {
                return Ok(Vec::new());
            }
            let size = u32::from_le_bytes(entry.data[..4].try_into().unwrap_or_default()) as usize;
            return Ok(entry.data[4..4 + size.min(entry.data.len() - 4)].to_vec());
        }

        self.reader.seek(SeekFrom::Start(entry.file_object_offset))?;
        let mut data = vec![0u8; entry.file_object_size as usize];
        self.reader.read_exact(&mut data)?;
        Ok(data)
    }

    /// Checks if a key exists.
    pub fn contains_key(&self, path: &str) -> bool {
        self.keys.contains_key(path)
    }
}

use core::mem::size_of;
