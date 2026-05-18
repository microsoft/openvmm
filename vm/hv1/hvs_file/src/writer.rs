// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writer for HyperV Storage files.
//!
//! Builds `.vmrs` / `.vmcx` / `.vsv` files from scratch in a single
//! sequential pass. Supports typed key values (Int, UInt, String, Array,
//! Bool, Node) and file objects for large binary blobs.

use crate::defs::*;
use crate::crc32;
use crate::struct_checksum;
use std::collections::HashMap;
use std::io::{self, Seek, SeekFrom, Write};
use zerocopy::IntoBytes;

/// Round `size` up to a multiple of `alignment`.
fn align_up(size: u64, alignment: u64) -> u64 {
    (size + alignment - 1) & !(alignment - 1)
}

/// A typed value to write to the key-value store.
#[derive(Clone, Debug)]
enum Value {
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
///    [`add_bool`], or [`add_array`]
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

    /// Adds a binary array key.
    ///
    /// Arrays of [`FILE_OBJECT_THRESHOLD`] bytes or larger are automatically
    /// stored as file objects, matching Hyper-V's `ShouldUseFileObject`.
    pub fn add_array(&mut self, path: &str, data: Vec<u8>) -> io::Result<()> {
        if data.len() >= FILE_OBJECT_THRESHOLD as usize {
            return self.add_file_object(path, &data);
        }
        self.pending_keys.push(PendingKey {
            path: path.to_string(),
            value: Value::Array(data),
            file_object: None,
        });
        Ok(())
    }

    /// Writes a file object for a large binary blob and adds a key
    /// referencing it. The data is written immediately to the file at the
    /// current `data_end` position.
    fn add_file_object(&mut self, path: &str, data: &[u8]) -> io::Result<()> {
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

        // Sort pending keys by path for deterministic key table layout.
        self.pending_keys.sort_by(|a, b| a.path.cmp(&b.path));

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
            // Insertion sequences must be 1-based. The DLL's AddChild treats
            // InsertionSequence == 0 as "uninitialized" and reassigns it,
            // which triggers DataChanged and causes Commit() to fail on
            // read-only files.
            let parent_ins_seq = self.insertion_sequences.entry(parent_path.to_string()).or_insert(1);
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
            let parent_ins_seq = self.insertion_sequences.entry(parent_path.clone()).or_insert(1);
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

        // Update NodeData.next_insertion_sequence for each node to match
        // the actual child count. The DLL's CompleteMapUpdate verifies
        // that each node's NextInsertionSequence >= max child
        // InsertionSequence + 1; if not, it updates the node data and
        // marks the key table dirty, which causes Commit() to fail on
        // read-only files.
        let num_node_entries = node_paths.len() - 1; // root is virtual
        for i in 0..num_node_entries {
            let node_path = &node_paths[i + 1];
            let next_ins = self.insertion_sequences.get(node_path.as_str()).copied().unwrap_or(0);
            let node_data = NodeData {
                change_tracking_sequence: 0,
                next_insertion_sequence: next_ins,
            };
            all_entries[i].data_bytes = node_data.as_bytes().to_vec();
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

            // Check if adding this entry would leave a gap too small for a
            // Free entry (< 21 bytes). The DLL's Verify requires that entries
            // exactly fill the table, so any gap must be >= entry_header_size.
            let remaining_after = usable_per_table.saturating_sub(current_table_buf.len() + entry_total);
            let would_overflow = current_table_buf.len() + entry_total > usable_per_table;
            let would_leave_small_gap = remaining_after > 0 && remaining_after <= entry_header_size;

            if (would_overflow || would_leave_small_gap) && !current_table_buf.is_empty() {
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

        // Pad each key table's entries to fill the full usable space.
        // The key table verifier requires that entries exactly fill
        // the data area (from sizeof(KeyTableHeader) to objectSizeInBytes).
        // Fill remaining space with a Free entry, or if remaining < 21
        // bytes (minimum Free entry), extend the last entry to absorb it.
        for table_data in &mut tables {
            if table_data.len() < usable_per_table {
                let remaining = usable_per_table - table_data.len();
                if remaining >= entry_header_size {
                    // Free entries have checksum = 0 (the CalculateChecksum
                    // method returns 0 for free entries — the CRC block is
                    // skipped entirely).
                    let free_header = KeyTableEntryHeader {
                        key_type: KeyType::FREE,
                        flags: 0,
                        size_in_bytes: remaining as u32,
                        parent_node_table: 0,
                        parent_node_offset: 0,
                        checksum: 0,
                        insertion_sequence: 0,
                        name_size_in_symbols: 0,
                    };

                    table_data.extend_from_slice(free_header.as_bytes());
                    table_data.resize(usable_per_table, 0);
                } else {
                    // Gap is too small for a Free entry. Extend the last
                    // entry's SizeInBytes to absorb the slack bytes.
                    eprintln!("DEBUG: absorbing {remaining}-byte gap into last entry");
                    let mut pos = 0;
                    let mut last_size_offset = 0;
                    while pos + entry_header_size <= table_data.len() {
                        let entry_size = u32::from_le_bytes(
                            table_data[pos + 2..pos + 6].try_into().unwrap(),
                        ) as usize;
                        if entry_size == 0 || pos + entry_size >= table_data.len() {
                            last_size_offset = pos + 2;
                            break;
                        }
                        last_size_offset = pos + 2;
                        pos += entry_size;
                    }
                    let old_size = u32::from_le_bytes(
                        table_data[last_size_offset..last_size_offset + 4]
                            .try_into()
                            .unwrap(),
                    );
                    let new_size = old_size + remaining as u32;
                    table_data[last_size_offset..last_size_offset + 4]
                        .copy_from_slice(&new_size.to_le_bytes());
                    table_data.resize(usable_per_table, 0);
                }
            }
        }

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
            header.checksum = struct_checksum(header.as_bytes(), 6);

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

        // Build object table.
        // Fill to full capacity so the DLL doesn't try to expand it
        // (expansion requires a write-back which fails on read-only files).
        let max_entries = (alignment as usize - size_of::<ObjectTableHeader>()) / size_of::<ObjectTableEntry>();

        let mut obj_entries: Vec<ObjectTableEntry> = Vec::with_capacity(max_entries);

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
            entry.entry_checksum = struct_checksum(entry.as_bytes(), 1);
            obj_entries.push(entry);
        }

        // File object entries
        for fo in &self.object_entries {
            let mut entry = *fo;
            entry.entry_checksum = struct_checksum(entry.as_bytes(), 1);
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
        chain_entry.entry_checksum = struct_checksum(chain_entry.as_bytes(), 1);
        obj_entries.push(chain_entry);

        // Fill remaining slots with empty entries (properly checksummed)
        while obj_entries.len() < max_entries {
            obj_entries.push(chain_entry); // reuse the checksummed empty entry
        }

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

        // Write an empty replay log at the current data end.
        // HyperVStorage expects a valid replay log region — InitializeForLoad
        // dereferences the header buffer and Verify checks MaximumNumberOfEntries > 0.
        let replay_log_offset = align_up(
            key_table_base + tables.len() as u64 * alignment,
            alignment,
        );
        let replay_log_header_size = alignment as u32;
        let replay_log_size = alignment;

        // Replay log header (packed struct, 34 bytes):
        //   Signature: u32             offset 0
        //   Checksum: u32              offset 4
        //   CurrentEntriesCount: u32   offset 8
        //   Reserved: u8              offset 12
        //   MaximumNumberOfEntries: u32 offset 13
        //   ChangeTrackingEnabled: u8  offset 17
        //   ChangeTrackingBufferOffset: u64 offset 18
        //   ChangeTrackingBufferSize: u32   offset 26
        //   ChangeTrackingBufferUsedSize: u32 offset 30
        // Total: 34 bytes
        let replay_header_struct_size = 34usize;
        let replay_entry_header_size = 28usize; // sizeof(ReplayLogEntryHeader)
        let max_entries = (replay_log_header_size as usize - replay_header_struct_size)
            / replay_entry_header_size;

        let mut replay_header = vec![0u8; alignment as usize];
        // Signature = 0x01110003
        replay_header[0..4].copy_from_slice(&0x01110003u32.to_le_bytes());
        // CurrentEntriesCount = 0 (already zero)
        // MaximumNumberOfEntries at offset 13
        replay_header[13..17].copy_from_slice(&(max_entries as u32).to_le_bytes());
        // Compute checksum over the 34-byte header struct with checksum field zeroed
        let checksum = {
            let mut buf = replay_header[..replay_header_struct_size].to_vec();
            buf[4..8].fill(0);
            crc32(&buf)
        };
        replay_header[4..8].copy_from_slice(&checksum.to_le_bytes());

        self.writer.seek(SeekFrom::Start(replay_log_offset))?;
        self.writer.write_all(&replay_header)?;

        // Write file headers.
        // The two copies must have different sequence numbers — if both are
        // valid with the same sequence, HyperVStorage treats the file as
        // corrupt. Write copy 0 with sequence 1 (authoritative) and copy 1
        // with sequence 0 (stale).
        let make_header = |seq: u16| -> Vec<u8> {
            let mut header = FileHeader {
                signature: HEADER_SIGNATURE,
                checksum: 0,
                sequence: seq,
                format_version: FORMAT_VERSION_4_0,
                data_version: 0,
                flags: 0,
                data_alignment_in_bytes: alignment as u32,
                replay_log_offset_in_bytes: replay_log_offset,
                replay_log_size_in_bytes: replay_log_size,
                replay_log_header_size_in_bytes: replay_log_header_size,
            };
            let checksum = struct_checksum(header.as_bytes(), 4);
            header.checksum = checksum;

            let mut page = vec![0u8; MIN_DATA_ALIGNMENT as usize];
            page[..size_of::<FileHeader>()].copy_from_slice(header.as_bytes());
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
