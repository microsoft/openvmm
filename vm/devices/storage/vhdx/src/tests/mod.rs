// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod crash_tests;
pub mod support;

#[cfg(test)]
mod integration {
	use crate::AsyncFile;
	use crate::cache::PageCache;
	use crate::create::{self, CreateParams};
	use crate::format;
	use crate::header;
	use crate::known_meta;
	use crate::metadata::METADATA_TAG;
	use crate::metadata::MetadataTable;
	use crate::region;
	use crate::tests::support::InMemoryFile;
	use guid::Guid;
	use pal_async::async_test;
	use std::sync::Arc;

	fn metadata_cache(file: InMemoryFile, metadata_offset: u64) -> PageCache<InMemoryFile> {
		let mut cache = PageCache::new(Arc::new(file), None, None, 0);
		cache.register_tag(METADATA_TAG, metadata_offset);
		cache
	}

	#[async_test]
	async fn create_then_parse_full_roundtrip() {
		let disk_size = 2 * format::GB1;
		let mut params = CreateParams {
			disk_size,
			block_size: 2 * format::MB1 as u32,
			logical_sector_size: 512,
			physical_sector_size: 4096,
			..CreateParams::default()
		};
		let file = InMemoryFile::new(0);
		create::create(&file, &mut params).await.unwrap();
		let file_length = file.file_size().await.unwrap();

		let parsed_header = header::parse_headers(&file, file_length).await.unwrap();
		assert_eq!(parsed_header.log_guid, Guid::ZERO);
		assert_ne!(parsed_header.file_write_guid, Guid::ZERO);
		assert_ne!(parsed_header.data_write_guid, Guid::ZERO);

		let regions = region::parse_region_tables(&file).await.unwrap();
		assert!(regions.rewrite_data.is_none());
		assert!(regions.bat_offset > 0);
		assert!(regions.metadata_offset > 0);

		let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
			.await
			.unwrap();

		known_meta::verify_known_metadata(&table, false).unwrap();

		let cache = metadata_cache(file, regions.metadata_offset);
		let meta = known_meta::read_known_metadata(&cache, &table)
			.await
			.unwrap();

		assert_eq!(meta.disk_size, disk_size);
		assert_eq!(meta.block_size, 2 * format::MB1 as u32);
		assert_eq!(meta.logical_sector_size, 512);
		assert_eq!(meta.physical_sector_size, 4096);
		assert!(!meta.has_parent);
		assert!(!meta.leave_blocks_allocated);
		assert_ne!(meta.page_83_data, Guid::ZERO);
	}

	#[async_test]
	async fn create_differencing_then_parse() {
		let mut params = CreateParams {
			disk_size: format::GB1,
			has_parent: true,
			..CreateParams::default()
		};
		let file = InMemoryFile::new(0);
		create::create(&file, &mut params).await.unwrap();
		let file_length = file.file_size().await.unwrap();

		let _header = header::parse_headers(&file, file_length).await.unwrap();
		let regions = region::parse_region_tables(&file).await.unwrap();
		let table = MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length)
			.await
			.unwrap();

		known_meta::verify_known_metadata(&table, false).unwrap();
		let cache = metadata_cache(file, regions.metadata_offset);
		let meta = known_meta::read_known_metadata(&cache, &table)
			.await
			.unwrap();

		assert!(meta.has_parent);
	}
}
