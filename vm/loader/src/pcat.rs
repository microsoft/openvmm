// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCAT specific loader definitions and implementation.

use crate::importer::BootPageAcceptance;
use crate::importer::ImageLoad;
use crate::importer::SegmentRegister;
use crate::importer::X86Register;
use hvdef::HV_PAGE_SIZE;
use thiserror::Error;

const STARTUP_IMAGE_TOP_PAGES: u64 = 0x100000 / HV_PAGE_SIZE; // 1MB
const STARTUP_IMAGE_MAX_PAGES: u64 = 0x20000 / HV_PAGE_SIZE; // 128KB
const IMAGE_SIZE: u64 = 0x40000; // 256KB
const FOUR_GB: u64 = 0x100000000;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to read firmware")]
    Firmware(#[source] std::io::Error),
    #[error("Firmware size invalid")]
    InvalidImageSize,
    #[error("Max ram below 4GB invalid")]
    InvalidMaxRamBelow4gb,
    #[error("Importer error")]
    Importer(#[source] anyhow::Error),
}

/// Load a PCAT BIOS image with the provided config type.
///
/// In cases where the image is preloaded into RAM or is being provided by a
/// ROM, `image` can be set to None.
pub fn load(
    importer: &mut dyn ImageLoad<X86Register>,
    image: Option<&[u8]>,
    max_ram_below_4gb: Option<u64>,
) -> Result<(), Error> {
    if let Some(image) = image {
        if (image.len() as u64) != IMAGE_SIZE {
            return Err(Error::InvalidImageSize);
        }

        let Some(max_ram_below_4gb) = max_ram_below_4gb else {
            return Err(Error::InvalidMaxRamBelow4gb);
        };

        if (max_ram_below_4gb % HV_PAGE_SIZE != 0)
            || (max_ram_below_4gb == 0)
            || (max_ram_below_4gb > FOUR_GB)
        {
            return Err(Error::InvalidMaxRamBelow4gb);
        }

        let image_page_count = image.len() as u64 / HV_PAGE_SIZE;
        let max_page_below_4gb = max_ram_below_4gb / HV_PAGE_SIZE;
        let page_base = max_page_below_4gb - image_page_count;
        tracing::trace!(
            image_page_count,
            max_page_below_4gb,
            page_base,
            "pcat pre-import",
        );

        importer
            .import_pages(
                page_base,
                image_page_count,
                "pcat-image",
                BootPageAcceptance::Exclusive,
                image,
            )
            .map_err(Error::Importer)?;

        tracing::trace!("max below 4gb bios import complete");

        let image_page_count = image_page_count.min(STARTUP_IMAGE_MAX_PAGES);
        let page_base = STARTUP_IMAGE_TOP_PAGES - image_page_count;
        let start = image.len() - ((image_page_count * HV_PAGE_SIZE) as usize);

        tracing::trace!(image_page_count, page_base, "pcat import",);

        importer
            .import_pages(
                page_base,
                image_page_count,
                "pcat-top-pages",
                BootPageAcceptance::Exclusive,
                &image[start..],
            )
            .map_err(Error::Importer)?;

        tracing::trace!("below 1mb bios import complete");
    }

    // Enable MTRRs, default MTRR is uncached, and set lowest 640KB and highest 128KB as WB
    let mut import_reg = |register| {
        importer
            .import_vp_register(register)
            .map_err(Error::Importer)
    };
    import_reg(X86Register::MtrrDefType(0xc00))?;
    import_reg(X86Register::MtrrFix64k00000(0x0606060606060606))?;
    import_reg(X86Register::MtrrFix16k80000(0x0606060606060606))?;
    import_reg(X86Register::MtrrFix4kE0000(0x0606060606060606))?;
    import_reg(X86Register::MtrrFix4kE8000(0x0606060606060606))?;
    import_reg(X86Register::MtrrFix4kF0000(0x0606060606060606))?;
    import_reg(X86Register::MtrrFix4kF8000(0x0606060606060606))?;

    // The PCAT bios expects the reset vector to be mapped at the top of 1MB, not the architectural reset
    // value at the top of 4GB, so set CS to the same values HyperV does to ensure things work across all
    // virtualization stacks.
    import_reg(X86Register::Cs(SegmentRegister {
        base: 0xF0000,
        limit: 0xFFFF,
        selector: 0xF000,
        attributes: 0x9B,
    }))?;

    Ok(())
}
