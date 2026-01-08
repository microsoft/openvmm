// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::ScsiError;
use super::SimpleScsiDisk;
use crate::UNMAP_RANGE_DESCRIPTOR_COUNT_MAX;
use crate::VHDMP_MAX_WRITE_SAME_LENGTH_BYTES;
use crate::scsi;
use guestmem::MemoryWrite;
use guid::Guid;
use scsi::AdditionalSenseCode;
use scsi_buffers::RequestBuffers;
use scsi_core::Request;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

type U16BE = zerocopy::byteorder::U16<zerocopy::byteorder::BigEndian>;
type U32BE = zerocopy::byteorder::U32<zerocopy::byteorder::BigEndian>;
type U64BE = zerocopy::byteorder::U64<zerocopy::byteorder::BigEndian>;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct VhdmpVpdWindowsBlockDeviceRodLimitsEcopDescriptorHeader {
    pub ecop_descriptor_type: U16BE,
    pub ecop_descriptor_length: U16BE,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct VhdmpVpdWindowsBlockDeviceRodLimitsEcopDescriptor {
    pub header: VhdmpVpdWindowsBlockDeviceRodLimitsEcopDescriptorHeader,
    pub microsoft_signature: u8,                  // 0x4D --> 'M'
    pub microsoft_command_id_and_versionsion: u8, // 0x1F
    pub populate_token_and_write_using_token_command_op_code: u8, // 0x83
    pub receive_rod_token_information_command_op_code: u8, // 0x84
    pub reserved1: [u8; 2],
    pub maximum_range_descriptors: U16BE,
    pub maximum_inactivity_timer: U32BE,
    pub default_inactivity_timer: U32BE,
    pub maximum_token_transfer_size: U64BE,
    pub optimal_transfer_count: U64BE,
}

pub const INQUIRY_DATA_TEMPLATE: scsi::InquiryData = scsi::InquiryData {
    header: scsi::InquiryDataHeader {
        // READ_ONLY_DIRECT_ACCESS_DEVICE is interpreted as CD-ROM, so despite
        // the fact that this is a read-only surface, don't report
        // READ_ONLY_DIRECT_ACCESS_DEVICE.
        device_type: scsi::DIRECT_ACCESS_DEVICE,
        // This is a virtual hard drive, so the media is never removable from
        // the device.  The device itself may be removable, depending on whether
        // the device is the boot drive or not.  The device itself may be hot
        // removable or not, depending on whether any of the backing stores are
        // hot removable.
        flags2: scsi::InquiryDataFlag2::new()
            .with_device_type_modifier(0x00)
            .with_removable_media(false),
        versions: scsi::T10_VERSION_SPC3,
        flags3: scsi::InquiryDataFlag3::new()
            .with_response_data_format(scsi::T10_RESPONSE_DATA_SPC3)
            .with_aerc(false)
            .with_hi_support(false)
            .with_norm_aca(false)
            .with_reserved_bit(false),
        additional_length: (size_of::<scsi::InquiryData>() - size_of::<scsi::InquiryDataHeader>())
            as u8,
    },
    reserved: [0; 2],
    misc: 0,
    vendor_id: *b"Msft    ",
    product_id: *b"Virtual Disk    ",
    product_revision_level: *b"1.0 ",
    vendor_specific: [0; 20],
    reserved3: [0; 2],
    version_descriptors: [0; 8],
    reserved4: [0; 30],
};

/// Writes a VPD page.
///
/// Assumes that allocation_length is already validated to be at least
/// `size_of::<scsi::VpdPageHeader>()`.
fn write_vpd_page<T: ?Sized + IntoBytes + Immutable + KnownLayout>(
    external_data: &RequestBuffers<'_>,
    allocation_length: usize,
    page_code: u8,
    page_data: &T,
) -> Result<usize, ScsiError> {
    let header = scsi::VpdPageHeader {
        device_type: scsi::DIRECT_ACCESS_DEVICE,
        page_code,
        reserved: 0,
        page_length: size_of_val(page_data).try_into().unwrap(),
    };

    let tx = std::cmp::min(
        allocation_length,
        size_of_val(&header) + size_of_val(page_data),
    );

    let mut writer = external_data.writer();
    writer
        .write(header.as_bytes())
        .map_err(ScsiError::MemoryAccess)?;

    writer
        .write(&page_data.as_bytes()[..tx - size_of_val(&header)])
        .map_err(ScsiError::MemoryAccess)?;

    Ok(tx)
}

impl SimpleScsiDisk {
    fn handle_vpd_supported_pages(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        let mut supported_pages = vec![
            scsi::VPD_SUPPORTED_PAGES,                // support this page
            scsi::VPD_DEVICE_IDENTIFIERS,             // support identifiers page
            scsi::VPD_MSFT_PAGING_EXTENT_PROPERTIES,  // Microsoft Paging Extent Properties
            scsi::VPD_MSFT_VIRTUAL_DEVICE_PROPERTIES, // Microsoft Virtual Device Properties
        ];

        if self.scsi_parameters.support_odx {
            supported_pages.push(scsi::VPD_THIRD_PARTY_COPY);
        }

        if self.scsi_parameters.support_unmap {
            supported_pages.extend([
                scsi::VPD_BLOCK_DEVICE_CHARACTERISTICS,
                scsi::VPD_BLOCK_LIMITS,
                scsi::VPD_LOGICAL_BLOCK_PROVISIONING,
            ]);
        }

        if !self.scsi_parameters.serial_number.is_empty() {
            supported_pages.push(scsi::VPD_SERIAL_NUMBER);
        }

        supported_pages.sort_unstable();

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_SUPPORTED_PAGES,
            supported_pages.as_slice(),
        )
    }

    fn handle_vpd_serial_number(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_SERIAL_NUMBER,
            self.scsi_parameters.serial_number.as_slice(),
        )
    }

    fn handle_vpd_device_identifiers(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        #[repr(C)]
        #[derive(IntoBytes, Immutable, KnownLayout)]
        struct Ids {
            t10_id: scsi::VpdT10Id,
            // NAA ID page - need this for compliance
            // as we move from BusTypeScsi to BusTypeSas.
            naa_id: scsi::VpdNaaId,
        }

        // Construct a full local copy and transfer as much as possible.
        // Start with the template and initialize the T10 and NAA
        // Ids appropriately.
        let mut page = Ids {
            t10_id: scsi::VpdT10Id {
                header: scsi::VpdIdentificationDescriptor {
                    code_set: scsi::VPD_CODE_SET_BINARY,
                    identifiertype: scsi::VPD_IDENTIFIER_TYPE_VENDOR_ID, //VpdAssocDevice = 0
                    reserved3: 0x00,
                    identifier_length: (size_of::<scsi::VpdT10Id>()
                        - size_of::<scsi::VpdIdentificationDescriptor>())
                        as u8,
                },
                vendor_id: self.scsi_parameters.identity.vendor_id.into(),
                context_guid: self.scsi_parameters.disk_id,
            },
            naa_id: scsi::VpdNaaId {
                header: scsi::VpdIdentificationDescriptor {
                    code_set: scsi::VPD_CODE_SET_BINARY,
                    identifiertype: scsi::VPD_IDENTIFIER_TYPE_FCPH_NAME, //VpdAssocDevice = 0
                    reserved3: 0x00,
                    identifier_length: (size_of::<scsi::VpdNaaId>()
                        - size_of::<scsi::VpdIdentificationDescriptor>())
                        as u8,
                },
                ouid_msb: 0x60, // 6(NAA), 0 (OuidMSB MSFT OUID used = 00-22-48 (hex))
                ouid_middle: [0x02, 0x24],
                ouid_lsb: 0x80,
                vendor_specific_id: [0; 12],
            },
        };

        // Best effort uniqueness:
        // Where possible we use version 4 UUID for the T10Id guid,
        // which has the format: xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx
        // where x is any hexadecimal digit and y is one of 8, 9, a or b
        //
        // Use the first and last 6 bytes of the T10Id's ContextGuid
        // which are the most random bytes.
        let id_split_size = page.naa_id.vendor_specific_id.len() / 2;
        page.naa_id.vendor_specific_id[..id_split_size]
            .copy_from_slice(&self.scsi_parameters.disk_id.as_bytes()[..id_split_size]);
        page.naa_id.vendor_specific_id[id_split_size..]
            .copy_from_slice(&page.t10_id.context_guid[10..]);

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_DEVICE_IDENTIFIERS,
            &page,
        )
    }

    fn handle_vpd_block_limits(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        let max_write_same_length = (self.scsi_parameters.maximum_transfer_length as u64)
            .min(VHDMP_MAX_WRITE_SAME_LENGTH_BYTES)
            >> self.sector_shift;
        // 0 indicates unsupported

        // Since we are only here if unmap is supported, ensure the reported
        // granularity is non-zero (or the guest will think that unmap is not
        // supported).
        let optimal_unmap_granularity = self.scsi_parameters.optimal_unmap_sectors.max(1);

        let page = scsi::VpdBlockLimitsDescriptor {
            reserved0: 0x00,
            max_compare_and_write_length: 0, // don't support compare_and_write
            max_unmap_lba_count: u32::MAX.into(),
            max_unmap_block_descriptor_count: u32::from(UNMAP_RANGE_DESCRIPTOR_COUNT_MAX).into(),
            optimal_unmap_granularity: optimal_unmap_granularity.into(),
            unmap_granularity_alignment: [0x80, 0x00, 0x00, 0x00], // UGAValid = 1
            max_write_same_length: max_write_same_length.into(),
            max_atomic_transfer_length: self
                .scsi_parameters
                .atomic_parameters
                .maximum_atomic_transfer_length
                .into(),
            atomic_alignment: self
                .scsi_parameters
                .atomic_parameters
                .atomic_alignment
                .into(),
            atomic_transfer_length_granularity: self
                .scsi_parameters
                .atomic_parameters
                .atomic_transfer_length_granularity
                .into(),
            maximum_atomic_transfer_length_with_atomic_boundary: self
                .scsi_parameters
                .atomic_parameters
                .maximum_atomic_transfer_length_with_atomic_boundary
                .into(),
            maximum_atomic_boundary_size: self
                .scsi_parameters
                .atomic_parameters
                .maximum_atomic_boundary_size
                .into(),
            ..FromZeros::new_zeroed()
        };

        tracing::debug!(
            max_lba_count = page.max_unmap_lba_count.get(), // TODO: didn't update?
            max_block_descriptor_count = page.max_unmap_block_descriptor_count.get(),
            optimal_unmap_granularity,
            "handle_vpd_block_limits unmap properties",
        );

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_BLOCK_LIMITS,
            &page,
        )
    }

    fn handle_vpd_block_device_characteristics(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        let page = scsi::VpdBlockDeviceCharacteristicsPage {
            medium_rotation_rate: self.scsi_parameters.medium_rotation_rate.into(),
            ..FromZeros::new_zeroed()
        };

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_BLOCK_DEVICE_CHARACTERISTICS,
            &page,
        )
    }

    fn handle_vpd_third_party_copy(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        // Before increasing this value, consider what happens in the host when max Q
        // depth of offload writes from various VMs is pending in the hosts's adapter.
        // Say a first VM is issuing a single write, and happens to get in line behind
        // a bunch of offload writes from other VMs.  Those offload writes can take up
        // to 4 seconds each in theory, but because of this length limit, will tend to
        // take less time than that.  Even so, assume a Q of 256 64MB offload writes,
        // and assume that the offload writes correspond to physical writes in the
        // array.  Now assume that the array can physically write at 1GB per second.
        // This queue of 256 64MB writes represents 16GB of data to be written, which
        // will take 16.4 seconds to write assuming no overhead.  That's already a bit
        // too much, as it's more than the first VM's 10 second SCSI timeout for a small
        // normal write.  The Storage QoS Filter is meant to help avoid putting all 16GB worth
        // of writes in flight to the HW at once, but the filter can have incorrect
        // low estimates for IO time cost sometimes.
        //
        // Generally, one SCSI reset in the guest is ok, but too many in quick
        // succession can start to propagate errors up to the workload in the VM.  Also,
        // VM SCSI reset will still actually complete all the IOs outstanding with their
        // normal completion status, so there's no way for a bunch more offload IOs to
        // "sneak in" ahead of the normal write that caused the timeout - that normal
        // write will still get a chance to complete with success despite a first SCSI
        // timeout having triggered in the guest.
        //
        // This value is already as big as I'm comfortable with given the current
        // environment (as of Feb 2012), so if you want to increase it further, first
        // make sure the previous paragraphs won't bite.
        const VHDMP_MAX_BYTES_PER_OFFLOAD: u64 = 64 * 1024 * 1024;

        // Make local copy of default page. Struct copy.
        let page = VhdmpVpdWindowsBlockDeviceRodLimitsEcopDescriptor {
            header: VhdmpVpdWindowsBlockDeviceRodLimitsEcopDescriptorHeader {
                ecop_descriptor_type: 0.into(),
                ecop_descriptor_length: ((size_of::<
                    VhdmpVpdWindowsBlockDeviceRodLimitsEcopDescriptor,
                >() - size_of::<
                    VhdmpVpdWindowsBlockDeviceRodLimitsEcopDescriptorHeader,
                >()) as u16)
                    .into(),
            },
            microsoft_signature: 0x4D,                  // 0x4D --> 'M'
            microsoft_command_id_and_versionsion: 0x1F, // 0x1F
            populate_token_and_write_using_token_command_op_code: 0x83, // scsi::SCSIOP_EXTENDED_COPY
            receive_rod_token_information_command_op_code: 0x84, // scsi::SCSIOP_RECEIVE_ROD_TOKEN_INFORMATION
            reserved1: [0; 2],
            maximum_range_descriptors: 8.into(),
            maximum_inactivity_timer: 0.into(), //do not report, since don't know
            default_inactivity_timer: 0.into(),
            maximum_token_transfer_size: (VHDMP_MAX_BYTES_PER_OFFLOAD >> self.sector_shift).into(),
            optimal_transfer_count: (VHDMP_MAX_BYTES_PER_OFFLOAD >> self.sector_shift).into(),
        };

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_THIRD_PARTY_COPY,
            &page,
        )
    }

    fn handle_vpd_msft_virtual_device_properties(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        const VPD_MSFT_VIRTUAL_DEVICE_PROPERTIES_PAGE_SIGNATURE: Guid =
            guid::guid!("89a98f15-c928-4d8b-94cd-ef51faa99d33");

        let page = scsi::VpdMsftVirtualDevicePropertiesPage {
            version: 1, // Version, leave at 1 to maintain back-compatibility with downlevel guest OSes
            flags: 0,   //LBPRZ, DisableIoRetries, Spaces
            reserved: [0; 2],
            signature: VPD_MSFT_VIRTUAL_DEVICE_PROPERTIES_PAGE_SIGNATURE.into(),
        };

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_MSFT_VIRTUAL_DEVICE_PROPERTIES,
            &page,
        )
    }

    fn handle_vpd_msft_paging_extent_properties(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        let page = scsi::VpdMsftPagingExtentPropertiesPage {
            version: 1,
            mode_select_extension: 1,
            reserved: [0; 2],
        };

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_MSFT_PAGING_EXTENT_PROPERTIES,
            &page,
        )
    }

    fn handle_vpd_logical_block_provisioning(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        // Build the VPD page with the proper threshold exponent value
        // for our VHD disk size
        //
        // the threshold exponent is calculated according to this:
        //     ((LastLBA + 1) / 2^(threshold exponent)) < 2^32
        //
        // and the exponent must be at least 1
        let threshold_exponent = (sector_count >> 32)
            .next_power_of_two()
            .trailing_zeros()
            .max(1) as u8;

        let mut page = scsi::VpdLogicalBlockProvisioningPage {
            threshold_exponent,
            flags: 0,
            provisioning_type: scsi::PROVISIONING_TYPE_THIN,
            reserved2: 0,
        };

        if self.scsi_parameters.support_unmap {
            page.flags = 0x80;
        }

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_LOGICAL_BLOCK_PROVISIONING,
            &page,
        )
    }

    fn handle_no_vpd_page(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        // Determine the number of bytes to transfer. We will fail if
        // we cannot transfer the page header, and we will not transfer
        // more than the full page. We will allow a partial page
        // transfer.
        if allocation_length < size_of::<scsi::InquiryDataHeader>() {
            return Err(ScsiError::SrbError);
        }

        let page = scsi::InquiryData {
            misc: 0x02, // CommandQueue = 1
            vendor_id: self.scsi_parameters.identity.vendor_id.into(),
            product_id: self.scsi_parameters.identity.product_id.into(),
            product_revision_level: self.scsi_parameters.identity.product_revision_level.into(),
            ..INQUIRY_DATA_TEMPLATE
        };

        let tx = std::cmp::min(allocation_length, size_of_val(&page));
        external_data
            .writer()
            .write(&page.as_bytes()[..tx])
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }

    pub(crate) fn handle_inquiry(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiError> {
        let cdb = scsi::CdbInquiry::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        let allocation_length = cdb.allocation_length.get() as usize;
        if external_data.len() < allocation_length {
            return Err(ScsiError::SrbError);
        }

        //  We don't support the command support data bit - it's deprecated
        if cdb.flags.csd() {
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        // Setting the page code without requesting product data is an error.
        let enable_vpd = cdb.flags.vpd();
        if cdb.page_code != 0 && !enable_vpd {
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        if enable_vpd {
            if allocation_length < size_of::<scsi::VpdPageHeader>() {
                return Err(ScsiError::SrbError);
            }

            match cdb.page_code {
                scsi::VPD_SUPPORTED_PAGES => {
                    self.handle_vpd_supported_pages(external_data, allocation_length)
                }
                scsi::VPD_SERIAL_NUMBER if !self.scsi_parameters.serial_number.is_empty() => {
                    self.handle_vpd_serial_number(external_data, allocation_length)
                }
                scsi::VPD_DEVICE_IDENTIFIERS => {
                    self.handle_vpd_device_identifiers(external_data, allocation_length)
                }
                scsi::VPD_BLOCK_LIMITS if self.scsi_parameters.support_unmap => {
                    self.handle_vpd_block_limits(external_data, allocation_length)
                }
                scsi::VPD_BLOCK_DEVICE_CHARACTERISTICS if self.scsi_parameters.support_unmap => {
                    self.handle_vpd_block_device_characteristics(external_data, allocation_length)
                }
                scsi::VPD_LOGICAL_BLOCK_PROVISIONING if self.scsi_parameters.support_unmap => self
                    .handle_vpd_logical_block_provisioning(
                        external_data,
                        allocation_length,
                        sector_count,
                    ),
                scsi::VPD_THIRD_PARTY_COPY if self.scsi_parameters.support_odx => {
                    self.handle_vpd_third_party_copy(external_data, allocation_length)
                }
                scsi::VPD_MSFT_VIRTUAL_DEVICE_PROPERTIES => {
                    self.handle_vpd_msft_virtual_device_properties(external_data, allocation_length)
                }
                scsi::VPD_MSFT_PAGING_EXTENT_PROPERTIES => {
                    self.handle_vpd_msft_paging_extent_properties(external_data, allocation_length)
                }
                n => Err(ScsiError::UnsupportedVpdPageCode(n)),
            }
        } else {
            self.handle_no_vpd_page(external_data, allocation_length)
        }
    }
}
