// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writer for HyperV Storage files.
//!
//! Builds `.vmrs` / `.vmcx` / `.vsv` files from scratch in a single
//! sequential pass. Supports typed key values (Int, UInt, String, Array,
//! Bool, Node) and file objects for large binary blobs.

use crate::defs::*;
use crate::struct_checksum;
use core::mem::offset_of;
use core::mem::size_of;
use std::io::{self, Seek, SeekFrom, Write};
use zerocopy::IntoBytes;

/// Zero buffer for writing alignment padding without allocating.
const ZERO_PAGE: [u8; 4096] = [0u8; 4096];

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
    /// stored as file objects.
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
            self.writer.write_all(&ZERO_PAGE[..pad_len])?;
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

        let key_table_size = DEFAULT_KEY_TABLE_SIZE as usize;
        let key_table_header_size = size_of::<KeyTableHeader>();
        let entry_header_size = size_of::<KeyTableEntryHeader>();

        struct EntryData {
            header: KeyTableEntryHeader,
            name_bytes: Vec<u8>,
            data_bytes: Vec<u8>,
            path: String,
            is_node: bool,
        }

        impl EntryData {
            fn parent_path(&self) -> &str {
                match self.path[1..].rfind('/') {
                    Some(pos) => &self.path[..pos + 1],
                    None => "",
                }
            }
        }

        fn make_node_entry(path: &str, entry_header_size: usize) -> EntryData {
            let name = path.rsplit('/').next().unwrap_or("");
            let mut name_bytes = name.as_bytes().to_vec();
            name_bytes.push(0);
            let data_bytes = NodeData {
                change_tracking_sequence: 0,
                next_insertion_sequence: 0, // filled in later
            }
            .as_bytes()
            .to_vec();
            let total_size = entry_header_size + name_bytes.len() + data_bytes.len();
            EntryData {
                header: KeyTableEntryHeader {
                    key_type: KeyType::NODE,
                    flags: 0,
                    size_in_bytes: total_size as u32,
                    parent_node_table: 0,
                    parent_node_offset: 0,
                    checksum: 0,
                    insertion_sequence: 0,
                    name_size_in_symbols: name_bytes.len() as u8,
                },
                name_bytes,
                data_bytes,
                path: path.to_string(),
                is_node: true,
            }
        }

        // Build all entries in a single pass over sorted pending keys.
        // Use a stack of ancestor paths to track position in the tree.
        // When the path diverges, pop to the common prefix and push new
        // node entries for new segments. Insertion sequences are assigned
        // as entries are created — children of the same parent are
        // contiguous, so a counter that resets on parent change suffices.
        let mut all_entries: Vec<EntryData> = Vec::new();
        let mut node_stack: Vec<String> = Vec::new();
        let mut current_parent = String::new();
        let mut ins_seq: u32 = 0;

        /// Assign the next insertion sequence, resetting if the parent changed.
        fn next_ins_seq(parent: &str, current_parent: &mut String, ins_seq: &mut u32) -> u32 {
            if parent != current_parent.as_str() {
                *current_parent = parent.to_string();
                *ins_seq = 0;
            }
            *ins_seq += 1;
            *ins_seq
        }

        for key in self.pending_keys {
            let trimmed = key.path.trim_start_matches('/').to_string();
            let segments: Vec<&str> = trimmed.split('/').collect();
            let ancestor_segments = &segments[..segments.len().saturating_sub(1)];

            // Find common prefix length with current node_stack.
            let common = node_stack
                .iter()
                .zip(ancestor_segments.iter())
                .take_while(|(stk, seg)| stk.rsplit('/').next().unwrap_or("") == **seg)
                .count();

            // Pop back to common prefix.
            node_stack.truncate(common);

            // Push new ancestor nodes, emitting node entries.
            for seg in &ancestor_segments[common..] {
                let node_path = if node_stack.is_empty() {
                    format!("/{seg}")
                } else {
                    format!("{}/{seg}", node_stack.last().unwrap())
                };
                let mut entry = make_node_entry(&node_path, entry_header_size);
                entry.header.insertion_sequence =
                    next_ins_seq(entry.parent_path(), &mut current_parent, &mut ins_seq);
                all_entries.push(entry);
                node_stack.push(node_path);
            }

            // Emit the leaf entry.
            let name = segments.last().unwrap_or(&"");
            let mut name_bytes = name.as_bytes().to_vec();
            name_bytes.push(0);

            let (key_type, flags, data_bytes) = if let Some(fo) = key.file_object {
                let fo_data = FileObjectData {
                    size_in_bytes: fo.size,
                    offset_in_bytes: fo.offset,
                };
                (KeyType::ARRAY, KEY_FLAG_POINTS_TO_FILE_OBJECT, fo_data.as_bytes().to_vec())
            } else {
                match key.value {
                    Value::Int(v) => (KeyType::INT, 0u8, v.to_le_bytes().to_vec()),
                    Value::UInt(v) => (KeyType::UINT, 0u8, v.to_le_bytes().to_vec()),
                    Value::Bool(v) => (KeyType::BOOL, 0u8, (v as i32).to_le_bytes().to_vec()),
                    Value::String(s) => {
                        let mut data = vec![0u8; 4];
                        for ch in s.encode_utf16().chain(core::iter::once(0)) {
                            data.extend_from_slice(&ch.to_le_bytes());
                        }
                        let byte_len = (data.len() - 4) as u32;
                        data[..4].copy_from_slice(&byte_len.to_le_bytes());
                        (KeyType::STRING, 0u8, data)
                    }
                    Value::Array(data) => {
                        let mut buf = (data.len() as u32).to_le_bytes().to_vec();
                        buf.extend_from_slice(&data);
                        (KeyType::ARRAY, 0u8, buf)
                    }
                }
            };

            let total_size = entry_header_size + name_bytes.len() + data_bytes.len();
            let parent_path_str = if node_stack.is_empty() {
                ""
            } else {
                node_stack.last().unwrap().as_str()
            };
            let leaf_ins_seq = next_ins_seq(parent_path_str, &mut current_parent, &mut ins_seq);

            all_entries.push(EntryData {
                header: KeyTableEntryHeader {
                    key_type,
                    flags,
                    size_in_bytes: total_size as u32,
                    parent_node_table: 0,
                    parent_node_offset: 0,
                    checksum: 0,
                    insertion_sequence: leaf_ins_seq,
                    name_size_in_symbols: name_bytes.len() as u8,
                },
                name_bytes,
                data_bytes,
                path: key.path,
                is_node: false,
            });
        }

        // Set NodeData.next_insertion_sequence for each node by counting
        // how many subsequent entries are its direct children.
        for i in 0..all_entries.len() {
            if !all_entries[i].is_node {
                continue;
            }
            let mut count = 0u32;
            let rest = &all_entries[i + 1..];
            for other in rest {
                if other.parent_path() == all_entries[i].path {
                    count += 1;
                } else if !other.path.starts_with(&all_entries[i].path) {
                    break;
                }
            }
            all_entries[i].data_bytes = NodeData {
                change_tracking_sequence: 0,
                next_insertion_sequence: count + 1,
            }
            .as_bytes()
            .to_vec();
        }

        // Layout entries across key tables. Use a stack to track the
        // current ancestor chain's (table_index, offset) for parent
        // pointer fixup — no map needed since nodes always precede
        // their children.
        let usable_per_table = key_table_size - key_table_header_size;
        let mut tables: Vec<Vec<u8>> = Vec::new();
        let mut current_table_buf = Vec::with_capacity(usable_per_table);
        let mut current_table_index: u16 = 1;
        // Stack of (path, table_index, offset). Root sentinel is (0, 0).
        let mut loc_stack: Vec<(String, u16, u32)> = Vec::new();

        for entry in &mut all_entries {
            let entry_total = entry.header.size_in_bytes as usize;

            let remaining_after = usable_per_table.saturating_sub(current_table_buf.len() + entry_total);
            let would_overflow = current_table_buf.len() + entry_total > usable_per_table;
            let would_leave_small_gap = remaining_after > 0 && remaining_after <= entry_header_size;

            if (would_overflow || would_leave_small_gap) && !current_table_buf.is_empty() {
                tables.push(current_table_buf);
                current_table_buf = Vec::with_capacity(usable_per_table);
                current_table_index += 1;
            }

            let offset_in_table = key_table_header_size + current_table_buf.len();

            // Pop the stack back to this entry's parent.
            let parent = entry.parent_path();
            while let Some(top) = loc_stack.last() {
                if parent.starts_with(&top.0) && (parent == top.0 || parent.len() == top.0.len()) {
                    break;
                }
                loc_stack.pop();
            }

            // Set parent pointer from the stack top (or root sentinel).
            let (pt, po) = loc_stack
                .last()
                .map(|(_, t, o)| (*t, *o))
                .unwrap_or((0, 0));
            entry.header.parent_node_table = pt;
            entry.header.parent_node_offset = po;

            // Push node location for children to reference.
            if entry.is_node {
                loc_stack.push((entry.path.clone(), current_table_index, offset_in_table as u32));
            }

            // Compute entry checksum using streaming CRC (no buffer allocation).
            {
                let header_bytes = entry.header.as_bytes();
                let cksum_off = offset_of!(KeyTableEntryHeader, checksum);
                let mut hasher = crc32fast::Hasher::new();
                hasher.update(&header_bytes[..cksum_off]);
                hasher.update(&[0u8; 4]);
                hasher.update(&header_bytes[cksum_off + 4..]);
                hasher.update(&entry.name_bytes);
                hasher.update(&entry.data_bytes);
                entry.header.checksum = hasher.finalize();
            }

            current_table_buf.extend_from_slice(entry.header.as_bytes());
            current_table_buf.extend_from_slice(&entry.name_bytes);
            current_table_buf.extend_from_slice(&entry.data_bytes);
        }
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

            let mut header = KeyTableHeader {
                signature: KEY_TABLE_SIGNATURE,
                table_index: (i + 1) as u16, // indices start at 1
                sequence: 1,
                checksum: 0,
            };
            header.checksum = struct_checksum(header.as_bytes(), offset_of!(KeyTableHeader, checksum));

            // Write header + entries + padding as a single aligned block.
            self.writer.seek(SeekFrom::Start(offset))?;
            self.writer.write_all(header.as_bytes())?;
            self.writer.write_all(table_data)?;
            let written = key_table_header_size + table_data.len();
            let pad = align_up(written as u64, alignment) as usize - written;
            if pad > 0 {
                self.writer.write_all(&ZERO_PAGE[..pad])?;
            }
        }

        // Build object entries: key tables + file objects.
        let mut obj_entries: Vec<ObjectTableEntry> = Vec::new();

        for &offset in &key_table_offsets {
            let mut entry = ObjectTableEntry {
                object_type: ObjectType::KEY_TABLE,
                entry_checksum: 0,
                file_offset_in_bytes: offset,
                size_in_bytes: alignment as u32,
                flags: OBJECT_ENTRY_FLAG_REQUIRED,
            };
            entry.entry_checksum = struct_checksum(entry.as_bytes(), offset_of!(ObjectTableEntry, entry_checksum));
            obj_entries.push(entry);
        }

        for fo in &self.object_entries {
            let mut entry = *fo;
            entry.entry_checksum = struct_checksum(entry.as_bytes(), offset_of!(ObjectTableEntry, entry_checksum));
            obj_entries.push(entry);
        }

        // Write object tables with chaining. The last entry in each table
        // is reserved as a chain slot pointing to the next table, or empty
        // if this is the final table.
        let entries_per_table = (alignment as usize - size_of::<ObjectTableHeader>()) / size_of::<ObjectTableEntry>();
        let usable_per_table = entries_per_table - 1; // last slot is chain

        // Checksummed empty entry for padding and chain termination.
        let mut empty_entry = ObjectTableEntry {
            object_type: ObjectType::EMPTY,
            entry_checksum: 0,
            file_offset_in_bytes: 0,
            size_in_bytes: 0,
            flags: 0,
        };
        empty_entry.entry_checksum = struct_checksum(empty_entry.as_bytes(), offset_of!(ObjectTableEntry, entry_checksum));

        let object_table_offset = 2 * MIN_DATA_ALIGNMENT as u64;
        let mut chunks: Vec<&[ObjectTableEntry]> = obj_entries.chunks(usable_per_table).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }

        // Determine where overflow tables go (after replay log).
        let replay_log_offset = align_up(
            key_table_base + tables.len() as u64 * alignment,
            alignment,
        );
        let replay_log_size = alignment;
        let mut overflow_base = replay_log_offset + replay_log_size;

        // Write each object table.
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let table_offset = if chunk_idx == 0 {
                object_table_offset
            } else {
                let off = overflow_base;
                overflow_base += alignment;
                off
            };

            let is_last = chunk_idx == chunks.len() - 1;

            let mut buf = Vec::with_capacity(alignment as usize);
            let header = ObjectTableHeader {
                signature: OBJECT_TABLE_SIGNATURE,
                entries_count: entries_per_table as u32,
            };
            buf.extend_from_slice(header.as_bytes());

            for entry in *chunk {
                buf.extend_from_slice(entry.as_bytes());
            }

            // Fill unused slots with empty entries.
            for _ in chunk.len()..usable_per_table {
                buf.extend_from_slice(empty_entry.as_bytes());
            }

            // Chain slot: point to next table or empty.
            if is_last {
                buf.extend_from_slice(empty_entry.as_bytes());
            } else {
                let next_offset = if chunk_idx + 1 == 1 {
                    // Second chunk goes after replay log
                    replay_log_offset + replay_log_size
                } else {
                    overflow_base
                };
                let mut chain = ObjectTableEntry {
                    object_type: ObjectType::OBJECT_TABLE,
                    entry_checksum: 0,
                    file_offset_in_bytes: next_offset,
                    size_in_bytes: alignment as u32,
                    flags: 0,
                };
                chain.entry_checksum = struct_checksum(chain.as_bytes(), offset_of!(ObjectTableEntry, entry_checksum));
                buf.extend_from_slice(chain.as_bytes());
            }

            buf.resize(alignment as usize, 0);
            self.writer.seek(SeekFrom::Start(table_offset))?;
            self.writer.write_all(&buf)?;
        }

        // Write an empty replay log.
        let replay_log_header_size = alignment as u32;

        let max_entries = (replay_log_header_size as usize - size_of::<ReplayLogHeader>())
            / size_of::<ReplayLogEntryHeader>();

        let mut header = ReplayLogHeader {
            signature: REPLAY_LOG_SIGNATURE,
            checksum: 0,
            current_entries_count: 0,
            reserved: 0,
            maximum_number_of_entries: max_entries as u32,
            change_tracking_enabled: 0,
            change_tracking_buffer_offset: 0,
            change_tracking_buffer_size: 0,
            change_tracking_buffer_used_size: 0,
        };
        header.checksum = struct_checksum(header.as_bytes(), offset_of!(ReplayLogHeader, checksum));

        self.writer.seek(SeekFrom::Start(replay_log_offset))?;
        self.writer.write_all(header.as_bytes())?;
        let pad = alignment as usize - size_of::<ReplayLogHeader>();
        if pad > 0 {
            self.writer.write_all(&ZERO_PAGE[..pad])?;
        }

        // Write file headers. The two copies must have different sequence
        // numbers — identical sequences are treated as corrupt.
        let mut page = [0u8; MIN_DATA_ALIGNMENT as usize];

        for (seq, offset) in [(1u16, 0u64), (0u16, MIN_DATA_ALIGNMENT as u64)] {
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
            header.checksum = struct_checksum(header.as_bytes(), offset_of!(FileHeader, checksum));
            page[..size_of::<FileHeader>()].copy_from_slice(header.as_bytes());
            page[size_of::<FileHeader>()..].fill(0);
            self.writer.seek(SeekFrom::Start(offset))?;
            self.writer.write_all(&page)?;
        }

        self.writer.flush()?;
        Ok(self.writer)
    }
}
