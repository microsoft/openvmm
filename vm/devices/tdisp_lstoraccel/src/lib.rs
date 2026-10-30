// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate reads the vendor/device specific information a local storage
//! accelerator (lstoraccel) places in the DEVICE_SPECIFIC_INFO field of its
//! TDI interface report, which `tdisp::devicereport::deserialize_tdi_report`
//! surfaces as an uninterpreted byte slice.
//!
//! The field holds two sections, in order:
//!
//! 1. The key information section, describing the state of the device's media
//!    encryption key. It is variable length, so it also determines where the
//!    second section begins.
//! 2. The NVMe controller section, containing the reported controller state
//!    and queue hash.
//!
//! DEVICE_SPECIFIC_INFO layout (offsets are relative to this field):
//!
//! ```text
//! Offset  Size     Field                         Present when
//! 0x00    1        MEK present flag               always (0 or 1)
//! 0x01    2        Wrapping-key certificate size  flag == 0
//! 0x03    L        Wrapping-key X.509 certificate flag == 0
//! 0x01    48       SHA-384 confirmation digest    flag == 1
//! X       128      NVMe controller information   always
//! ```
//!
//! The two key-info forms are mutually exclusive. L is the certificate size;
//! X is 3 + L when the flag is 0, or 49 when it is 1. The confirmation digest
//! covers the CSP X.509 certificate followed by the wrapped key.
//!
//! The NVMe section follows the NVM Express NVMe over PCIe Transport
//! Specification, Revision 1.4, Section 3.8.9, Figure 69 (NVMe TDISP
//! DEVICE_INTERFACE_REPORT Reporting Structure). Both sections are parsed;
//! the key confirmation digest and NVMe queue hash are not verified.

#![forbid(unsafe_code)]

/// The key information section of the vendor/device specific information.
pub mod key_info;

use crate::key_info::KeyInfo;
use crate::key_info::deserialize_key_info;
use crate::key_info::serialize_key_info;
use anyhow::Context;
use nvme_spec::tdisp::NVME_TDISP_INFO_VERSION;
use nvme_spec::tdisp::NvmeTdispInfo;
use nvme_spec::tdisp::TNI_NVME;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// The deserialized DEVICE_SPECIFIC_INFO field of a local storage accelerator.
///
/// The key information borrows from the report buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LstoraccelDeviceSpecificInfo<'a> {
    /// What the device reported about its media encryption key.
    pub key_info: KeyInfo<'a>,
    /// The reported NVMe controller state and queue hash.
    pub nvme_info: NvmeTdispInfo,
}

/// Reads the vendor/device specific information reported by a local storage
/// accelerator in its TDI interface report.
///
/// * `data` - The whole DEVICE_SPECIFIC_INFO field, as reported by the device.
pub fn deserialize_device_specific_info(
    data: &[u8],
) -> anyhow::Result<LstoraccelDeviceSpecificInfo<'_>> {
    let (key_info, nvme_section) =
        deserialize_key_info(data).context("failed to deserialize the key information section")?;

    let nvme_info = NvmeTdispInfo::read_from_bytes(nvme_section)
        .map_err(|e| anyhow::anyhow!("failed to deserialize the NVMe controller section: {e:?}"))?;

    anyhow::ensure!(
        nvme_info.tni == TNI_NVME,
        "unexpected NVMe identifier {:#010x}",
        nvme_info.tni
    );

    let version = nvme_info.tdisp_information().ver();
    anyhow::ensure!(
        version == NVME_TDISP_INFO_VERSION,
        "unsupported NVMe controller information version {version}"
    );

    Ok(LstoraccelDeviceSpecificInfo {
        key_info,
        nvme_info,
    })
}

/// Writes vendor/device specific information out in the form a device reports
/// it, so that an emulated local storage accelerator can describe itself.
///
/// The reported digests are copied, not recomputed.
///
/// * `info` - The information to write out.
///
/// Returns an error if the key information cannot be serialized.
pub fn serialize_device_specific_info(
    info: &LstoraccelDeviceSpecificInfo<'_>,
) -> anyhow::Result<Vec<u8>> {
    let mut buffer = serialize_key_info(&info.key_info)
        .context("failed to serialize the key information section")?;
    buffer.extend_from_slice(info.nvme_info.as_bytes());
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key_info::SHA384_DIGEST_SIZE;
    use nvme_spec::tdisp::TdispInformation;
    use tdisp::devicereport::TdiReportStruct;
    use tdisp::devicereport::TdispTdiReportInterfaceInfo;
    use tdisp::devicereport::TdispTdiReportMmioFlags;
    use tdisp::devicereport::TdispTdiReportMmioInterfaceInfo;
    use tdisp::devicereport::deserialize_tdi_report;
    use tdisp::devicereport::serialize_tdi_report;

    // Arbitrary test values, not values prescribed by the device or NVMe spec.
    // Distinct register bytes expose byte-order and field-placement errors.
    const TEST_CU: u8 = 0x12;
    const TEST_VU: u8 = 0x23;
    const TEST_AQA: u32 = 0x0007_0003; // ASQS = 3, ACQS = 7.
    const TEST_ASQ: u32 = 0x1122_3344;
    const TEST_ACQ: u32 = 0x5566_7788;
    const TEST_CMBMSC: u32 = 0x99aa_bbcc;
    const TEST_PMRMSC: u32 = 0xddee_ff00;
    const TEST_PMRCTL: u32 = 0x1234_5678;

    const TEST_SHNV: u8 = 2;
    const TEST_SHSTV: u8 = 1;
    const TEST_CSSV: u8 = 5;
    const TEST_MPSV: u8 = 9;
    const TEST_IOSQESV: u8 = 6;
    const TEST_IOCQESV: u8 = 4;

    // 0x0102 bytes exercises both bytes of the device-specific length field.
    // This is an opaque parser payload, not a valid X.509 certificate.
    const TEST_CERTIFICATE: [u8; 258] = [0xee; 258];

    // Synthetic digest bytes; these tests check transport, not hash verification.
    const TEST_CONFIRMATION_DIGEST: [u8; SHA384_DIGEST_SIZE] = [0x33; SHA384_DIGEST_SIZE];
    const TEST_QUEUE_HASH: [u8; SHA384_DIGEST_SIZE] = [0x5a; SHA384_DIGEST_SIZE];

    // NVMe over PCIe Transport Specification 1.4, Section 3.8.9,
    // Figure 69. These offsets are independent of the Rust structure layout.
    const NVME_TNI_OFFSET: usize = 0x00;
    const NVME_TI_OFFSET: usize = 0x04;
    const NVME_CU_OFFSET: usize = 0x08;
    const NVME_VU_OFFSET: usize = 0x09;
    const NVME_AQA_OFFSET: usize = 0x0c;
    const NVME_ASQ_OFFSET: usize = 0x10;
    const NVME_ACQ_OFFSET: usize = 0x14;
    const NVME_CMBMSC_OFFSET: usize = 0x18;
    const NVME_PMRMSC_OFFSET: usize = 0x1c;
    const NVME_PMRCTL_OFFSET: usize = 0x20;
    const NVME_QUEUE_HASH_OFFSET: usize = 0x24;
    const NVME_RESERVED1_OFFSET: usize = 0x54;
    const NVME_REPORT_SIZE: usize = 0x80;

    // Spec-defined identifier bytes, independent of TNI_NVME.
    const NVME_TNI_WIRE_BYTES: [u8; 4] = [0x65, 0x4d, 0x56, 0x4e];

    // Independent encoding of the synthetic TI values used by report():
    // VER=1, ACPS=1, ENV=1, SHNV=2, SHSTV=1, STV=1,
    // CSSV=5, MPSV=9, IOSQESV=6, IOCQESV=4.
    const TEST_TI_WIRE_BYTES: [u8; 4] = [0x01, 0x5b, 0x95, 0x46];

    // Device-specific prefix flag indicating that the MEK is present.
    const TEST_MEK_PRESENT_FLAG: u8 = 0x01;

    fn report<'a>(key_info: KeyInfo<'a>) -> LstoraccelDeviceSpecificInfo<'a> {
        LstoraccelDeviceSpecificInfo {
            key_info,
            nvme_info: NvmeTdispInfo {
                tni: TNI_NVME,
                ti: TdispInformation::new()
                    .with_ver(NVME_TDISP_INFO_VERSION)
                    .with_acps(true)
                    .with_env(true)
                    .with_shnv(TEST_SHNV)
                    .with_shstv(TEST_SHSTV)
                    .with_stv(true)
                    .with_cssv(TEST_CSSV)
                    .with_mpsv(TEST_MPSV)
                    .with_iosqesv(TEST_IOSQESV)
                    .with_iocqesv(TEST_IOCQESV)
                    .into(),
                cu: TEST_CU,
                vu: TEST_VU,
                reserved0: [0; 2],
                aqa: TEST_AQA,
                asq: TEST_ASQ,
                acq: TEST_ACQ,
                cmbmsc: TEST_CMBMSC,
                pmrmsc: TEST_PMRMSC,
                pmrctl: TEST_PMRCTL,
                queue_hash: TEST_QUEUE_HASH,
                reserved1: [0; 44],
            },
        }
    }

    #[test]
    fn complete_report_matches_section_bytes() {
        // Device-specific prefix: MEK absent, certificate length 0x0102.
        let mut not_loaded = vec![0x00, 0x02, 0x01];
        not_loaded.extend_from_slice(&TEST_CERTIFICATE);

        // Device-specific prefix: MEK present, followed by its digest.
        let mut loaded = vec![0x01];
        loaded.extend_from_slice(&TEST_CONFIRMATION_DIGEST);

        for (mut data, key_info) in [
            (
                not_loaded,
                KeyInfo::NotLoaded {
                    mek_wrapping_key_cert: &TEST_CERTIFICATE,
                },
            ),
            (
                loaded,
                KeyInfo::Loaded {
                    mek_confirmation_digest: &TEST_CONFIRMATION_DIGEST,
                },
            ),
        ] {
            let expected = report(key_info);
            data.extend_from_slice(expected.nvme_info.as_bytes());

            let parsed = deserialize_device_specific_info(&data).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(serialize_device_specific_info(&expected).unwrap(), data);

            let ti = parsed.nvme_info.tdisp_information();
            assert_eq!(ti.ver(), NVME_TDISP_INFO_VERSION);
            assert!(ti.acps());
            assert!(ti.env());
            assert_eq!(ti.shnv(), TEST_SHNV);
            assert_eq!(ti.shstv(), TEST_SHSTV);
            assert!(ti.stv());
            assert_eq!(ti.cssv(), TEST_CSSV);
            assert_eq!(ti.mpsv(), TEST_MPSV);
            assert_eq!(ti.iosqesv(), TEST_IOSQESV);
            assert_eq!(ti.iocqesv(), TEST_IOCQESV);
        }
    }

    #[test]
    fn complete_report_matches_spec_wire_layout() {
        let mut nvme_bytes = [0u8; NVME_REPORT_SIZE];
        nvme_bytes[NVME_TNI_OFFSET..NVME_TI_OFFSET].copy_from_slice(&NVME_TNI_WIRE_BYTES);
        nvme_bytes[NVME_TI_OFFSET..NVME_CU_OFFSET].copy_from_slice(&TEST_TI_WIRE_BYTES);
        nvme_bytes[NVME_CU_OFFSET] = TEST_CU;
        nvme_bytes[NVME_VU_OFFSET] = TEST_VU;
        nvme_bytes[NVME_AQA_OFFSET..NVME_ASQ_OFFSET].copy_from_slice(&TEST_AQA.to_le_bytes());
        nvme_bytes[NVME_ASQ_OFFSET..NVME_ACQ_OFFSET].copy_from_slice(&TEST_ASQ.to_le_bytes());
        nvme_bytes[NVME_ACQ_OFFSET..NVME_CMBMSC_OFFSET].copy_from_slice(&TEST_ACQ.to_le_bytes());
        nvme_bytes[NVME_CMBMSC_OFFSET..NVME_PMRMSC_OFFSET]
            .copy_from_slice(&TEST_CMBMSC.to_le_bytes());
        nvme_bytes[NVME_PMRMSC_OFFSET..NVME_PMRCTL_OFFSET]
            .copy_from_slice(&TEST_PMRMSC.to_le_bytes());
        nvme_bytes[NVME_PMRCTL_OFFSET..NVME_QUEUE_HASH_OFFSET]
            .copy_from_slice(&TEST_PMRCTL.to_le_bytes());
        nvme_bytes[NVME_QUEUE_HASH_OFFSET..NVME_RESERVED1_OFFSET].copy_from_slice(&TEST_QUEUE_HASH);

        let mut data = vec![TEST_MEK_PRESENT_FLAG];
        data.extend_from_slice(&TEST_CONFIRMATION_DIGEST);
        data.extend_from_slice(&nvme_bytes);

        let parsed = deserialize_device_specific_info(&data).unwrap();
        let expected = report(KeyInfo::Loaded {
            mek_confirmation_digest: &TEST_CONFIRMATION_DIGEST,
        });
        assert_eq!(parsed, expected);

        let ti = parsed.nvme_info.tdisp_information();
        assert_eq!(ti.ver(), NVME_TDISP_INFO_VERSION);
        assert!(ti.acps());
        assert!(ti.env());
        assert_eq!(ti.shnv(), TEST_SHNV);
        assert_eq!(ti.shstv(), TEST_SHSTV);
        assert!(ti.stv());
        assert_eq!(ti.cssv(), TEST_CSSV);
        assert_eq!(ti.mpsv(), TEST_MPSV);
        assert_eq!(ti.iosqesv(), TEST_IOSQESV);
        assert_eq!(ti.iocqesv(), TEST_IOCQESV);

        assert_eq!(serialize_device_specific_info(&expected).unwrap(), data);
    }

    #[test]
    fn device_specific_info_is_parsed_from_a_complete_tdi_report() {
        for key_info in [
            KeyInfo::NotLoaded {
                mek_wrapping_key_cert: &TEST_CERTIFICATE,
            },
            KeyInfo::Loaded {
                mek_confirmation_digest: &TEST_CONFIRMATION_DIGEST,
            },
        ] {
            let expected = report(key_info);
            let tdi_report = TdiReportStruct {
                interface_info: TdispTdiReportInterfaceInfo::new(),
                msi_x_message_control: 0,
                lnr_control: 0,
                tph_control: 0,
                mmio_interface_info: vec![
                    TdispTdiReportMmioInterfaceInfo {
                        first_4k_page_offset: 0xf000_0000,
                        num_4k_pages: 16,
                        flags: TdispTdiReportMmioFlags::new(),
                        range_id: 0,
                    },
                    TdispTdiReportMmioInterfaceInfo {
                        first_4k_page_offset: 0xf001_0000,
                        num_4k_pages: 1,
                        flags: TdispTdiReportMmioFlags::new().with_range_maps_msix_table(true),
                        range_id: 1,
                    },
                ],
                device_specific_info: serialize_device_specific_info(&expected).unwrap(),
            };

            let data = serialize_tdi_report(&tdi_report);
            let parsed_report = deserialize_tdi_report(&data).unwrap();

            assert_eq!(parsed_report.mmio_interface_info.len(), 2);
            assert_eq!(
                parsed_report.device_specific_info,
                tdi_report.device_specific_info
            );
            assert_eq!(
                deserialize_device_specific_info(&parsed_report.device_specific_info).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn oversized_wrapping_key_certificate_prevents_report_serialization() {
        let certificate = vec![0xee; usize::from(u16::MAX) + 1];
        let info = report(KeyInfo::NotLoaded {
            mek_wrapping_key_cert: &certificate,
        });

        assert!(serialize_device_specific_info(&info).is_err());
    }

    #[test]
    fn complete_report_survives_both_key_forms_and_variable_offsets() {
        let certificate = [0xee; 7];
        let key_infos = (0..=certificate.len())
            .map(|len| KeyInfo::NotLoaded {
                mek_wrapping_key_cert: &certificate[..len],
            })
            .chain(std::iter::once(KeyInfo::Loaded {
                mek_confirmation_digest: &TEST_CONFIRMATION_DIGEST,
            }));

        for key_info in key_infos {
            let key_size = serialize_key_info(&key_info).unwrap().len();
            let info = report(key_info);
            let data = serialize_device_specific_info(&info).unwrap();

            assert_eq!(data.len(), key_size + size_of::<NvmeTdispInfo>());
            assert_eq!(deserialize_device_specific_info(&data).unwrap(), info);
        }
    }

    #[test]
    fn missing_or_truncated_nvme_section_is_rejected() {
        for key_info in [
            KeyInfo::NotLoaded {
                mek_wrapping_key_cert: &[0xee; 5],
            },
            KeyInfo::Loaded {
                mek_confirmation_digest: &TEST_CONFIRMATION_DIGEST,
            },
        ] {
            let key_size = serialize_key_info(&key_info).unwrap().len();
            let data = serialize_device_specific_info(&report(key_info)).unwrap();

            for len in key_size..data.len() {
                assert!(deserialize_device_specific_info(&data[..len]).is_err());
            }
        }
    }

    #[test]
    fn invalid_nvme_section_is_rejected() {
        let key_info = KeyInfo::Loaded {
            mek_confirmation_digest: &TEST_CONFIRMATION_DIGEST,
        };
        let nvme_offset = serialize_key_info(&key_info).unwrap().len();
        let data = serialize_device_specific_info(&report(key_info)).unwrap();

        let mut invalid = data.clone();
        let tni_offset = nvme_offset + std::mem::offset_of!(NvmeTdispInfo, tni);
        invalid[tni_offset] ^= 1;
        assert!(deserialize_device_specific_info(&invalid).is_err());

        let ti_offset = nvme_offset + std::mem::offset_of!(NvmeTdispInfo, ti);
        for version in [0, 2, 0x11, 0xff] {
            let mut invalid = data.clone();
            // The version occupies the first byte of TI.
            invalid[ti_offset] = version;
            assert!(deserialize_device_specific_info(&invalid).is_err());
        }

        let mut overlong = data;
        overlong.push(0);
        assert!(deserialize_device_specific_info(&overlong).is_err());
    }
}
