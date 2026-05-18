// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writer for HyperV Storage files.
//!
//! Builds `.vmrs` / `.vmcx` / `.vsv` files from scratch in a single
//! sequential pass. Supports typed key values (Int, UInt, String, Array,
//! Bool, Node) and file objects for large binary blobs.

use crate::defs::*;
use std::collections::HashMap;
use std::io::{self, Seek, SeekFrom, Write};
use zerocopy::IntoBytes;

/// Computes CRC-32 (ISO 3309) over a byte slice.
pub fn crc32(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Computes the checksum for a structure, with the checksum field zeroed.
fn struct_checksum(bytes: &mut [u8], checksum_offset: usize) -> u32 {
    let saved = [
        bytes[checksum_offset],
        bytes[checksum_offset + 1],
        bytes[checksum_offset + 2],
        bytes[checksum_offset + 3],
    ];
    bytes[checksum_offset..checksum_offset + 4].fill(0);
    let crc = crc32(bytes);
    bytes[checksum_offset..checksum_offset + 4].copy_from_slice(&saved);
    crc
}

/// Round `size` up to a multiple of `alignment`.
fn align_up(size: u64, alignment: u64) -> u64 {
    (size + alignment - 1) & !(alignment - 1)
}

/// A typed value to write to the key-value store.
#[derive(Clone, Debug)]
pub enum Value {
    Int(i64),
    UInt(u64),
    String(String),
    Array(Vec<u8>),
    Bool(bool),
}

/// Tracks a pending key entry to be written into the key table.
struct PendingKey {
    path: String,
    value: Value,
    /// If the value is stored as a file object, this is the object table
    /// entry index and the actual data size (filled in after writing).
    file_object: Option<FileObjectRef>,
}

struct FileObjectRef {
    offset: u64,
    size: u32,
}

/// Writer for HyperV Storage files.
///
/// Usage:
/// 1. Create with [`HvsFileWriter::new`]
/// 2. Add keys with [`add_int`], [`add_uint`], [`add_string`], [`add_array`],
///    [`add_bool`], or [`add_file_object`]
/// 3. Call [`finish`] to write the complete file
pub struct HvsFileWriter<W: Write + Seek> {
    writer: W,
    alignment: u64,
    data_end: u64,
    /// Object table entries (offset, size, type).
    object_entries: Vec<ObjectTableEntry>,
    /// Pending keys to write.
    pending_keys: Vec<PendingKey>,
    /// Node insertion sequence counters, keyed by parent path.
    insertion_sequences: HashMap<String, u32>,
}

impl<W: Write + Seek> HvsFileWriter<W> {
    /// Creates a new writer.
    ///
    /// Writes the two header copies and the initial (empty) object table.
    /// Keys and file objects are buffered until [`finish`] is called.
    pub fn new(mut writer: W) -> io::Result<Self> {
        let alignment = DEFAULT_DATA_ALIGNMENT as u64;
        // Object table starts at offset 8192
        let object_table_offset = 2 * MIN_DATA_ALIGNMENT as u64;
        let data_end = object_table_offset + alignment;

        // Reserve space for headers + object table (will be written in finish)
        writer.seek(SeekFrom::Start(data_end))?;

        Ok(Self {
            writer,
            alignment,
            data_end,
            object_entries: Vec::new(),
            pending_keys: Vec::new(),
            insertion_sequences: HashMap::new(),
        })
    }

    /// Adds an integer key.
    pub fn add_int(&mut self, path: &str, value: i64) {
        self.pending_keys.push(PendingKey {
            path: path.to_string(),
            value: Value::Int(value),
            file_object: None,
        });
    }

    /// Adds an unsigned integer key.
    pub fn add_uint(&mut self, path: &str, value: u64) {
        self.pending_keys.push(PendingKey {
            path: path.to_string(),
            value: Value::UInt(value),
            file_object: None,
        });
    }

    /// Adds a string key (stored as UTF-16LE).
    pub fn add_string(&mut self, path: &str, value: &str) {
        self.pending_keys.push(PendingKey {
            path: path.to_string(),
            value: Value::String(value.to_string()),
            file_object: None,
        });
    }

    /// Adds a boolean key.
    pub fn add_bool(&mut self, path: &str, value: bool) {
        self.pending_keys.push(PendingKey {
            path: path.to_string(),
            value: Value::Bool(value),
            file_object: None,
        });
    }

    /// Adds an array key with inline data (for small arrays).
    pub fn add_array(&mut self, path: &str, data: Vec<u8>) {
        self.pending_keys.push(PendingKey {
            path: path.to_string(),
            value: Value::Array(data),
            file_object: None,
        });
    }

    /// Writes a file object for a large binary blob and adds a key
    /// referencing it. The data is written immediately to the file at the
    /// current `data_end` position.
    pub fn add_file_object(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
        let offset = self.data_end;
        let actual_size = data.len() as u32;
        let aligned_size = align_up(data.len() as u64, self.alignment);

        // Write the raw data at data_end
        self.writer.seek(SeekFrom::Start(offset))?;
        self.writer.write_all(data)?;

        // Pad to alignment
        let pad_len = aligned_size as usize - data.len();
        if pad_len > 0 {
            self.writer.write_all(&vec![0u8; pad_len])?;
        }

        // Track the object table entry
        self.object_entries.push(ObjectTableEntry {
            object_type: ObjectType::FILE_OBJECT,
            entry_checksum: 0, // filled in during finish
            file_offset_in_bytes: offset,
            size_in_bytes: aligned_size as u32,
            flags: 0,
        });

        self.data_end = offset + aligned_size;

        self.pending_keys.push(PendingKey {
            path: path.to_string(),
            value: Value::Array(Vec::new()), // placeholder
            file_object: Some(FileObjectRef {
                offset,
                size: actual_size,
            }),
        });

        Ok(())
    }

    /// Finishes writing the file: builds key tables, object table, and headers.
    pub fn finish(mut self) -> io::Result<W> {
        let alignment = self.alignment;

        // Collect all unique node paths from the pending keys
        let mut node_paths: Vec<String> = Vec::new();
        node_paths.push(String::new()); // root node (empty path)
        for key in &self.pending_keys {
            let parts: Vec<&str> = key.path.trim_start_matches('/').split('/').collect();
            let mut current = String::new();
            for (i, part) in parts.iter().enumerate() {
                if i < parts.len() - 1 {
                    // This is a directory node
                    if current.is_empty() {
                        current = format!("/{part}");
                    } else {
                        current = format!("{current}/{part}");
                    }
                    if !node_paths.contains(&current) {
                        node_paths.push(current.clone());
                    }
                }
            }
        }

        // Build key table(s)
        let key_table_size = DEFAULT_KEY_TABLE_SIZE as usize;
        let key_table_header_size = size_of::<KeyTableHeader>();
        let entry_header_size = size_of::<KeyTableEntryHeader>();

        // We'll build all entries in memory, then split across key tables
        struct EntryData {
            header: KeyTableEntryHeader,
            name_bytes: Vec<u8>,
            data_bytes: Vec<u8>,
        }

        let mut all_entries: Vec<EntryData> = Vec::new();

        // Track node locations: path -> (table_index, offset_within_table)
        let mut node_locations: HashMap<String, (u16, u32)> = HashMap::new();

        // The root node is virtual — it lives in memory as m_RootNode, not
        // in any key table. Parent reference (0, 0) is the sentinel for
        // "child of root". Key table indices start at 1.
        node_locations.insert(String::new(), (0, 0));

        // Add node entries for intermediate directories
        for node_path in &node_paths[1..] {
            let name = node_path.rsplit('/').next().unwrap_or("");
            let mut name_bytes = name.as_bytes().to_vec();
            name_bytes.push(0); // NUL terminator

            let parent_path = if let Some(pos) = node_path[1..].rfind('/') {
                &node_path[..pos + 1]
            } else {
                "" // parent is root
            };

            let parent_loc = node_locations.get(parent_path).copied().unwrap_or((0, key_table_header_size as u32));
            let parent_ins_seq = self.insertion_sequences.entry(parent_path.to_string()).or_insert(0);
            let ins_seq = *parent_ins_seq;
            *parent_ins_seq += 1;

            let node_data = NodeData {
                change_tracking_sequence: 0,
                next_insertion_sequence: 0,
            };
            let data_bytes = node_data.as_bytes().to_vec();
            let total_size = entry_header_size + name_bytes.len() + data_bytes.len();

            all_entries.push(EntryData {
                header: KeyTableEntryHeader {
                    key_type: KeyType::NODE,
                    flags: 0,
                    size_in_bytes: total_size as u32,
                    parent_node_table: parent_loc.0,
                    parent_node_offset: parent_loc.1,
                    checksum: 0,
                    insertion_sequence: ins_seq,
                    name_size_in_symbols: name_bytes.len() as u8,
                },
                name_bytes,
                data_bytes,
            });

            // Compute where this node will land — we need to know offsets
            // We'll fix this up after we know the layout
        }

        // Add leaf key entries
        for key in &self.pending_keys {
            let parts: Vec<&str> = key.path.trim_start_matches('/').split('/').collect();
            let name = parts.last().unwrap_or(&"");
            let mut name_bytes = name.as_bytes().to_vec();
            name_bytes.push(0); // NUL terminator

            let parent_path = if parts.len() > 1 {
                let parent_parts = &parts[..parts.len() - 1];
                format!("/{}", parent_parts.join("/"))
            } else {
                String::new() // parent is root
            };

            let parent_loc = node_locations.get(&parent_path).copied().unwrap_or((0, key_table_header_size as u32));
            let parent_ins_seq = self.insertion_sequences.entry(parent_path.clone()).or_insert(0);
            let ins_seq = *parent_ins_seq;
            *parent_ins_seq += 1;

            let (key_type, flags, data_bytes) = if let Some(ref fo) = key.file_object {
                let fo_data = FileObjectData {
                    size_in_bytes: fo.size,
                    offset_in_bytes: fo.offset,
                };
                (KeyType::ARRAY, KEY_FLAG_POINTS_TO_FILE_OBJECT, fo_data.as_bytes().to_vec())
            } else {
                match &key.value {
                    Value::Int(v) => (KeyType::INT, 0u8, v.to_le_bytes().to_vec()),
                    Value::UInt(v) => (KeyType::UINT, 0u8, v.to_le_bytes().to_vec()),
                    Value::Bool(v) => (KeyType::BOOL, 0u8, (*v as i32).to_le_bytes().to_vec()),
                    Value::String(s) => {
                        // UTF-16LE with NUL terminator, length-prefixed
                        let utf16: Vec<u16> = s.encode_utf16().chain(core::iter::once(0)).collect();
                        let byte_len = utf16.len() * 2;
                        let mut data = (byte_len as u32).to_le_bytes().to_vec();
                        for ch in &utf16 {
                            data.extend_from_slice(&ch.to_le_bytes());
                        }
                        (KeyType::STRING, 0u8, data)
                    }
                    Value::Array(data) => {
                        // Length-prefixed
                        let mut buf = (data.len() as u32).to_le_bytes().to_vec();
                        buf.extend_from_slice(data);
                        (KeyType::ARRAY, 0u8, buf)
                    }
                }
            };

            let total_size = entry_header_size + name_bytes.len() + data_bytes.len();

            all_entries.push(EntryData {
                header: KeyTableEntryHeader {
                    key_type,
                    flags,
                    size_in_bytes: total_size as u32,
                    parent_node_table: parent_loc.0,
                    parent_node_offset: parent_loc.1,
                    checksum: 0,
                    insertion_sequence: ins_seq,
                    name_size_in_symbols: name_bytes.len() as u8,
                },
                name_bytes,
                data_bytes,
            });
        }

        // Now layout the entries across key tables, computing offsets
        // Also fix up node_locations as we go
        let usable_per_table = key_table_size - key_table_header_size;
        let mut tables: Vec<Vec<u8>> = Vec::new();
        let mut current_table_buf = Vec::with_capacity(usable_per_table);
        // Key table indices start at 1 (0 is reserved for the virtual root)
        let mut current_table_index: u16 = 1;

        for (i, entry) in all_entries.iter_mut().enumerate() {
            let entry_total = entry.header.size_in_bytes as usize;

            if current_table_buf.len() + entry_total > usable_per_table && !current_table_buf.is_empty() {
                // Start a new key table
                tables.push(current_table_buf);
                current_table_buf = Vec::with_capacity(usable_per_table);
                current_table_index += 1;
            }

            let offset_in_table = key_table_header_size + current_table_buf.len();

            // Fix up node locations for node entries
            if entry.header.key_type == KeyType::NODE {
                // Node entries correspond to node_paths[1..] (index 0 is root, not stored)
                // In all_entries, node entries come first (indices 0..node_paths.len()-1)
                let node_idx = i + 1; // +1 because root (index 0) is virtual
                if node_idx < node_paths.len() {
                    let path = &node_paths[node_idx];
                    node_locations.insert(path.clone(), (current_table_index, offset_in_table as u32));

                    // Fix parent reference
                    let parent_path = if let Some(pos) = path[1..].rfind('/') {
                        &path[..pos + 1]
                    } else {
                        "" // parent is root
                    };
                    if let Some(&(pt, po)) = node_locations.get(parent_path) {
                        entry.header.parent_node_table = pt;
                        entry.header.parent_node_offset = po;
                    }
                }
            }

            // Fix parent references for leaf entries
            let num_node_entries = node_paths.len() - 1; // root is virtual
            if i >= num_node_entries {
                let key_idx = i - num_node_entries;
                if key_idx < self.pending_keys.len() {
                    let key = &self.pending_keys[key_idx];
                    let parts: Vec<&str> = key.path.trim_start_matches('/').split('/').collect();
                    let parent_path = if parts.len() > 1 {
                        format!("/{}", parts[..parts.len() - 1].join("/"))
                    } else {
                        String::new()
                    };
                    if let Some(&(pt, po)) = node_locations.get(&parent_path) {
                        entry.header.parent_node_table = pt;
                        entry.header.parent_node_offset = po;
                    }
                }
            }

            // Compute checksum: header (with checksum zeroed) + name + data
            let mut checksum_buf = Vec::with_capacity(entry_total);
            let mut header_bytes = entry.header.as_bytes().to_vec();
            // Zero checksum field (offset 12 in the packed header:
            // Type(1) + Flags(1) + Size(4) + ParentNodeTable(2) + ParentNodeOffset(4) = 12)
            header_bytes[12..16].fill(0);
            checksum_buf.extend_from_slice(&header_bytes);
            checksum_buf.extend_from_slice(&entry.name_bytes);
            checksum_buf.extend_from_slice(&entry.data_bytes);
            entry.header.checksum = crc32(&checksum_buf);

            // Write entry bytes
            current_table_buf.extend_from_slice(entry.header.as_bytes());
            current_table_buf.extend_from_slice(&entry.name_bytes);
            current_table_buf.extend_from_slice(&entry.data_bytes);
        }
        // Push the last table
        if !current_table_buf.is_empty() {
            tables.push(current_table_buf);
        }

        // Update NodeData.next_insertion_sequence for each node
        // (We'd need to seek back into the table buffers — for now, the
        // insertion sequences in the node data are set to 0, which is
        // acceptable for read-only dump files.)

        // Now write everything to the file

        // Write key tables right after the data_end reserved for file objects
        let key_table_base = self.data_end;
        let mut key_table_offsets: Vec<u64> = Vec::new();

        for (i, table_data) in tables.iter().enumerate() {
            let offset = key_table_base + (i as u64) * alignment;
            key_table_offsets.push(offset);

            // Build the key table header
            let mut header = KeyTableHeader {
                signature: KEY_TABLE_SIGNATURE,
                table_index: (i + 1) as u16, // indices start at 1
                sequence: 1,
                checksum: 0,
            };
            let mut header_bytes = header.as_bytes().to_vec();
            header.checksum = struct_checksum(&mut header_bytes, 6); // checksum at offset 6

            self.writer.seek(SeekFrom::Start(offset))?;
            self.writer.write_all(header.as_bytes())?;
            self.writer.write_all(table_data)?;

            // Pad the key table to alignment
            let written = key_table_header_size + table_data.len();
            let pad = align_up(written as u64, alignment) as usize - written;
            if pad > 0 {
                self.writer.write_all(&vec![0u8; pad])?;
            }
        }

        let total_key_table_space = tables.len() as u64 * alignment;
        let _final_data_end = key_table_base + total_key_table_space;

        // Build object table
        // Entries: key tables + file objects + chain slot
        let num_key_tables = tables.len();
        let num_file_objects = self.object_entries.len();
        let total_entries = num_key_tables + num_file_objects + 1; // +1 for chain slot

        let mut obj_entries: Vec<ObjectTableEntry> = Vec::with_capacity(total_entries);

        // Key table entries
        for (i, &offset) in key_table_offsets.iter().enumerate() {
            let _ = i;
            let mut entry = ObjectTableEntry {
                object_type: ObjectType::KEY_TABLE,
                entry_checksum: 0,
                file_offset_in_bytes: offset,
                size_in_bytes: alignment as u32,
                flags: OBJECT_ENTRY_FLAG_REQUIRED,
            };
            let mut bytes = entry.as_bytes().to_vec();
            entry.entry_checksum = struct_checksum(&mut bytes, 1);
            obj_entries.push(entry);
        }

        // File object entries
        for fo in &self.object_entries {
            let mut entry = *fo;
            let mut bytes = entry.as_bytes().to_vec();
            entry.entry_checksum = struct_checksum(&mut bytes, 1);
            obj_entries.push(entry);
        }

        // Chain slot (empty — no more tables)
        let mut chain_entry = ObjectTableEntry {
            object_type: ObjectType::EMPTY,
            entry_checksum: 0,
            file_offset_in_bytes: 0,
            size_in_bytes: 0,
            flags: 0,
        };
        let mut chain_bytes = chain_entry.as_bytes().to_vec();
        chain_entry.entry_checksum = struct_checksum(&mut chain_bytes, 1);
        obj_entries.push(chain_entry);

        // Write object table at offset 8192.
        let object_table_offset = 2 * MIN_DATA_ALIGNMENT as u64;

        self.writer.seek(SeekFrom::Start(object_table_offset))?;

        let obj_header = ObjectTableHeader {
            signature: OBJECT_TABLE_SIGNATURE,
            entries_count: obj_entries.len() as u32,
        };
        self.writer.write_all(obj_header.as_bytes())?;
        for entry in &obj_entries {
            self.writer.write_all(entry.as_bytes())?;
        }

        // Pad remainder of block to alignment
        let obj_table_written = size_of::<ObjectTableHeader>() + obj_entries.len() * size_of::<ObjectTableEntry>();
        let pad = align_up(obj_table_written as u64, alignment) as usize - obj_table_written;
        if pad > 0 {
            self.writer.write_all(&vec![0u8; pad])?;
        }

        // Write file headers.
        // The two copies must have different sequence numbers — if both are
        // valid with the same sequence, HyperVStorage treats the file as
        // corrupt. Write copy 0 with sequence 1 (authoritative) and copy 1
        // with sequence 0 (stale).
        let make_header = |seq: u16| -> Vec<u8> {
            let header = FileHeader {
                signature: HEADER_SIGNATURE,
                checksum: 0,
                sequence: seq,
                format_version: FORMAT_VERSION_4_0,
                data_version: 0,
                flags: 0,
                data_alignment_in_bytes: alignment as u32,
                replay_log_offset_in_bytes: 0,
                replay_log_size_in_bytes: 0,
                replay_log_header_size_in_bytes: 0,
            };
            let mut header_bytes = header.as_bytes().to_vec();
            let checksum = struct_checksum(&mut header_bytes, 4);
            let mut final_header = header;
            final_header.checksum = checksum;

            let mut page = vec![0u8; MIN_DATA_ALIGNMENT as usize];
            page[..size_of::<FileHeader>()].copy_from_slice(final_header.as_bytes());
            page
        };

        // Write copy 0 (sequence 1, authoritative)
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(&make_header(1))?;

        // Write copy 1 (sequence 0, stale)
        self.writer.write_all(&make_header(0))?;

        self.writer.flush()?;
        Ok(self.writer)
    }
}

use core::mem::size_of;
