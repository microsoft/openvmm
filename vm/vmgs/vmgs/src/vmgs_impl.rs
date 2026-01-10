// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::error::Error;
use crate::logger::VmgsLogEvent;
use crate::logger::VmgsLogger;
use crate::storage::VmgsStorage;
#[cfg(with_encryption)]
use anyhow::Context;
#[cfg(with_encryption)]
use anyhow::anyhow;
use cvm_tracing::CVM_ALLOWED;
use disk_backend::Disk;
#[cfg(feature = "inspect")]
use inspect::Inspect;
#[cfg(feature = "inspect")]
use inspect_counters::Counter;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use vmgs_format::EncryptionAlgorithm;
use vmgs_format::FileAttribute;
use vmgs_format::FileId;
use vmgs_format::VMGS_BYTES_PER_BLOCK;
use vmgs_format::VMGS_FILE_TABLE_BLOCK_SIZE;
use vmgs_format::VMGS_MIN_FILE_BLOCK_OFFSET;
use vmgs_format::VMGS_SIGNATURE;
use vmgs_format::VMGS_VERSION_3_0;
use vmgs_format::VmgsAuthTag;
use vmgs_format::VmgsDatastoreKey;
use vmgs_format::VmgsEncryptionKey;
use vmgs_format::VmgsExtendedFileEntry;
use vmgs_format::VmgsExtendedFileTable;
use vmgs_format::VmgsFileEntry;
use vmgs_format::VmgsFileTable;
use vmgs_format::VmgsHeader;
use vmgs_format::VmgsMarkers;
use vmgs_format::VmgsNonce;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Operation types for provisioning telemetry.
#[derive(Debug)]
enum LogOpType {
    VmgsProvision,
}

/// Info about a specific VMGS file.
#[derive(Debug)]
#[cfg_attr(feature = "mesh", derive(mesh_protobuf::Protobuf))]
pub struct VmgsFileInfo {
    /// Number of bytes allocated in the file.
    pub allocated_bytes: u64,
    /// Number of valid bytes in the file.
    pub valid_bytes: u64,
    /// Whether this file is encrypted.
    pub encrypted: bool,
}

/// GSP types that can be used to encrypt a VMGS file.
#[derive(Debug, Clone, Copy)]
pub enum GspType {
    /// No GSP
    None,
    /// GSP by ID
    GspById,
    /// GSP key
    GspKey,
}

// Aggregates fully validated data from the FILE_TABLE and EXTENDED_FILE_TABLE
// control blocks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "inspect", derive(Inspect))]
struct ResolvedFileControlBlock {
    // FILE_TABLE data
    // ---------------
    block_offset: u32,
    #[cfg_attr(feature = "inspect", inspect(with = "|x| x.get()"))]
    allocated_blocks: NonZeroU32,
    valid_bytes: u64,

    nonce: VmgsNonce,
    authentication_tag: VmgsAuthTag,

    // EXTENDED_FILE_TABLE data
    // ---------------
    attributes: FileAttribute,
    encryption_key: VmgsDatastoreKey,
}

impl ResolvedFileControlBlock {
    fn new(block_offset: u32, block_count: u32, valid_bytes: usize, encrypt: bool) -> Self {
        let (attributes, encryption_key, nonce) = if encrypt {
            (
                FileAttribute::new()
                    .with_encrypted(true)
                    .with_authenticated(true),
                {
                    let mut encryption_key = VmgsDatastoreKey::new_zeroed();
                    getrandom::fill(&mut encryption_key).expect("rng failure");
                    encryption_key
                },
                generate_nonce(),
            )
        } else {
            (
                FileAttribute::new(),
                VmgsDatastoreKey::new_zeroed(),
                VmgsNonce::new_zeroed(),
            )
        };

        ResolvedFileControlBlock {
            block_offset,
            allocated_blocks: NonZeroU32::new(block_count).unwrap(),
            valid_bytes: valid_bytes as u64,

            nonce,
            authentication_tag: VmgsAuthTag::new_zeroed(),

            attributes,
            encryption_key,
        }
    }

    fn file_info(&self) -> VmgsFileInfo {
        VmgsFileInfo {
            allocated_bytes: block_count_to_byte_count(self.allocated_blocks.get()),
            valid_bytes: self.valid_bytes,
            encrypted: self.is_encrypted(),
        }
    }

    fn is_encrypted(&self) -> bool {
        self.attributes.encrypted() || self.attributes.authenticated()
    }

    fn fill_file_entry(&self, version: u32, file_entry: &mut VmgsFileEntry) {
        file_entry.offset = self.block_offset;
        file_entry.allocation_size = self.allocated_blocks.get();
        file_entry.valid_data_size = self.valid_bytes;

        if version >= VMGS_VERSION_3_0 {
            file_entry.nonce.copy_from_slice(&self.nonce);
            file_entry
                .authentication_tag
                .copy_from_slice(&self.authentication_tag);
            file_entry.attributes = self.attributes;
        }
    }

    fn fill_extended_file_entry(&self, extended_file_entry: &mut VmgsExtendedFileEntry) {
        extended_file_entry.attributes = self.attributes;
        extended_file_entry
            .encryption_key
            .copy_from_slice(&self.encryption_key);
    }

    fn from_file_entry(version: u32, file_entry: &VmgsFileEntry) -> Self {
        let (nonce, authentication_tag, attributes) = if version >= VMGS_VERSION_3_0 {
            (
                file_entry.nonce,
                file_entry.authentication_tag,
                file_entry.attributes,
            )
        } else {
            Default::default()
        };

        ResolvedFileControlBlock {
            block_offset: file_entry.offset,
            allocated_blocks: NonZeroU32::new(file_entry.allocation_size).unwrap(),
            valid_bytes: file_entry.valid_data_size,

            nonce,
            authentication_tag,

            attributes,
            encryption_key: VmgsDatastoreKey::new_zeroed(),
        }
    }

    #[cfg(with_encryption)]
    fn update_extended_data(&mut self, extended_file_entry: &VmgsExtendedFileEntry) {
        self.attributes = extended_file_entry.attributes;
        self.encryption_key = extended_file_entry.encryption_key;
    }
}

enum RefOrOwned<'a> {
    Ref(&'a [u8]),
    Owned(Vec<u8>),
}

impl<'a> RefOrOwned<'a> {
    fn len(&self) -> usize {
        match self {
            RefOrOwned::Ref(x) => x.len(),
            RefOrOwned::Owned(x) => x.len(),
        }
    }

    fn as_mut(&mut self) -> &mut [u8] {
        match self {
            RefOrOwned::Ref(_) => panic!("can only modify owned value"),
            RefOrOwned::Owned(x) => x,
        }
    }

    fn as_ref(&self) -> &[u8] {
        match self {
            RefOrOwned::Ref(x) => x,
            RefOrOwned::Owned(x) => x,
        }
    }
}

#[derive(PartialEq, Eq)]
enum ExtendedFileTableUpdate {
    OnlyIfOtherEncrypted,
    AlwaysEmptyUnencrypted,
    #[cfg(with_encryption)]
    AlwaysEncrypted,
}

/// Implementation of the VMGS file format, backed by a generic [`Disk`]
/// device.
#[cfg_attr(feature = "inspect", derive(Inspect))]
pub struct Vmgs {
    storage: VmgsStorage,

    #[cfg(feature = "inspect")]
    stats: vmgs_inspect::VmgsStats,

    active_header_index: usize,
    active_header_sequence_number: u32,
    version: u32,
    #[cfg_attr(feature = "inspect", inspect(with = "vmgs_inspect::fcbs"))]
    fcbs: HashMap<FileId, ResolvedFileControlBlock>,
    encryption_algorithm: EncryptionAlgorithm,
    #[allow(dead_code)]
    datastore_key_count: u8,
    active_datastore_key_index: Option<usize>,
    #[cfg_attr(feature = "inspect", inspect(iter_by_index))]
    datastore_keys: [VmgsDatastoreKey; 2],
    metadata_key: VmgsDatastoreKey,
    #[cfg_attr(feature = "inspect", inspect(iter_by_index))]
    encrypted_metadata_keys: [VmgsEncryptionKey; 2],
    reprovisioned: bool,
    provisioned_this_boot: bool,

    #[cfg_attr(feature = "inspect", inspect(skip))]
    logger: Option<Arc<dyn VmgsLogger>>,
}

#[cfg(feature = "inspect")]
mod vmgs_inspect {
    use super::*;

    #[derive(Default)]
    pub struct IoStat {
        pub attempt: Counter,
        pub resolved: Counter,
    }

    // explicit inspect implementation, since we want to massage the data's
    // presentation a bit
    impl Inspect for IoStat {
        fn inspect(&self, req: inspect::Request<'_>) {
            let mut resp = req.respond();
            resp.counter("ok", self.resolved.get())
                .counter("err", self.attempt.get() - self.resolved.get());
        }
    }

    #[derive(Inspect, Default)]
    pub struct VmgsStats {
        #[inspect(with = "stat_map")]
        pub read: HashMap<FileId, IoStat>,
        #[inspect(with = "stat_map")]
        pub write: HashMap<FileId, IoStat>,
    }

    pub(super) fn fcbs(fcbs: &HashMap<FileId, ResolvedFileControlBlock>) -> impl Inspect + '_ {
        inspect::adhoc(|req| {
            let mut res = req.respond();
            for (id, fcb) in fcbs.iter() {
                res.field(&format!("{}-{:?}", id.0, id), fcb);
            }
        })
    }

    pub fn stat_map(map: &HashMap<FileId, IoStat>) -> impl Inspect + '_ {
        inspect::iter_by_key(map).map_key(|x| format!("{:?}", x))
    }
}

impl Vmgs {
    /// Attempt to open the VMGS file, optionally formatting if it is
    /// empty or corrupted.
    pub async fn try_open(
        disk: Disk,
        logger: Option<Arc<dyn VmgsLogger>>,
        format_on_empty: bool,
        format_on_failure: bool,
    ) -> Result<Self, Error> {
        match Self::open(disk.clone(), logger.clone()).await {
            Ok(vmgs) => Ok(vmgs),
            Err(Error::EmptyFile) if format_on_empty => {
                tracing::info!(CVM_ALLOWED, "empty vmgs file, formatting");
                Self::format_new(disk, logger).await
            }
            Err(err) if format_on_failure => {
                tracing::warn!(CVM_ALLOWED, ?err, "vmgs initialization error, reformatting");
                Self::format_new(disk, logger).await
            }
            Err(err) => {
                let event_log_id = match err {
                    // The data store format is invalid or not supported.
                    Error::InvalidFormat(_) => VmgsLogEvent::InvalidFormat,
                    // The data store is corrupted.
                    Error::CorruptFormat(_) => VmgsLogEvent::CorruptFormat,
                    // All other errors
                    _ => VmgsLogEvent::InitFailed,
                };

                logger.log_event_fatal(event_log_id).await;
                Err(err)
            }
        }
    }

    /// Open the VMGS file.
    pub async fn open(disk: Disk, logger: Option<Arc<dyn VmgsLogger>>) -> Result<Self, Error> {
        tracing::debug!(CVM_ALLOWED, "opening VMGS datastore");
        let storage = VmgsStorage::new_validated(disk).map_err(Error::Initialization)?;
        Self::open_inner(storage, logger).await
    }

    /// Format and open a new VMGS file.
    pub async fn format_new(
        disk: Disk,
        logger: Option<Arc<dyn VmgsLogger>>,
    ) -> Result<Self, Error> {
        tracing::info!(
            CVM_ALLOWED,
            op_type = ?LogOpType::VmgsProvision,
            "formatting and initializing VMGS datastore"
        );
        let storage = VmgsStorage::new_validated(disk).map_err(Error::Initialization)?;
        Self::format_new_inner(storage, VMGS_VERSION_3_0, logger).await
    }

    /// Format and open a new VMGS file.
    pub async fn request_format(
        disk: Disk,
        logger: Option<Arc<dyn VmgsLogger>>,
    ) -> Result<Self, Error> {
        let mut storage = VmgsStorage::new_validated(disk).map_err(Error::Initialization)?;

        match Self::open_header(&mut storage).await {
            Ok((active_header, active_header_index)) if active_header.markers.reprovisioned() => {
                tracing::info!(CVM_ALLOWED, "reprovisioned marker found, skipping format");
                Self::finish_open(storage, active_header, active_header_index, logger).await
            }
            _ => {
                tracing::info!(CVM_ALLOWED, "formatting vmgs file on request");
                let mut vmgs = Vmgs::format_new_inner(storage, VMGS_VERSION_3_0, logger).await?;

                // set the reprovisioned marker to prevent the vmgs from
                // repeatedly being reset
                vmgs.set_reprovisioned(true).await?;

                Ok(vmgs)
            }
        }
    }

    async fn open_inner(
        mut storage: VmgsStorage,
        logger: Option<Arc<dyn VmgsLogger>>,
    ) -> Result<Self, Error> {
        let (active_header, active_header_index) = Self::open_header(&mut storage).await?;

        let mut vmgs =
            Self::finish_open(storage, active_header, active_header_index, logger).await?;

        // clear the reprovisioned marker after successfully opening the vmgs
        // without being requested to reprovision.
        vmgs.set_reprovisioned(false).await?;

        Ok(vmgs)
    }

    async fn open_header(storage: &mut VmgsStorage) -> Result<(VmgsHeader, usize), Error> {
        let (header_1, header_2) = read_headers_inner(storage).await.map_err(|(e, _)| e)?;

        let active_header_index =
            get_active_header(validate_header(&header_1), validate_header(&header_2))?;

        let active_header = if active_header_index == 0 {
            header_1
        } else {
            header_2
        };

        Ok((active_header, active_header_index))
    }

    async fn finish_open(
        storage: VmgsStorage,
        active_header: VmgsHeader,
        active_header_index: usize,
        logger: Option<Arc<dyn VmgsLogger>>,
    ) -> Result<Vmgs, Error> {
        let version = active_header.version;
        let (encryption_algorithm, encrypted_metadata_keys, datastore_key_count, reprovisioned) =
            if version >= VMGS_VERSION_3_0 {
                let encryption_algorithm =
                    if active_header.encryption_algorithm == EncryptionAlgorithm::AES_GCM {
                        EncryptionAlgorithm::AES_GCM
                    } else {
                        EncryptionAlgorithm::NONE
                    };
                let encrypted_metadata_keys = active_header.metadata_keys;

                let is_key_zero_empty = is_empty_key(&encrypted_metadata_keys[0].encryption_key);
                let is_key_one_empty = is_empty_key(&encrypted_metadata_keys[1].encryption_key);
                let datastore_key_count = {
                    if is_key_zero_empty && is_key_one_empty {
                        0
                    } else if !is_key_zero_empty && !is_key_one_empty {
                        encrypted_metadata_keys.len() as u8
                    } else {
                        1
                    }
                };
                (
                    encryption_algorithm,
                    active_header.metadata_keys,
                    datastore_key_count,
                    active_header.markers.reprovisioned(),
                )
            } else {
                (
                    EncryptionAlgorithm::NONE,
                    [VmgsEncryptionKey::new_zeroed(); 2],
                    0,
                    false,
                )
            };

        // Add the file table FCB so the rest can be populated later
        let mut fcbs = HashMap::new();
        fcbs.insert(
            FileId::FILE_TABLE,
            ResolvedFileControlBlock::new(
                active_header.file_table_offset,
                active_header.file_table_size,
                size_of::<VmgsFileTable>(),
                false,
            ),
        );

        let mut vmgs = Self {
            storage,

            active_header_index,
            active_header_sequence_number: active_header.sequence,
            version,
            fcbs,
            encryption_algorithm,
            datastore_key_count,
            active_datastore_key_index: None,
            datastore_keys: [VmgsDatastoreKey::new_zeroed(); 2],
            metadata_key: VmgsDatastoreKey::new_zeroed(),
            encrypted_metadata_keys,
            reprovisioned,
            provisioned_this_boot: false,

            #[cfg(feature = "inspect")]
            stats: Default::default(),

            logger,
        };

        vmgs.initialize_file_metadata().await?;

        Ok(vmgs)
    }

    /// Initializes cached file metadata from the specified header. (File control blocks)
    async fn initialize_file_metadata(&mut self) -> Result<(), Error> {
        let file_table_buffer = self.read_file_internal(FileId::FILE_TABLE, false).await?;
        let file_table = VmgsFileTable::ref_from_bytes(&file_table_buffer).unwrap();
        let file_entries = file_table.entries;

        for (file_id, file_entry) in file_entries.iter().enumerate() {
            let file_id = FileId(file_id as u32);

            // Check if the file is allocated.
            if file_entry.allocation_size == 0 {
                continue;
            };

            // Validate the file offset.
            if file_entry.offset < VMGS_MIN_FILE_BLOCK_OFFSET
                || file_entry.offset >= self.storage.block_capacity()
            {
                return Err(Error::CorruptFormat(format!(
                    "Invalid file offset {} for file_id {:?} \n{:?}",
                    file_entry.offset, file_id, file_entry
                )));
            }

            // The file must entirely fit in the available space.
            let file_allocation_end_block = file_entry.offset + file_entry.allocation_size;
            if file_allocation_end_block > self.storage.block_capacity() {
                return Err(Error::CorruptFormat(String::from(
                    "Invalid file allocation end block",
                )));
            }

            // Validate the valid data size.
            let file_allocation_size_bytes = block_count_to_byte_count(file_entry.allocation_size);
            if file_entry.valid_data_size > file_allocation_size_bytes {
                return Err(Error::CorruptFormat(String::from("Invalid data size")));
            }

            // Initialize the file control block for this file ID
            self.fcbs.insert(
                file_id,
                ResolvedFileControlBlock::from_file_entry(self.version, file_entry),
            );
        }

        Ok(())
    }

    fn new(storage: VmgsStorage, version: u32, logger: Option<Arc<dyn VmgsLogger>>) -> Vmgs {
        Self {
            storage,

            active_header_index: 1,
            active_header_sequence_number: 0,
            version,
            fcbs: HashMap::new(),
            encryption_algorithm: EncryptionAlgorithm::NONE,
            datastore_key_count: 0,
            active_datastore_key_index: None,
            datastore_keys: [VmgsDatastoreKey::new_zeroed(); 2],
            metadata_key: VmgsDatastoreKey::new_zeroed(),
            encrypted_metadata_keys: [VmgsEncryptionKey::new_zeroed(); 2],
            reprovisioned: false,
            provisioned_this_boot: true,

            #[cfg(feature = "inspect")]
            stats: Default::default(),

            logger,
        }
    }

    /// Formats the backing store with initial metadata, and sets active header.
    async fn format_new_inner(
        storage: VmgsStorage,
        version: u32,
        logger: Option<Arc<dyn VmgsLogger>>,
    ) -> Result<Vmgs, Error> {
        tracing::info!(CVM_ALLOWED, "Formatting new VMGS file.");

        let mut vmgs = Self::new(storage, version, logger);

        // zero out the active header, update header will populate the other one
        vmgs.write_header_internal(&VmgsHeader::new_zeroed(), vmgs.active_header_index)
            .await?;

        // write a blank file table
        vmgs.write_files_internal(
            BTreeMap::new(),
            ExtendedFileTableUpdate::AlwaysEmptyUnencrypted,
        )
        .await?;

        // write the active header
        vmgs.update_header().await?;

        // Flush the device to persist changes
        vmgs.storage.flush().await.map_err(Error::FlushDisk)?;

        Ok(vmgs)
    }

    /// Get allocated and valid bytes from File Control Block for file_id.
    ///
    /// When reading data from a file, the buffer must be at least `valid_bytes` long.
    pub fn get_file_info(&self, file_id: FileId) -> Result<VmgsFileInfo, Error> {
        Ok(self
            .fcbs
            .get(&file_id)
            .ok_or(Error::FileInfoNotAllocated)?
            .file_info())
    }

    /// maps out the used/unused space in the file and finds the smallest
    /// unused space to allocate new data.
    fn allocate_space<'a>(
        &self,
        files_to_allocate: BTreeMap<FileId, (RefOrOwned<'a>, bool)>,
    ) -> Result<BTreeMap<FileId, (ResolvedFileControlBlock, RefOrOwned<'a>)>, Error> {
        // populate the allocation list with any existing files
        let mut allocation_list = self
            .fcbs
            .iter()
            .map(|(_, fcb)| AllocationBlock {
                block_offset: fcb.block_offset,
                allocated_blocks: fcb.allocated_blocks.get(),
            })
            .collect();

        // allocate space for the new files
        files_to_allocate
            .into_iter()
            .map(|(file_id, (buf, encrypt))| {
                Ok((
                    file_id,
                    (
                        self.allocate_space_inner(&mut allocation_list, buf.len(), encrypt)?,
                        buf,
                    ),
                ))
            })
            .collect()
    }

    fn allocate_space_inner(
        &self,
        allocation_list: &mut Vec<AllocationBlock>,
        valid_bytes: usize,
        encrypt: bool,
    ) -> Result<ResolvedFileControlBlock, Error> {
        let mut block_count = (round_up_count(valid_bytes, VMGS_BYTES_PER_BLOCK)
            / VMGS_BYTES_PER_BLOCK as u64) as u32;
        // Always allocate at least one block, to allow for zero sized data buffers
        if block_count == 0 {
            block_count = 1;
        }
        if block_count as u64 > vmgs_format::VMGS_MAX_FILE_SIZE_BLOCKS {
            return Err(Error::WriteFileBlocks);
        }

        let block_offset =
            allocate_helper(allocation_list, block_count, self.storage.block_capacity())?;

        allocation_list.push(AllocationBlock {
            block_offset,
            allocated_blocks: block_count,
        });

        Ok(ResolvedFileControlBlock::new(
            block_offset,
            block_count,
            valid_bytes,
            encrypt,
        ))
    }

    /// Writes `buf` to a file_id, optionally encrypting or overwriting
    /// encrypted data with plaintext. Updates file tables as appropriate.
    async fn write_file_inner(
        &mut self,
        file_id: FileId,
        buf: &[u8],
        encrypt: bool,
        overwrite_encrypted: bool,
    ) -> Result<(), Error> {
        #[cfg(feature = "inspect")]
        self.stats
            .write
            .entry(file_id)
            .or_default()
            .attempt
            .increment();

        if matches!(file_id, FileId::FILE_TABLE | FileId::EXTENDED_FILE_TABLE) {
            return Err(Error::FileId);
        }
        if buf.len() > vmgs_format::VMGS_MAX_FILE_SIZE_BYTES as usize {
            return Err(Error::WriteFileLength);
        }
        if encrypt && !self.is_encrypted() {
            tracing::trace!(
                CVM_ALLOWED,
                "VMGS file not encrypted, performing plaintext write"
            );
        }

        let encrypt = encrypt && self.is_encrypted();
        let existing_encrypted = self
            .fcbs
            .get(&file_id)
            .is_some_and(|fcb| fcb.is_encrypted());

        if !encrypt && existing_encrypted {
            if overwrite_encrypted {
                tracing::warn!(
                    CVM_ALLOWED,
                    "overwriting encrypted file with plaintext data!"
                )
            } else {
                return Err(Error::OverwriteEncrypted);
            }
        }

        self.write_files_internal(
            [(file_id, (RefOrOwned::Ref(buf), encrypt))].into(),
            ExtendedFileTableUpdate::OnlyIfOtherEncrypted,
        )
        .await?;

        // Data must be hardened on persistent storage before the header is updated.
        self.storage.flush().await.map_err(Error::FlushDisk)?;

        // Update the metadata key
        if encrypt {
            self.encrypt_metadata_key(false)?;
        }

        // Update the header
        self.update_header().await?;

        #[cfg(feature = "inspect")]
        self.stats
            .write
            .entry(file_id)
            .or_default()
            .resolved
            .increment();

        Ok(())
    }

    /// Write a set of files and update the in-memory FCBs if all are successful
    async fn write_files_internal<'a>(
        &mut self,
        mut files: BTreeMap<FileId, (RefOrOwned<'a>, bool)>,
        extended_file_table_update: ExtendedFileTableUpdate,
    ) -> Result<(), Error> {
        // ensure the necessary file tables are in the allocation list
        files.insert(
            FileId::FILE_TABLE,
            (
                RefOrOwned::Owned(vec![0; size_of::<VmgsFileTable>()]),
                false,
            ),
        );
        if extended_file_table_update != ExtendedFileTableUpdate::OnlyIfOtherEncrypted
            || files.iter().any(|(_, (_, encrypt))| *encrypt)
        {
            files.insert(
                FileId::EXTENDED_FILE_TABLE,
                (
                    RefOrOwned::Owned(vec![0; size_of::<VmgsExtendedFileTable>()]),
                    extended_file_table_update != ExtendedFileTableUpdate::AlwaysEmptyUnencrypted,
                ),
            );
        }

        // allocate space for the files and encypt if necessary
        let mut files = self
            .allocate_space(files)?
            .into_iter()
            .map(Self::maybe_encrypt)
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        {
            let (temp_fcbs, mut temp_data): (BTreeMap<_, _>, BTreeMap<_, _>) = files
                .iter_mut()
                .map(|(file_id, (fcb, buf))| ((file_id, &*fcb), (file_id, buf)))
                .unzip();
            if let Some(buf) = temp_data.get_mut(&FileId::EXTENDED_FILE_TABLE) {
                self.fill_extended_file_table(
                    temp_fcbs,
                    VmgsExtendedFileTable::mut_from_bytes(buf.as_mut()).unwrap(),
                )?;
            }
        }

        if let Some((fcb, buf)) = files.get_mut(&FileId::EXTENDED_FILE_TABLE) {
            Self::encrypt(fcb, buf)?;
        }

        // extract the new key and generate the file tables
        let (temp_fcbs, mut temp_data): (BTreeMap<_, _>, BTreeMap<_, _>) = files
            .iter_mut()
            .map(|(file_id, (fcb, buf))| ((file_id, &*fcb), (file_id, buf)))
            .unzip();
        let new_metadata_key = temp_fcbs
            .get(&FileId::EXTENDED_FILE_TABLE)
            .map(|fcb: &&ResolvedFileControlBlock| fcb.encryption_key);
        let new_file_table =
            VmgsFileTable::mut_from_bytes(temp_data.get_mut(&FileId::FILE_TABLE).unwrap().as_mut())
                .unwrap();
        self.fill_file_table(temp_fcbs.clone(), new_file_table)?;

        // write the files
        for (_, (fcb, buf)) in files.iter_mut() {
            self.write_file_internal(fcb, buf.as_ref()).await?;
        }

        // update the cache if successful
        self.fcbs
            .extend(files.into_iter().map(|(file_id, (fcb, _))| (file_id, fcb)));

        if let Some(new_metadata_key) = new_metadata_key {
            self.metadata_key = new_metadata_key;
        }

        Ok(())
    }

    fn maybe_encrypt<'a>(
        file: (FileId, (ResolvedFileControlBlock, RefOrOwned<'a>)),
    ) -> Result<(FileId, (ResolvedFileControlBlock, RefOrOwned<'a>)), Error> {
        let (file_id, (mut fcb, mut buf)) = file;
        // encrypt the data if necessary
        if fcb.is_encrypted() && file_id != FileId::EXTENDED_FILE_TABLE {
            Self::encrypt(&mut fcb, &mut buf)?
        }
        Ok((file_id, (fcb, buf)))
    }

    fn encrypt<'a>(
        fcb: &mut ResolvedFileControlBlock,
        buf: &mut RefOrOwned<'a>,
    ) -> Result<(), Error> {
        #[cfg(not(with_encryption))]
        unreachable!("Encryption requires the encryption feature");
        #[cfg(with_encryption)]
        {
            let encrypted_data = crate::encrypt::vmgs_encrypt(
                &fcb.encryption_key,
                &fcb.nonce,
                buf.as_ref(),
                &mut fcb.authentication_tag,
            )?;

            *buf = RefOrOwned::Owned(encrypted_data);
        }
        Ok(())
    }

    /// Writes `buf` to the block offset specified in the file control block.
    /// Encrypts the data and returns the auth tag if applicable.
    async fn write_file_internal<'a>(
        &mut self,
        fcb: &mut ResolvedFileControlBlock,
        buf: &[u8],
    ) -> Result<(), Error> {
        if let Err(e) = self
            .storage
            .write_block(block_count_to_byte_count(fcb.block_offset), buf)
            .await
        {
            self.logger
                .log_event_fatal(VmgsLogEvent::AccessFailed)
                .await;

            return Err(Error::WriteDisk(e));
        }

        Ok(())
    }

    /// Copies current file metadata to a file table structure.
    fn fill_file_table<'a>(
        &'a self,
        temp_fcbs: impl IntoIterator<Item = (&'a FileId, &'a ResolvedFileControlBlock)>,
        new_file_table: &mut VmgsFileTable,
    ) -> Result<(), Error> {
        for (file_id, fcb) in self.fcbs.iter().chain(temp_fcbs.into_iter()) {
            fcb.fill_file_entry(self.version, &mut new_file_table.entries[*file_id]);
        }
        Ok(())
    }

    /// Copies current file metadata to an extended file table structure.
    fn fill_extended_file_table<'a>(
        &'a self,
        temp_fcbs: impl IntoIterator<Item = (&'a FileId, &'a ResolvedFileControlBlock)>,
        new_extended_file_table: &mut VmgsExtendedFileTable,
    ) -> Result<(), Error> {
        for (file_id, fcb) in self.fcbs.iter().chain(temp_fcbs.into_iter()) {
            fcb.fill_extended_file_entry(&mut new_extended_file_table.entries[*file_id]);
        }
        Ok(())
    }

    /// Update and write new header to storage device.
    async fn update_header(&mut self) -> Result<(), Error> {
        let mut new_header = self.prepare_new_header();
        self.update_header_inner(&mut new_header).await
    }

    /// Initializes a new VMGS header populated using the current in-memory state
    fn prepare_new_header(&self) -> VmgsHeader {
        let file_table_fcb = self.fcbs.get(&FileId::FILE_TABLE).unwrap();
        let mut header = VmgsHeader {
            signature: VMGS_SIGNATURE,
            version: self.version,
            header_size: size_of::<VmgsHeader>() as u32,
            file_table_offset: file_table_fcb.block_offset,
            file_table_size: file_table_fcb.allocated_blocks.get(),
            encryption_algorithm: self.encryption_algorithm,
            markers: VmgsMarkers::new().with_reprovisioned(self.reprovisioned),

            ..VmgsHeader::new_zeroed()
        };
        header
            .metadata_keys
            .copy_from_slice(&self.encrypted_metadata_keys);
        header
    }

    /// Calculate the sequence and checksum and write the new header to disk.
    async fn update_header_inner(&mut self, new_header: &mut VmgsHeader) -> Result<(), Error> {
        // Wrapping prevents integer overflow checks
        new_header.sequence = self.active_header_sequence_number.wrapping_add(1);
        new_header.checksum = 0;
        new_header.checksum = compute_crc32(new_header.as_bytes());

        let new_header_index = if self.active_header_index == 0 { 1 } else { 0 };

        self.write_header_internal(new_header, new_header_index)
            .await?;
        self.set_active_header(new_header_index, new_header.sequence);
        Ok(())
    }

    /// write the new header to disk.
    async fn write_header_internal(
        &mut self,
        header: &VmgsHeader,
        index: usize,
    ) -> Result<(), Error> {
        eprintln!(
            "write header seqnum {} table {}",
            header.sequence, header.file_table_offset
        );
        assert!(index < 2);
        self.storage
            .write_block(
                index as u64 * self.storage.aligned_header_size(),
                header.as_bytes(),
            )
            .await
            .map_err(Error::WriteDisk)?;
        Ok(())
    }

    /// Sets the active header index and sequence number.
    fn set_active_header(
        &mut self,
        active_header_index: usize,
        active_header_sequence_number: u32,
    ) {
        assert!(active_header_index < 2);
        self.active_header_index = active_header_index;
        self.active_header_sequence_number = active_header_sequence_number;
    }

    /// Reads the specified `file_id`, decrypting its contents.
    pub async fn read_file(&mut self, file_id: FileId) -> Result<Vec<u8>, Error> {
        self.read_file_inner(file_id, true).await
    }

    /// Reads the specified `file_id`, but does not decrypt the contents.
    pub async fn read_file_raw(&mut self, file_id: FileId) -> Result<Vec<u8>, Error> {
        self.read_file_inner(file_id, false).await
    }

    /// User-facing file read
    async fn read_file_inner(&mut self, file_id: FileId, decrypt: bool) -> Result<Vec<u8>, Error> {
        #[cfg(feature = "inspect")]
        self.stats
            .read
            .entry(file_id)
            .or_default()
            .attempt
            .increment();

        if matches!(file_id, FileId::FILE_TABLE | FileId::EXTENDED_FILE_TABLE) {
            return Err(Error::FileId);
        }

        let buf = self.read_file_internal(file_id, decrypt).await?;

        #[cfg(feature = "inspect")]
        self.stats
            .read
            .entry(file_id)
            .or_default()
            .resolved
            .increment();

        Ok(buf)
    }

    /// read a file_id, decrypting if requested and possible
    async fn read_file_internal(
        &mut self,
        file_id: FileId,
        decrypt: bool,
    ) -> Result<Vec<u8>, Error> {
        let fcb = self.fcbs.get(&file_id).ok_or(Error::FileInfoNotAllocated)?;

        // read the file
        let buf = {
            let mut buf = vec![0; fcb.valid_bytes as usize];

            if let Err(e) = self
                .storage
                .read_block(block_count_to_byte_count(fcb.block_offset), &mut buf)
                .await
            {
                self.logger
                    .log_event_fatal(VmgsLogEvent::AccessFailed)
                    .await;

                return Err(Error::ReadDisk(e));
            }

            buf
        };

        // decrypt if necessary
        if decrypt
            && self.version >= VMGS_VERSION_3_0
            && self.is_encrypted()
            && fcb.is_encrypted()
            && self.active_datastore_key_index.is_some()
        {
            #[cfg(not(with_encryption))]
            unreachable!("Encryption requires the encryption feature");
            #[cfg(with_encryption)]
            {
                // sanity check: encrypted data should never be all zeros. if we
                // find that it is all-zeroes, then that's indicative of some kind
                // of logic error / data corruption
                if buf.iter().all(|x| *x == 0) {
                    return Err(Error::InvalidFormat("encrypted data is all-zeros".into()));
                }

                let decrypted = match crate::encrypt::vmgs_decrypt(
                    &fcb.encryption_key,
                    &fcb.nonce,
                    &buf,
                    &fcb.authentication_tag,
                ) {
                    Err(e) => {
                        self.logger
                            .log_event_fatal(VmgsLogEvent::AccessFailed)
                            .await;

                        return Err(e.into());
                    }
                    Ok(b) => b,
                };

                if decrypted.len() as u64 != fcb.valid_bytes {
                    return Err(Error::Other(anyhow!(
                        "Decrypted bytes should be {}, got {}",
                        fcb.valid_bytes,
                        decrypted.len()
                    )));
                }

                Ok(decrypted)
            }
        } else if fcb.is_encrypted() && decrypt {
            return Err(Error::ReadEncrypted);
        } else {
            Ok(buf)
        }
    }

    /// Writes `buf` to a file_id without encrypting it.
    ///
    /// If the file is already encrypted, this will return a failure. Use
    /// [`Self::write_file_allow_overwrite_encrypted`] if you want to allow
    /// this.
    ///
    /// To write encrypted data, use `write_file_encrypted` instead.
    pub async fn write_file(&mut self, file_id: FileId, buf: &[u8]) -> Result<(), Error> {
        self.write_file_inner(file_id, buf, false, false).await
    }

    /// Writes `buf` to a file_id without encrypting it, allowing overrites of
    /// an already-encrypted file.
    pub async fn write_file_allow_overwrite_encrypted(
        &mut self,
        file_id: FileId,
        buf: &[u8],
    ) -> Result<(), Error> {
        self.write_file_inner(file_id, buf, false, true).await
    }

    /// Encrypts `buf` and writes the encrypted payload to a file_id if the VMGS file has encryption configured.
    /// If the VMGS doesn't have encryption configured, will do a plaintext write instead.
    #[cfg(with_encryption)]
    pub async fn write_file_encrypted(&mut self, file_id: FileId, buf: &[u8]) -> Result<(), Error> {
        self.write_file_inner(file_id, buf, true, true).await
    }

    /// Decrypts the extended file table by the encryption_key and
    /// updates the related metadata in memory.
    #[cfg(with_encryption)]
    pub async fn unlock_with_encryption_key(&mut self, encryption_key: &[u8]) -> Result<(), Error> {
        if self.version < VMGS_VERSION_3_0 {
            return Err(Error::Other(anyhow!(
                "unlock_with_encryption_key() not supported with VMGS version"
            )));
        }
        if self.encryption_algorithm == EncryptionAlgorithm::NONE {
            return Err(Error::Other(anyhow!(
                "unlock_with_encryption_key() not supported with None EncryptionAlgorithm"
            )));
        }

        // Iterate through two metadata keys and get the index of the valid key which can be successfully
        // decrypted by the encryption_key, as well as set the decrypted key as the VMGS's metadata key
        let mut valid_index = None;
        let mut errs = [None, None];

        for (i, key) in self.encrypted_metadata_keys.iter().enumerate() {
            let result = decrypt_metadata_key(
                encryption_key,
                &key.nonce,
                &key.encryption_key,
                &key.authentication_tag,
            );

            match result {
                Ok(metadata_key) => {
                    self.metadata_key.copy_from_slice(&metadata_key);
                    let eft = self.fcbs.get_mut(&FileId::EXTENDED_FILE_TABLE).unwrap();
                    eft.encryption_key.copy_from_slice(&metadata_key);
                    valid_index = Some(i);

                    break;
                }
                Err(err) => {
                    errs[i] = Some(err);
                }
            }
        }

        let valid_index = match valid_index {
            Some(idx) => idx,
            None => {
                tracing::error!(
                    CVM_ALLOWED,
                    error = &errs[0].take().unwrap() as &dyn std::error::Error,
                    "first index failed to decrypt",
                );
                tracing::error!(
                    CVM_ALLOWED,
                    error = &errs[1].take().unwrap() as &dyn std::error::Error,
                    "second index failed to decrypt",
                );
                return Err(Error::Other(anyhow::anyhow!(
                    "failed to use the root key provided to decrypt VMGS metadata key"
                )));
            }
        };

        self.datastore_keys[valid_index].copy_from_slice(encryption_key);
        self.active_datastore_key_index = Some(valid_index);

        // Read and decrypt the extended file table
        let extended_file_table_buffer = self
            .read_file_internal(FileId::EXTENDED_FILE_TABLE, true)
            .await
            .context("failed to decrypt extended file table")?;

        // Update the cached extended file table
        let extended_file_table =
            VmgsExtendedFileTable::ref_from_bytes(extended_file_table_buffer.as_bytes())
                .map_err(|_| anyhow!("Invalid decrypted extended file table"))?; // TODO: zerocopy: use result (https://github.com/microsoft/openvmm/issues/759)

        for (file_id, fcb) in self.fcbs.iter_mut() {
            fcb.update_extended_data(&extended_file_table.entries[*file_id]);
        }

        Ok(())
    }

    /// Associates a new root key with the data store and removes the old
    /// encryption key, if it exists. If two keys already exist, the
    /// inactive key is removed first.
    #[cfg(with_encryption)]
    pub async fn update_encryption_key(
        &mut self,
        encryption_key: &[u8],
        encryption_algorithm: EncryptionAlgorithm,
    ) -> Result<(), Error> {
        let old_index = self.active_datastore_key_index;

        match self
            .add_new_encryption_key(encryption_key, encryption_algorithm)
            .await
        {
            Ok(_) => {}
            Err(Error::DatastoreKeysFull) => {
                if let Some(old_index) = old_index {
                    let inactive_index = if old_index == 0 { 1 } else { 0 };
                    tracing::warn!(CVM_ALLOWED, inactive_index, "removing inactive key");
                    self.remove_encryption_key(inactive_index).await?;
                    tracing::trace!(CVM_ALLOWED, "attempting to add the key again");
                    self.add_new_encryption_key(encryption_key, encryption_algorithm)
                        .await?;
                } else {
                    return Err(Error::NoActiveDatastoreKey);
                }
            }
            Err(e) => return Err(e),
        };

        if let Some(old_index) = old_index {
            self.remove_encryption_key(old_index).await?;
        }

        Ok(())
    }

    /// Associates a new root key with the data store.
    #[cfg(with_encryption)]
    async fn add_new_encryption_key(
        &mut self,
        encryption_key: &[u8],
        encryption_algorithm: EncryptionAlgorithm,
    ) -> Result<(), Error> {
        if self.version < VMGS_VERSION_3_0 {
            return Err(Error::Other(anyhow!(
                "add_new_encryption_key() not supported with VMGS version"
            )));
        }
        if self.encryption_algorithm != EncryptionAlgorithm::NONE
            && self.active_datastore_key_index.is_none()
        {
            return Err(Error::Other(anyhow!(
                "add_new_encryption_key() invalid datastore key index"
            )));
        }
        if self.datastore_key_count == self.datastore_keys.len() as u8 {
            return Err(Error::DatastoreKeysFull);
        }
        if is_empty_key(encryption_key) {
            return Err(Error::Other(anyhow!("Trying to add empty encryption key")));
        }
        if encryption_algorithm == EncryptionAlgorithm::NONE {
            return Err(Error::Other(anyhow!(
                "Encryption not supported for VMGS file"
            )));
        }
        if self.encryption_algorithm != EncryptionAlgorithm::NONE
            && encryption_algorithm != self.encryption_algorithm
        {
            return Err(Error::Other(anyhow!(
                "Encryption algorithm provided to add_new_encryption_key does not match VMGS's encryption algorithm."
            )));
        }

        let write_extended_file_table = self.active_datastore_key_index.is_none();

        let new_key_index = self
            .active_datastore_key_index
            .map_or(0, |i| if i == 0 { 1 } else { 0 });
        self.encryption_algorithm = encryption_algorithm;
        self.datastore_keys[new_key_index].copy_from_slice(encryption_key);
        self.active_datastore_key_index = Some(new_key_index);
        self.datastore_key_count += 1;

        // Allocate and write the new file tables
        if write_extended_file_table {
            self.write_files_internal(BTreeMap::new(), ExtendedFileTableUpdate::AlwaysEncrypted)
                .await?;
        }

        self.encrypt_metadata_key(true)?;

        // Update the header on the storage device
        self.update_header().await?;

        Ok(())
    }

    /// Update the metadata key
    fn encrypt_metadata_key(&mut self, new_nonce: bool) -> Result<(), Error> {
        let current_index = self
            .active_datastore_key_index
            .expect("cannot update metadata key without datastore key");

        if new_nonce {
            self.encrypted_metadata_keys[current_index]
                .nonce
                .copy_from_slice(&generate_nonce());
        } else {
            increment_nonce(&mut self.encrypted_metadata_keys[current_index].nonce)?;
        }

        let mut metadata_key_auth_tag = VmgsAuthTag::new_zeroed();
        let encrypted_metadata_key = encrypt_metadata_key(
            &self.datastore_keys[current_index],
            &self.encrypted_metadata_keys[current_index].nonce,
            &self.metadata_key,
            &mut metadata_key_auth_tag,
        )?;

        self.encrypted_metadata_keys[current_index]
            .authentication_tag
            .copy_from_slice(&metadata_key_auth_tag);
        self.encrypted_metadata_keys[current_index]
            .encryption_key
            .copy_from_slice(&encrypted_metadata_key);

        Ok(())
    }

    /// Disassociates the root key at the specified index from the data store.
    #[cfg(with_encryption)]
    async fn remove_encryption_key(&mut self, key_index: usize) -> Result<(), Error> {
        if self.version < VMGS_VERSION_3_0 {
            return Err(Error::Other(anyhow!(
                "remove_encryption_key() not supported with VMGS version."
            )));
        }
        if self.encryption_algorithm != EncryptionAlgorithm::NONE
            && self.active_datastore_key_index.is_none()
        {
            return Err(Error::Other(anyhow!(
                "remove_encryption_key() invalid datastore key index or encryption algorithm."
            )));
        }
        if self.datastore_key_count != self.datastore_keys.len() as u8
            && self.active_datastore_key_index != Some(key_index)
        {
            return Err(Error::Other(anyhow!(
                "remove_encryption_key() invalid key_index"
            )));
        }

        // Remove the corresponding datastore_key
        self.datastore_keys[key_index].fill(0);

        // Remove the corresponding metadata_key
        self.encrypted_metadata_keys[key_index] = VmgsEncryptionKey::new_zeroed();

        // Update cached metadata
        if self.datastore_key_count == 1 {
            self.encryption_algorithm = EncryptionAlgorithm::NONE;
            self.datastore_key_count = 0;
            self.active_datastore_key_index = None;
        } else {
            self.datastore_key_count = 1;

            let new_active_datastore_key_index = if key_index == 0 { 1 } else { 0 };
            if is_empty_key(&self.datastore_keys[new_active_datastore_key_index]) {
                self.active_datastore_key_index = None;
            } else {
                self.active_datastore_key_index = Some(new_active_datastore_key_index);
            }
        }

        // Update the header on the storage device
        self.update_header().await?;

        Ok(())
    }

    /// Gets the encryption algorithm of the VMGS
    pub fn get_encryption_algorithm(&self) -> EncryptionAlgorithm {
        self.encryption_algorithm
    }

    /// Whether the VMGS file is encrypted
    pub fn is_encrypted(&self) -> bool {
        self.encryption_algorithm != EncryptionAlgorithm::NONE
    }

    /// Whether the VMGS file was provisioned during the most recent boot
    pub fn was_provisioned_this_boot(&self) -> bool {
        self.provisioned_this_boot
    }

    async fn set_reprovisioned(&mut self, value: bool) -> Result<(), Error> {
        if self.reprovisioned != value {
            tracing::info!(reprovisioned = value, "update vmgs marker");
            self.reprovisioned = value;
            self.update_header().await?;
        }
        Ok(())
    }
}

/// Additional test-only functions for use in other crates that reveal
/// implmentation details of the vmgs datastore encryption keys.
#[cfg(feature = "test_helpers")]
mod test_helpers {
    use super::*;

    impl Vmgs {
        /// Get the active datastore key index
        pub fn test_get_active_datastore_key_index(&self) -> Option<usize> {
            self.active_datastore_key_index
        }

        /// Associates a new root key with the data store.
        #[cfg(with_encryption)]
        pub async fn test_add_new_encryption_key(
            &mut self,
            encryption_key: &[u8],
            encryption_algorithm: EncryptionAlgorithm,
        ) -> Result<(), Error> {
            self.add_new_encryption_key(encryption_key, encryption_algorithm)
                .await
        }
    }
}

/// Attempt to read both headers and separately return any validation errors
pub async fn read_headers(
    disk: Disk,
) -> Result<(VmgsHeader, VmgsHeader), (Error, Option<(VmgsHeader, VmgsHeader)>)> {
    let mut storage = VmgsStorage::new(disk);
    match (storage.validate(), read_headers_inner(&mut storage).await) {
        (Ok(_), res) => res,
        (Err(e), res) => Err((Error::Initialization(e), res.ok())),
    }
}

async fn read_headers_inner(
    storage: &mut VmgsStorage,
) -> Result<(VmgsHeader, VmgsHeader), (Error, Option<(VmgsHeader, VmgsHeader)>)> {
    // first_two_blocks will contain enough bytes to read the first two headers
    let mut first_two_blocks = [0; (VMGS_BYTES_PER_BLOCK * 2) as usize];

    storage
        .read_block(0, &mut first_two_blocks)
        .await
        .map_err(|e| (Error::ReadDisk(e), None))?;

    let header_1 = VmgsHeader::read_from_prefix(&first_two_blocks).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    let header_2 =
        VmgsHeader::read_from_prefix(&first_two_blocks[storage.aligned_header_size() as usize..])
            .unwrap()
            .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    let headers = (header_1, header_2);

    if vmgs_is_v1(&first_two_blocks) {
        Err((Error::V1Format, Some(headers)))
    } else if vmgs_headers_empty(&headers.0, &headers.1) {
        Err((Error::EmptyFile, Some(headers)))
    } else {
        Ok(headers)
    }
}

fn vmgs_is_v1(first_two_blocks: &[u8; 2 * VMGS_BYTES_PER_BLOCK as usize]) -> bool {
    const EFI_SIGNATURE: &[u8] = b"EFI PART";
    const EFI_SIGNATURE_OFFSET: usize = 512;

    EFI_SIGNATURE
        == &first_two_blocks[EFI_SIGNATURE_OFFSET..EFI_SIGNATURE_OFFSET + EFI_SIGNATURE.len()]
}

fn vmgs_headers_empty(header_1: &VmgsHeader, header_2: &VmgsHeader) -> bool {
    let empty_header = VmgsHeader::new_zeroed();

    header_1.as_bytes() == empty_header.as_bytes() && header_2.as_bytes() == empty_header.as_bytes()
}

/// Determines which header to use given the results of checking the
/// validity of each of the headers.
pub fn get_active_header(
    header_1: Result<&VmgsHeader, Error>,
    header_2: Result<&VmgsHeader, Error>,
) -> Result<usize, Error> {
    let active_header_index =
        if let (Ok(header_1), Ok(header_2)) = (header_1.as_deref(), header_2.as_deref()) {
            // If both headers are valid, find the header with the larger sequence number.
            // The header with the most recent sequence number is considered
            // the current copy. To handle integer overflow, a header with sequence number 0
            // is considered the current copy if and only if the other header contains 0xFFFFFFFF.
            if header_1.sequence == header_2.sequence.wrapping_add(1) {
                0
            } else if header_2.sequence == header_1.sequence.wrapping_add(1) {
                1
            } else {
                return Err(Error::CorruptFormat(format!(
                    "Invalid header sequence numbers. Header 1: {}, Header 2: {}",
                    header_1.sequence, header_2.sequence
                )));
            }
        } else if header_1.is_ok() {
            0
        } else if header_2.is_ok() {
            1
        } else {
            return Err(Error::InvalidFormat(format!(
                "No valid header: Header 1: {} Header 2: {}",
                header_1.err().unwrap(),
                header_2.err().unwrap()
            )));
        };

    Ok(active_header_index)
}

/// Validate the contents of header match VMGS file type.
pub fn validate_header(header: &VmgsHeader) -> Result<&VmgsHeader, Error> {
    if header.signature != VMGS_SIGNATURE {
        return Err(Error::InvalidFormat(String::from(
            "Invalid header signature",
        )));
    }
    if header.version != VMGS_VERSION_3_0 {
        return Err(Error::InvalidFormat(String::from("Invalid header version")));
    }
    if header.header_size != size_of::<VmgsHeader>() as u32 {
        return Err(Error::InvalidFormat(String::from("Invalid header size")));
    }
    if header.file_table_offset < VMGS_MIN_FILE_BLOCK_OFFSET {
        return Err(Error::InvalidFormat(String::from(
            "Invalid file table offset",
        )));
    }
    if header.file_table_size != VMGS_FILE_TABLE_BLOCK_SIZE {
        return Err(Error::InvalidFormat(String::from(
            "Invalid file table size",
        )));
    }

    let stored_checksum = header.checksum;
    let mut zero_checksum_header = *header;
    zero_checksum_header.checksum = 0;
    let computed_checksum = compute_crc32(zero_checksum_header.as_bytes());
    if stored_checksum != computed_checksum {
        return Err(Error::CorruptFormat(String::from(
            "Invalid header checksum",
        )));
    }
    Ok(header)
}

/// Convert block count to byte count.
fn block_count_to_byte_count(block_count: u32) -> u64 {
    block_count as u64 * VMGS_BYTES_PER_BLOCK as u64
}

fn round_up_count(count: usize, pow2: u32) -> u64 {
    (count as u64 + pow2 as u64 - 1) & !(pow2 as u64 - 1)
}

/// Generates a nonce for the encryption. First 4 bytes are a random seed, and last 8 bytes are zero's.
fn generate_nonce() -> VmgsNonce {
    let mut nonce = VmgsNonce::new_zeroed();
    // Generate a 4-byte random seed for nonce
    getrandom::fill(&mut nonce[..vmgs_format::VMGS_NONCE_RANDOM_SEED_SIZE]).expect("rng failure");
    nonce
}

/// Increment Nonce by one.
fn increment_nonce(nonce: &mut VmgsNonce) -> Result<(), Error> {
    // Update the random seed of nonce
    getrandom::fill(&mut nonce[..vmgs_format::VMGS_NONCE_RANDOM_SEED_SIZE]).expect("rng failure");

    // Increment the counter of nonce by 1.
    for i in &mut nonce[vmgs_format::VMGS_NONCE_RANDOM_SEED_SIZE..] {
        *i = i.wrapping_add(1);

        if *i != 0 {
            break;
        }
    }

    Ok(())
}

/// Checks whether an encryption key is all zero's.
fn is_empty_key(encryption_key: &[u8]) -> bool {
    encryption_key.iter().all(|&x| x == 0)
}

/// Encrypts MetadataKey. Returns encrypted_metadata_key.
#[cfg_attr(not(with_encryption), expect(unused_variables))]
fn encrypt_metadata_key(
    encryption_key: &[u8],
    nonce: &[u8],
    metadata_key: &[u8],
    authentication_tag: &mut [u8],
) -> Result<Vec<u8>, Error> {
    #[cfg(not(with_encryption))]
    unreachable!("Encryption requires the encryption feature");
    #[cfg(with_encryption)]
    {
        let encrypted_metadata_key =
            crate::encrypt::vmgs_encrypt(encryption_key, nonce, metadata_key, authentication_tag)?;

        if encrypted_metadata_key.len() != metadata_key.len() {
            return Err(Error::Other(anyhow!(format!(
                "encrypted metadata key length ({}) doesn't match metadata key length ({})",
                encrypted_metadata_key.len(),
                metadata_key.len()
            ))));
        }
        Ok(encrypted_metadata_key)
    }
}

/// Decrypts metadata_key. Returns decrypted_metadata_key.
#[cfg_attr(not(with_encryption), expect(unused_variables), expect(dead_code))]
fn decrypt_metadata_key(
    datastore_key: &[u8],
    nonce: &[u8],
    metadata_key: &[u8],
    authentication_tag: &[u8],
) -> Result<Vec<u8>, Error> {
    #[cfg(not(with_encryption))]
    unreachable!("Encryption requires the encryption feature");
    #[cfg(with_encryption)]
    {
        let decrypted_metadata_key =
            crate::encrypt::vmgs_decrypt(datastore_key, nonce, metadata_key, authentication_tag)?;
        if decrypted_metadata_key.len() != metadata_key.len() {
            return Err(Error::Other(anyhow!(format!(
                "decrypted metadata key length ({}) doesn't match metadata key length ({})",
                decrypted_metadata_key.len(),
                metadata_key.len()
            ))));
        }

        Ok(decrypted_metadata_key)
    }
}

/// Computes the cr32 checksum for a given byte stream.
fn compute_crc32(buf: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(buf);
    hasher.finalize()
}

struct AllocationBlock {
    block_offset: u32,
    allocated_blocks: u32,
}

/// maps out the used/unused space in the file and finds the smallest
/// unused space to allocate new data.
fn allocate_helper(
    allocation_list: &mut Vec<AllocationBlock>,
    block_count: u32,
    block_capacity: u32,
) -> Result<u32, Error> {
    // TODO: this will get removed when allocation_list is re-written
    // sort by block offset
    allocation_list.sort_by_key(|a| a.block_offset);

    let mut best_offset = 0;
    let mut best_free_count = 0;
    let mut last_allocation_end_offset = VMGS_MIN_FILE_BLOCK_OFFSET;
    let mut found = false;

    // find smallest set of blocks that will fit the data we're allocating
    for fcb in allocation_list.iter() {
        if fcb.block_offset < last_allocation_end_offset {
            return Err(Error::AllocateOffset);
        }
        let free_count = fcb.block_offset - last_allocation_end_offset;
        if free_count >= block_count && (best_free_count == 0 || free_count < best_free_count) {
            best_free_count = free_count;
            best_offset = last_allocation_end_offset;
            found = true;
        }
        last_allocation_end_offset = fcb.block_offset + fcb.allocated_blocks;
    }
    if last_allocation_end_offset < block_capacity {
        let free_count = block_capacity - last_allocation_end_offset;
        if free_count >= block_count && (best_free_count == 0 || free_count < best_free_count) {
            best_offset = last_allocation_end_offset;
            found = true;
        }
    }
    if !found {
        return Err(Error::InsufficientResources);
    }
    Ok(best_offset)
}

#[cfg(feature = "save_restore")]
#[expect(missing_docs)]
pub mod save_restore {
    use super::*;

    pub mod state {
        use mesh_protobuf::Protobuf;
        use std::num::NonZeroU32;

        pub type SavedVmgsNonce = [u8; 12];
        pub type SavedVmgsAuthTag = [u8; 16];
        pub type SavedVmgsDatastoreKey = [u8; 32];

        #[derive(Protobuf)]
        #[mesh(package = "vmgs")]
        pub struct SavedResolvedFileControlBlock {
            #[mesh(1)]
            pub block_offset: u32,
            #[mesh(2)]
            pub allocated_blocks: NonZeroU32,
            #[mesh(3)]
            pub valid_bytes: u64,
            #[mesh(4)]
            pub nonce: SavedVmgsNonce,
            #[mesh(5)]
            pub authentication_tag: SavedVmgsAuthTag,
            #[mesh(6)]
            pub attributes: u32,
            #[mesh(7)]
            pub encryption_key: SavedVmgsDatastoreKey,
        }

        #[derive(Protobuf)]
        #[mesh(package = "vmgs")]
        pub struct SavedVmgsEncryptionKey {
            #[mesh(1)]
            pub nonce: SavedVmgsNonce,
            #[mesh(2)]
            pub authentication_tag: SavedVmgsAuthTag,
            #[mesh(3)]
            pub encryption_key: SavedVmgsDatastoreKey,
        }

        #[derive(Protobuf)]
        #[mesh(package = "vmgs")]
        pub struct SavedVmgsState {
            #[mesh(1)]
            pub active_header_index: usize,
            #[mesh(2)]
            pub active_header_sequence_number: u32,
            #[mesh(3)]
            pub version: u32,
            #[mesh(4)]
            pub fcbs: Vec<(u32, SavedResolvedFileControlBlock)>,
            #[mesh(5)]
            pub encryption_algorithm: u16,
            #[mesh(6)]
            pub datastore_key_count: u8,
            #[mesh(7)]
            pub active_datastore_key_index: Option<usize>,
            #[mesh(8)]
            pub datastore_keys: [SavedVmgsDatastoreKey; 2],
            #[mesh(9)]
            pub metadata_key: SavedVmgsDatastoreKey,
            #[mesh(10)]
            pub encrypted_metadata_keys: [SavedVmgsEncryptionKey; 2],
            #[mesh(11)]
            pub reprovisioned: bool,
        }
    }

    impl Vmgs {
        /// Construct a [`Vmgs`] instance, re-using existing saved-state from an
        /// earlier instance.
        ///
        /// # Safety
        ///
        /// `open_from_saved` does NOT perform ANY validation on the provided
        /// `state`, and will blindly assume that it matches the underlying
        /// `storage` instance!
        ///
        /// Callers MUST ensure that the provided `state` matches the provided
        /// `storage`, and that no external entities have modified `storage` between
        /// the call to `save` and `open_from_saved`.
        ///
        /// Failing to do so may result in data corruption/loss, read/write
        /// failures, encryption errors, etc... (though, notably: it will _not_
        /// result in any memory-unsafety, hence why the function isn't marked
        /// `unsafe`).
        pub fn open_from_saved(
            disk: Disk,
            state: state::SavedVmgsState,
            logger: Option<Arc<dyn VmgsLogger>>,
        ) -> Self {
            let state::SavedVmgsState {
                active_header_index,
                active_header_sequence_number,
                version,
                fcbs,
                encryption_algorithm,
                datastore_key_count,
                active_datastore_key_index,
                datastore_keys,
                metadata_key,
                encrypted_metadata_keys,
                reprovisioned,
            } = state;

            Self {
                storage: VmgsStorage::new(disk),
                #[cfg(feature = "inspect")]
                stats: Default::default(),

                active_header_index,
                active_header_sequence_number,
                version,
                fcbs: fcbs
                    .into_iter()
                    .map(|(file_id, fcb)| {
                        let state::SavedResolvedFileControlBlock {
                            block_offset,
                            allocated_blocks,
                            valid_bytes,
                            nonce,
                            authentication_tag,
                            attributes,
                            encryption_key,
                        } = fcb;

                        (
                            FileId(file_id),
                            ResolvedFileControlBlock {
                                block_offset,
                                allocated_blocks,
                                valid_bytes,
                                nonce,
                                authentication_tag,
                                attributes: FileAttribute::from(attributes),
                                encryption_key,
                            },
                        )
                    })
                    .collect(),
                encryption_algorithm: EncryptionAlgorithm(encryption_algorithm),
                datastore_key_count,
                active_datastore_key_index,
                datastore_keys,
                metadata_key,
                encrypted_metadata_keys: encrypted_metadata_keys.map(|k| {
                    let state::SavedVmgsEncryptionKey {
                        nonce,
                        authentication_tag,
                        encryption_key,
                    } = k;

                    VmgsEncryptionKey {
                        nonce,
                        reserved: 0,
                        authentication_tag,
                        encryption_key,
                    }
                }),
                reprovisioned,
                provisioned_this_boot: false,
                logger,
            }
        }

        /// Save the in-memory Vmgs file metadata.
        ///
        /// This saved state can be used alongside `open_from_saved` to obtain a
        /// new `Vmgs` instance _without_ needing to invoke any IOs on the
        /// underlying storage.
        pub fn save(&self) -> state::SavedVmgsState {
            let Self {
                storage: _,

                #[cfg(feature = "inspect")]
                    stats: _,

                active_header_index,
                active_header_sequence_number,
                version,
                fcbs,
                encryption_algorithm,
                datastore_key_count,
                active_datastore_key_index,
                datastore_keys,
                metadata_key,
                encrypted_metadata_keys,
                logger: _,
                reprovisioned,
                provisioned_this_boot: _,
            } = self;

            state::SavedVmgsState {
                active_header_index: *active_header_index,
                active_header_sequence_number: *active_header_sequence_number,
                version: *version,
                fcbs: fcbs
                    .iter()
                    .map(|(file_id, fcb)| {
                        let ResolvedFileControlBlock {
                            block_offset,
                            allocated_blocks,
                            valid_bytes,
                            nonce,
                            authentication_tag,
                            attributes,
                            encryption_key,
                        } = fcb;

                        (
                            file_id.0,
                            state::SavedResolvedFileControlBlock {
                                block_offset: *block_offset,
                                allocated_blocks: *allocated_blocks,
                                valid_bytes: *valid_bytes,
                                nonce: *nonce,
                                authentication_tag: *authentication_tag,
                                attributes: (*attributes).into(),
                                encryption_key: *encryption_key,
                            },
                        )
                    })
                    .collect(),
                encryption_algorithm: encryption_algorithm.0,
                datastore_key_count: *datastore_key_count,
                active_datastore_key_index: *active_datastore_key_index,
                datastore_keys: *datastore_keys,
                metadata_key: *metadata_key,
                encrypted_metadata_keys: encrypted_metadata_keys.map(|k| {
                    let VmgsEncryptionKey {
                        nonce,
                        reserved: _,
                        authentication_tag,
                        encryption_key,
                    } = k;

                    state::SavedVmgsEncryptionKey {
                        nonce,
                        authentication_tag,
                        encryption_key,
                    }
                }),
                reprovisioned: *reprovisioned,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pal_async::async_test;
    #[cfg(with_encryption)]
    use parking_lot::Mutex;
    #[cfg(with_encryption)]
    use std::sync::Arc;
    #[cfg(with_encryption)]
    use vmgs_format::VMGS_ENCRYPTION_KEY_SIZE;

    const ONE_MEGA_BYTE: u64 = 1024 * 1024;

    #[cfg(with_encryption)]
    struct TestVmgsLogger {
        data: Arc<Mutex<String>>,
    }

    #[cfg(with_encryption)]
    #[async_trait::async_trait]
    impl VmgsLogger for TestVmgsLogger {
        async fn log_event_fatal(&self, _event: VmgsLogEvent) {
            let mut data = self.data.lock();
            *data = "test logger".to_string();
        }
    }

    fn new_test_file() -> Disk {
        disklayer_ram::ram_disk(4 * ONE_MEGA_BYTE, false).unwrap()
    }

    #[async_test]
    async fn empty_vmgs() {
        let disk = new_test_file();

        let result = Vmgs::open(disk, None).await;
        assert!(matches!(result, Err(Error::EmptyFile)));
    }

    #[async_test]
    async fn format_empty_vmgs() {
        let disk = new_test_file();
        let result = Vmgs::format_new(disk, None).await;
        assert!(result.is_ok());
    }

    #[async_test]
    async fn basic_read_write() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();
        assert_eq!(vmgs.active_header_index, 0);
        assert_eq!(vmgs.active_header_sequence_number, 1);
        assert_eq!(vmgs.version, VMGS_VERSION_3_0);

        // write
        let buf = b"hello world";
        vmgs.write_file(FileId::BIOS_NVRAM, buf).await.unwrap();

        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 2);

        // read
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();

        assert_eq!(buf, &*read_buf);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 2);
    }

    #[async_test]
    async fn basic_read_write_large() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();

        // write
        let buf: Vec<u8> = (0..).map(|x| x as u8).take(1024 * 4 + 1).collect();

        vmgs.write_file(FileId::BIOS_NVRAM, &buf).await.unwrap();

        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 2);

        // read
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();

        assert_eq!(buf, read_buf);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 2);

        // write
        let buf: Vec<u8> = (0..).map(|x| x as u8).take(1024 * 4 * 4 + 1).collect();

        vmgs.write_file(FileId::TPM_PPI, &buf).await.unwrap();

        assert_eq!(vmgs.active_header_index, 0);
        assert_eq!(vmgs.active_header_sequence_number, 3);

        // read
        let read_buf = vmgs.read_file(FileId::TPM_PPI).await.unwrap();

        assert_eq!(buf, read_buf);
        assert_eq!(vmgs.active_header_index, 0);
        assert_eq!(vmgs.active_header_sequence_number, 3);

        // write
        let buf: Vec<u8> = (0..).map(|x| x as u8).take(1024 * 4 * 4 * 4 + 1).collect();

        vmgs.write_file(FileId::GUEST_FIRMWARE, &buf).await.unwrap();

        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 4);

        // read
        let read_buf = vmgs.read_file(FileId::GUEST_FIRMWARE).await.unwrap();

        assert_eq!(buf, read_buf);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 4);
    }

    #[async_test]
    async fn open_existing_file() {
        let buf_1 = b"hello world";
        let buf_2 = b"short sentence";
        let buf_3 = b"funny joke";

        // Create VMGS file and write to different FileId's
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();

        vmgs.write_file(FileId::BIOS_NVRAM, buf_1).await.unwrap();

        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 2);
        assert_eq!(vmgs.fcbs[&FileId(0)].block_offset, 4);
        assert_eq!(vmgs.fcbs[&FileId(1)].block_offset, 5);

        vmgs.write_file(FileId::TPM_PPI, buf_2).await.unwrap();

        assert_eq!(vmgs.active_header_index, 0);
        assert_eq!(vmgs.active_header_sequence_number, 3);
        assert_eq!(vmgs.fcbs[&FileId(0)].block_offset, 2);
        assert_eq!(vmgs.fcbs[&FileId(1)].block_offset, 5);
        assert_eq!(vmgs.fcbs[&FileId(2)].block_offset, 6);

        vmgs.write_file(FileId::BIOS_NVRAM, buf_3).await.unwrap();

        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 4);
        assert_eq!(vmgs.fcbs[&FileId(0)].block_offset, 4);
        assert_eq!(vmgs.fcbs[&FileId(1)].block_offset, 7);
        assert_eq!(vmgs.fcbs[&FileId(2)].block_offset, 6);

        // Re-open VMGS file and read from the same FileId's
        drop(vmgs);

        let mut vmgs = Vmgs::open(disk, None).await.unwrap();

        assert_eq!(vmgs.fcbs[&FileId(0)].block_offset, 4);
        assert_eq!(vmgs.fcbs[&FileId(1)].block_offset, 7);
        assert_eq!(vmgs.fcbs[&FileId(2)].block_offset, 6);
        let read_buf_1 = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();

        assert_eq!(buf_3, &*read_buf_1);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 4);

        let read_buf_2 = vmgs.read_file(FileId::TPM_PPI).await.unwrap();

        assert_eq!(buf_2, &*read_buf_2);
        assert_eq!(vmgs.fcbs[&FileId(0)].block_offset, 4);
        assert_eq!(vmgs.fcbs[&FileId(1)].block_offset, 7);
        assert_eq!(vmgs.fcbs[&FileId(2)].block_offset, 6);
    }

    #[async_test]
    async fn multiple_read_write() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();

        let buf_1 = b"Data data data";
        let buf_2 = b"password";
        let buf_3 = b"other data data";

        vmgs.write_file(FileId::BIOS_NVRAM, buf_1).await.unwrap();
        let read_buf_1 = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf_1, &*read_buf_1);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 2);

        vmgs.write_file(FileId::TPM_PPI, buf_2).await.unwrap();
        let info = vmgs.get_file_info(FileId::TPM_PPI).unwrap();
        assert_eq!(info.valid_bytes as usize, buf_2.len());
        let read_buf_2 = vmgs.read_file(FileId::TPM_PPI).await.unwrap();
        assert_eq!(buf_2, &*read_buf_2);
        assert_eq!(vmgs.active_header_index, 0);
        assert_eq!(vmgs.active_header_sequence_number, 3);

        vmgs.write_file(FileId::BIOS_NVRAM, buf_3).await.unwrap();
        let read_buf_3 = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf_3, &*read_buf_3);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 4);

        vmgs.write_file(FileId::BIOS_NVRAM, buf_1).await.unwrap();
        let read_buf_1 = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf_1, &*read_buf_1);
        assert_eq!(vmgs.active_header_index, 0);
        assert_eq!(vmgs.active_header_sequence_number, 5);

        vmgs.write_file(FileId::TPM_PPI, buf_2).await.unwrap();
        let read_buf_2 = vmgs.read_file(FileId::TPM_PPI).await.unwrap();
        assert_eq!(buf_2, &*read_buf_2);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 6);

        vmgs.write_file(FileId::BIOS_NVRAM, buf_3).await.unwrap();
        let read_buf_3 = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf_3, &*read_buf_3);
        assert_eq!(vmgs.active_header_index, 0);
        assert_eq!(vmgs.active_header_sequence_number, 7);
    }

    #[async_test]
    async fn test_insufficient_resources() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();

        let buf: Vec<u8> = vec![1; ONE_MEGA_BYTE as usize * 5];
        let result = vmgs.write_file(FileId::BIOS_NVRAM, &buf).await;
        assert!(result.is_err());
        if let Err(e) = result {
            match e {
                Error::InsufficientResources => (),
                _ => panic!("Wrong error returned"),
            }
        } else {
            panic!("Should have returned Insufficient resources error");
        }
    }

    #[async_test]
    async fn test_empty_write() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();

        let buf: Vec<u8> = Vec::new();
        vmgs.write_file(FileId::BIOS_NVRAM, &buf).await.unwrap();

        // read
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();

        assert_eq!(buf, read_buf);
        assert_eq!(read_buf.len(), 0);
        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 2);
    }

    // general functions
    #[test]
    fn test_block_count_to_byte_count() {
        let block_count = 10;
        let byte_count = block_count_to_byte_count(block_count);
        assert!(byte_count == block_count as u64 * VMGS_BYTES_PER_BLOCK as u64);
    }

    #[test]
    fn test_validate_header() {
        let mut header = VmgsHeader::new_zeroed();
        header.signature = VMGS_SIGNATURE;
        header.version = VMGS_VERSION_3_0;
        header.header_size = size_of::<VmgsHeader>() as u32;
        header.file_table_offset = VMGS_MIN_FILE_BLOCK_OFFSET;
        header.file_table_size = VMGS_FILE_TABLE_BLOCK_SIZE;
        header.checksum = compute_crc32(header.as_bytes());

        let result = validate_header(&header);
        assert!(result.is_ok());

        let mut header_signature = header;
        header_signature.signature = 0;
        header_signature.checksum = 0;
        header_signature.checksum = compute_crc32(header_signature.as_bytes());
        let result = validate_header(&header_signature);
        match result {
            Err(Error::InvalidFormat(err)) => assert_eq!(err, "Invalid header signature"),
            _ => panic!(),
        };

        let mut header_version = header;
        header_version.version = 0;
        header_version.checksum = 0;
        header_version.checksum = compute_crc32(header_version.as_bytes());
        match validate_header(&header_version) {
            Err(Error::InvalidFormat(err)) => assert_eq!(err, "Invalid header version"),
            _ => panic!(),
        };

        let mut header_header_size = header;
        header_header_size.header_size = 0;
        header_header_size.checksum = 0;
        header_header_size.checksum = compute_crc32(header_header_size.as_bytes());
        match validate_header(&header_header_size) {
            Err(Error::InvalidFormat(err)) => assert_eq!(err, "Invalid header size"),
            _ => panic!(),
        };

        let mut header_ft_offset = header;
        header_ft_offset.file_table_offset = 0;
        header_ft_offset.checksum = 0;
        header_ft_offset.checksum = compute_crc32(header_ft_offset.as_bytes());
        match validate_header(&header_ft_offset) {
            Err(Error::InvalidFormat(err)) => assert_eq!(err, "Invalid file table offset"),
            _ => panic!(),
        };

        let mut header_ft_size = header;
        header_ft_size.file_table_size = 0;
        header_ft_size.checksum = 0;
        header_ft_size.checksum = compute_crc32(header_ft_size.as_bytes());
        match validate_header(&header_ft_size) {
            Err(Error::InvalidFormat(err)) => assert_eq!(err, "Invalid file table size"),
            _ => panic!(),
        };
    }

    // #[test]
    // fn test_initialize_file_metadata() {
    //     let mut file_table = VmgsFileTable::new_zeroed();

    //     file_table.entries[0].offset = 6;
    //     file_table.entries[0].allocation_size = 1;
    //     file_table.entries[1].offset = 2;
    //     file_table.entries[1].allocation_size = 1;
    //     file_table.entries[2].offset = 4;
    //     file_table.entries[2].allocation_size = 5;
    //     file_table.entries[3].offset = 3;
    //     file_table.entries[3].allocation_size = 3;

    //     let block_capacity = 1000;

    //     let fcbs = initialize_file_metadata(&file_table, VMGS_VERSION_3_0, block_capacity).unwrap();
    //     // assert VmgsFileEntry correctly converted to FileControlBlock
    //     assert!(fcbs[&FileId(0)].block_offset == 6);
    //     assert!(fcbs[&FileId(0)].allocated_blocks.get() == 1);
    //     assert!(fcbs[&FileId(1)].block_offset == 2);
    //     assert!(fcbs[&FileId(1)].allocated_blocks.get() == 1);
    //     assert!(fcbs[&FileId(2)].block_offset == 4);
    //     assert!(fcbs[&FileId(2)].allocated_blocks.get() == 5);
    //     assert!(fcbs[&FileId(3)].block_offset == 3);
    //     assert!(fcbs[&FileId(3)].allocated_blocks.get() == 3);
    // }

    #[test]
    fn test_round_up_count() {
        assert!(round_up_count(0, 4096) == 0);
        assert!(round_up_count(1, 4096) == 4096);
        assert!(round_up_count(4095, 4096) == 4096);
        assert!(round_up_count(4096, 4096) == 4096);
        assert!(round_up_count(4097, 4096) == 8192);
    }

    #[async_test]
    async fn test_header_sequence_overflow() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();

        vmgs.active_header_sequence_number = u32::MAX;

        // write
        let buf = b"hello world";
        vmgs.write_file(FileId::BIOS_NVRAM, buf).await.unwrap();

        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 0);

        vmgs.set_active_header(0, u32::MAX);

        let mut new_header = VmgsHeader::new_zeroed();
        vmgs.update_header_inner(&mut new_header).await.unwrap();

        assert_eq!(vmgs.active_header_index, 1);
        assert_eq!(vmgs.active_header_sequence_number, 0);
        assert_eq!(new_header.sequence, 0);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn write_file_v3() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
        let encryption_key = [12; VMGS_ENCRYPTION_KEY_SIZE];

        // write
        let buf = b"hello world";
        let buf_1 = b"hello universe";
        vmgs.write_file(FileId::BIOS_NVRAM, buf).await.unwrap();
        vmgs.update_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        vmgs.write_file_encrypted(FileId::TPM_PPI, buf_1)
            .await
            .unwrap();

        // read
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, &*read_buf);
        let info = vmgs.get_file_info(FileId::TPM_PPI).unwrap();
        assert_eq!(info.valid_bytes as usize, buf_1.len());
        let read_buf = vmgs.read_file(FileId::TPM_PPI).await.unwrap();
        assert_eq!(buf_1, &*read_buf);

        // Read the file after re-opening the vmgs file
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk, None).await.unwrap();
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf.as_bytes());
        let info = vmgs.get_file_info(FileId::TPM_PPI).unwrap();
        assert_eq!(info.valid_bytes as usize, buf_1.len());
        let read_buf = vmgs.read_file_raw(FileId::TPM_PPI).await.unwrap();
        assert_ne!(buf_1, read_buf.as_bytes());

        // Unlock datastore
        vmgs.unlock_with_encryption_key(&encryption_key)
            .await
            .unwrap();
        let info = vmgs.get_file_info(FileId::TPM_PPI).unwrap();
        assert_eq!(info.valid_bytes as usize, buf_1.len());
        let read_buf = vmgs.read_file(FileId::TPM_PPI).await.unwrap();
        assert_eq!(buf_1, &*read_buf);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn overwrite_file_v3() {
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk, None).await.unwrap();
        let encryption_key = [1; VMGS_ENCRYPTION_KEY_SIZE];
        let buf = vec![1; 8 * 1024];
        let buf_1 = vec![2; 8 * 1024];

        // Add root key.
        vmgs.add_new_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));

        // Write a file to the store.
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, &buf)
            .await
            .unwrap();

        // Encrypt and overwrite the original file.
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, &buf_1)
            .await
            .unwrap();

        // Verify new file contents
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf_1, read_buf);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn file_encryption() {
        let buf: Vec<u8> = (0..255).collect();
        let encryption_key = [1; VMGS_ENCRYPTION_KEY_SIZE];

        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();

        // Add datastore key.
        vmgs.add_new_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));

        // Write a file to the store.
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, &buf)
            .await
            .unwrap();

        // Read the file, without closing the datastore
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);

        drop(vmgs);

        // Read the file, after closing and reopening the data store.
        let mut vmgs = Vmgs::open(disk, None).await.unwrap();

        let info = vmgs.get_file_info(FileId::BIOS_NVRAM).unwrap();
        assert_eq!(info.valid_bytes as usize, buf.len());

        // Unlock the store.

        vmgs.unlock_with_encryption_key(&encryption_key)
            .await
            .unwrap();

        assert_eq!(vmgs.active_datastore_key_index, Some(0));

        // Change to a new datastore key.
        let new_encryption_key = [2; VMGS_ENCRYPTION_KEY_SIZE];
        vmgs.add_new_encryption_key(&new_encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));
        vmgs.remove_encryption_key(0).await.unwrap();

        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn add_new_encryption_key() {
        let buf: Vec<u8> = (0..255).collect();
        let encryption_key = [1; VMGS_ENCRYPTION_KEY_SIZE];
        let new_encryption_key = [5; VMGS_ENCRYPTION_KEY_SIZE];

        // Initialize version 3 data store
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();

        // Add datastore key.
        vmgs.add_new_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));

        // Write a file to the store.
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, &buf)
            .await
            .unwrap();

        // Read the file, after closing and reopening the data store.
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk.clone(), None).await.unwrap();
        vmgs.unlock_with_encryption_key(&encryption_key)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(read_buf, buf);

        // Add new datastore key.
        vmgs.add_new_encryption_key(&new_encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));

        // Read the file by using two different datastore keys, after closing and reopening the data store.
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk, None).await.unwrap();
        vmgs.unlock_with_encryption_key(&encryption_key)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));
        vmgs.unlock_with_encryption_key(&new_encryption_key)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(read_buf, buf);

        // Remove the newly added datastore key and add it again.
        vmgs.remove_encryption_key(1).await.unwrap();
        vmgs.add_new_encryption_key(&new_encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));

        // Remove the old datastore key
        vmgs.remove_encryption_key(0).await.unwrap();
        let result = vmgs.unlock_with_encryption_key(&encryption_key).await;
        assert!(matches!(result, Err(Error::Other(_))));

        // Try to remove the old datastore key again
        let result = vmgs.remove_encryption_key(0).await;
        assert!(matches!(result, Err(Error::Other(_))));

        // Remove the new datastore key and try to read file content, which should be in encrypted state
        vmgs.remove_encryption_key(1).await.unwrap();
        let read_buf = vmgs.read_file_raw(FileId::BIOS_NVRAM).await;
        assert_ne!(read_buf.unwrap(), buf);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn test_write_file_encrypted() {
        // Call write_file_encrypted on an unencrypted VMGS and check that plaintext was written

        // Initialize version 3 data store
        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();
        let buf = b"This is plaintext";

        // call write file encrypted
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, buf)
            .await
            .unwrap();

        // Read
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(vmgs.encryption_algorithm, EncryptionAlgorithm::NONE);
        assert_eq!(buf, &*read_buf);

        // ensure that when we re-create the VMGS object, we can still read the
        // FileId as plaintext
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk, None).await.unwrap();

        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(vmgs.encryption_algorithm, EncryptionAlgorithm::NONE);
        assert_eq!(buf, &*read_buf);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn test_logger() {
        let disk = new_test_file();
        let data = Arc::new(Mutex::new(String::new()));
        let mut vmgs = Vmgs::format_new(
            disk.clone(),
            Some(Arc::new(TestVmgsLogger { data: data.clone() })),
        )
        .await
        .unwrap();
        let encryption_key = [12; VMGS_ENCRYPTION_KEY_SIZE];

        // write
        let buf = b"hello world";
        vmgs.update_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, buf)
            .await
            .unwrap();

        let fcb = vmgs.fcbs.get_mut(&FileId::BIOS_NVRAM).unwrap();

        // Manipulate the nonce and expect the read to fail.
        fcb.nonce[0] ^= 1;

        // read and expect to fail
        let result = vmgs.read_file(FileId::BIOS_NVRAM).await;
        assert!(result.is_err());

        // verify that the string is logged
        let result = data.lock();
        assert_eq!(*result, "test logger");
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn update_key() {
        let buf: Vec<u8> = (0..255).collect();
        let encryption_key = [1; VMGS_ENCRYPTION_KEY_SIZE];

        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();

        // Add datastore key.
        vmgs.update_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));
        assert_eq!(vmgs.datastore_key_count, 1);

        // Write a file to the store.
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, &buf)
            .await
            .unwrap();

        // Read the file, without closing the datastore
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);

        // Close and reopen the store
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk.clone(), None).await.unwrap();

        // Unlock the store.
        vmgs.unlock_with_encryption_key(&encryption_key)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));

        // Read the file again
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);

        // Change to a new datastore key.
        let new_encryption_key = [2; VMGS_ENCRYPTION_KEY_SIZE];
        vmgs.update_encryption_key(&new_encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));
        assert_eq!(vmgs.datastore_key_count, 1);

        // Read the file again
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);

        // Close and reopen the store
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk, None).await.unwrap();

        // Unlock the store.
        vmgs.unlock_with_encryption_key(&new_encryption_key)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));

        // Read the file again
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);
    }

    #[cfg(with_encryption)]
    #[async_test]
    async fn update_key_no_space() {
        let buf: Vec<u8> = (0..255).collect();
        let encryption_key = [1; VMGS_ENCRYPTION_KEY_SIZE];

        let disk = new_test_file();
        let mut vmgs = Vmgs::format_new(disk.clone(), None).await.unwrap();

        // Add datastore key.
        vmgs.update_encryption_key(&encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));
        assert_eq!(vmgs.datastore_key_count, 1);

        // Write a file to the store.
        vmgs.write_file_encrypted(FileId::BIOS_NVRAM, &buf)
            .await
            .unwrap();

        // Read the file, without closing the datastore
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);

        // Add a new datastore key, but don't remove the old one.
        let new_encryption_key = [2; VMGS_ENCRYPTION_KEY_SIZE];
        vmgs.add_new_encryption_key(&new_encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));
        assert_eq!(vmgs.datastore_key_count, 2);

        // Close and reopen the store
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk.clone(), None).await.unwrap();

        // Unlock the store.
        vmgs.unlock_with_encryption_key(&new_encryption_key)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(1));

        // Add yet another new datastore key. This should remove both previous keys
        let another_encryption_key = [2; VMGS_ENCRYPTION_KEY_SIZE];
        vmgs.update_encryption_key(&another_encryption_key, EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));
        assert_eq!(vmgs.datastore_key_count, 1);

        // Close and reopen the store
        drop(vmgs);
        let mut vmgs = Vmgs::open(disk, None).await.unwrap();

        // Unlock the store.
        vmgs.unlock_with_encryption_key(&another_encryption_key)
            .await
            .unwrap();
        assert_eq!(vmgs.active_datastore_key_index, Some(0));

        // Read the file again
        let read_buf = vmgs.read_file(FileId::BIOS_NVRAM).await.unwrap();
        assert_eq!(buf, read_buf);
    }
}
