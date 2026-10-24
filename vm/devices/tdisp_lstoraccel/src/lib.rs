// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LstoraccelDeviceSpecificInfo {
    /// What the device reported about its media encryption key.
    pub key_info: KeyInfo,
    /// The reported NVMe controller state and queue hash.
    pub nvme_info: NvmeTdispInfo,
}

/// Reads the vendor/device specific information reported by a local storage
/// accelerator in its TDI interface report.
///
/// * `data` - The whole DEVICE_SPECIFIC_INFO field, as reported by the device.
pub fn deserialize_device_specific_info(
    data: &[u8],
) -> anyhow::Result<LstoraccelDeviceSpecificInfo> {
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
    info: &LstoraccelDeviceSpecificInfo,
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
    use tdisp::devicereport::TdiReportStruct;
    use tdisp::devicereport::TdispTdiReportInterfaceInfo;
    use tdisp::devicereport::TdispTdiReportMmioFlags;
    use tdisp::devicereport::TdispTdiReportMmioInterfaceInfo;
    use tdisp::devicereport::deserialize_tdi_report;
    use tdisp::devicereport::serialize_tdi_report;

    fn report(key_info: KeyInfo) -> LstoraccelDeviceSpecificInfo {
        LstoraccelDeviceSpecificInfo {
            key_info,
            nvme_info: NvmeTdispInfo {
                tni: TNI_NVME,
                ti: 0x4695_5b01,
                cu: 0x12,
                vu: 0x23,
                reserved0: [0; 2],
                aqa: 0x0007_0003,
                asq: 0x1122_3344,
                acq: 0x5566_7788,
                cmbmsc: 0x99aa_bbcc,
                pmrmsc: 0xddee_ff00,
                pmrctl: 0x1234_5678,
                queue_hash: [0x5a; 48],
                reserved1: [0; 44],
            },
        }
    }

    fn fixed_nvme_section() -> [u8; 128] {
        let mut data = [0; 128];
        data[..0x24].copy_from_slice(&[
            0x65, 0x4d, 0x56, 0x4e, // TNI.
            0x01, 0x5b, 0x95, 0x46, // TI.
            0x12, 0x23, 0x00, 0x00, // CU, VU, reserved.
            0x03, 0x00, 0x07, 0x00, // AQA.
            0x44, 0x33, 0x22, 0x11, // ASQ.
            0x88, 0x77, 0x66, 0x55, // ACQ.
            0xcc, 0xbb, 0xaa, 0x99, // CMBMSC.
            0x00, 0xff, 0xee, 0xdd, // PMRMSC.
            0x78, 0x56, 0x34, 0x12, // PMRCTL.
        ]);
        data[0x24..0x54].fill(0x5a);
        data
    }

    #[test]
    fn complete_report_matches_fixed_wire_bytes() {
        let mut not_loaded = vec![0x00, 0x02, 0x01];
        not_loaded.extend(std::iter::repeat_n(0xee, 258));

        let mut loaded = vec![0x01];
        loaded.extend_from_slice(&[0x33; 48]);

        for (mut data, key_info) in [
            (
                not_loaded,
                KeyInfo::NotLoaded {
                    mek_wrapping_key_cert: vec![0xee; 258],
                },
            ),
            (
                loaded,
                KeyInfo::Loaded {
                    mek_confirmation_digest: [0x33; SHA384_DIGEST_SIZE],
                },
            ),
        ] {
            data.extend_from_slice(&fixed_nvme_section());
            let expected = report(key_info);

            let parsed = deserialize_device_specific_info(&data).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(serialize_device_specific_info(&expected).unwrap(), data);

            let ti = parsed.nvme_info.tdisp_information();
            assert_eq!(ti.ver(), NVME_TDISP_INFO_VERSION);
            assert!(ti.acps());
            assert!(ti.env());
            assert_eq!(ti.shnv(), 2);
            assert_eq!(ti.shstv(), 1);
            assert!(ti.stv());
            assert_eq!(ti.cssv(), 5);
            assert_eq!(ti.mpsv(), 9);
            assert_eq!(ti.iosqesv(), 6);
            assert_eq!(ti.iocqesv(), 4);
        }
    }

    #[test]
    fn device_specific_info_is_parsed_from_a_complete_tdi_report() {
        for key_info in [
            KeyInfo::NotLoaded {
                mek_wrapping_key_cert: vec![0xee; 258],
            },
            KeyInfo::Loaded {
                mek_confirmation_digest: [0x33; SHA384_DIGEST_SIZE],
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
        let info = report(KeyInfo::NotLoaded {
            mek_wrapping_key_cert: vec![0xee; usize::from(u16::MAX) + 1],
        });

        assert!(serialize_device_specific_info(&info).is_err());
    }

    #[test]
    fn complete_report_survives_both_key_forms_and_variable_offsets() {
        let key_infos = (0..=7)
            .map(|len| KeyInfo::NotLoaded {
                mek_wrapping_key_cert: vec![0xee; len],
            })
            .chain(std::iter::once(KeyInfo::Loaded {
                mek_confirmation_digest: [0x33; SHA384_DIGEST_SIZE],
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
                mek_wrapping_key_cert: vec![0xee; 5],
            },
            KeyInfo::Loaded {
                mek_confirmation_digest: [0x33; SHA384_DIGEST_SIZE],
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
            mek_confirmation_digest: [0x33; SHA384_DIGEST_SIZE],
        };
        let nvme_offset = serialize_key_info(&key_info).unwrap().len();
        let data = serialize_device_specific_info(&report(key_info)).unwrap();

        let mut invalid = data.clone();
        invalid[nvme_offset] ^= 1;
        assert!(deserialize_device_specific_info(&invalid).is_err());

        for version in [0, 2, 0x11, 0xff] {
            let mut invalid = data.clone();
            invalid[nvme_offset + 4] = version;
            assert!(deserialize_device_specific_info(&invalid).is_err());
        }

        let mut overlong = data;
        overlong.push(0);
        assert!(deserialize_device_specific_info(&overlong).is_err());
    }
}
