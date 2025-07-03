use log::info;

use crate::{arch::tpm, tmk_assert};

pub fn exec<T>(ctx: &mut T) {
    let date = crate::arch::rtc::read_rtc();
    log::info!("Current RTC: {} UNIX epoch: {}", date, date.to_unix_epoch_sec());

    let mut tpm = tpm::tpm_driver_example();
    info!("TPM driver example started {:?}", tpm);

}