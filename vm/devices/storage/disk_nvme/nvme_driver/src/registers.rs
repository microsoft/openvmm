// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device register access.

use super::spec;
use inspect::Inspect;
use pal_async::driver::Driver;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use tracing::instrument;
use user_driver::DeviceBacking;
use user_driver::DeviceRegisterIo;
use user_driver::backoff::Backoff;

#[derive(Inspect)]
#[inspect(extra = "Self::inspect_extra")]
pub(crate) struct DeviceRegisters<T: DeviceBacking> {
    pub bar0: Bar0<T::Registers>,
    pub cap: spec::Cap,
    #[inspect(with = "inspect::AtomicMut")]
    inspect_hw: AtomicBool,
}

impl<T: DeviceBacking> DeviceRegisters<T> {
    pub fn new(bar0: Bar0<T::Registers>) -> Self {
        let cap = bar0.cap();
        Self {
            bar0,
            cap,
            inspect_hw: false.into(),
        }
    }

    fn doorbell_offset(&self, qid: u16, completion: bool) -> usize {
        let doorbell_stride_bits = self.cap.dstrd() + 2;
        0x1000 + ((qid as usize * 2 + completion as usize) << doorbell_stride_bits)
    }

    pub fn doorbell(&self, qid: u16, completion: bool, value: u32) {
        self.bar0
            .0
            .write_u32(self.doorbell_offset(qid, completion), value)
    }

    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        if self.inspect_hw.load(Relaxed) {
            resp.child("hw", |req| {
                req.respond()
                    .field("cap", self.bar0.cap())
                    .hex("asq", self.bar0.asq())
                    .hex("acq", self.bar0.acq())
                    .field("cc", self.bar0.cc())
                    .field("csts", self.bar0.csts())
                    .field("aqa", self.bar0.aqa())
                    .hex(
                        "sq0tdbl",
                        self.bar0.0.read_u32(self.doorbell_offset(0, false)),
                    )
                    .hex(
                        "cq0hdbl",
                        self.bar0.0.read_u32(self.doorbell_offset(0, true)),
                    );
            });
        }
    }
}

#[derive(Inspect)]
pub(crate) struct Bar0<T: Inspect>(#[inspect(flatten)] pub T);

macro_rules! reg32 {
    ($get:ident, $set:ident, $reg:ident, $ty:ty) => {
        #[allow(dead_code)]
        pub fn $get(&self) -> $ty {
            let r = <$ty>::from(self.0.read_u32(spec::Register::$reg.0 as usize));
            tracing::trace!(r = ?r, reg = stringify!($reg), "Read register");
            r
        }
        #[allow(dead_code)]
        pub fn $set(&self, v: $ty) {
            tracing::trace!(v = ?v, reg = stringify!($reg), "Writing register");
            self.0.write_u32(spec::Register::$reg.0 as usize, v.into())
        }
    };
}

macro_rules! reg64 {
    ($get:ident, $set:ident, $reg:ident, $ty:ty) => {
        #[allow(dead_code)]
        pub fn $get(&self) -> $ty {
            let r = <$ty>::from(self.0.read_u64(spec::Register::$reg.0 as usize));
            tracing::trace!(r = ?r, reg = stringify!($reg), "Read register");
            r
        }
        #[allow(dead_code)]
        pub fn $set(&self, v: $ty) {
            tracing::trace!(v = ?v, reg = stringify!($reg), "Writing register");
            self.0.write_u64(spec::Register::$reg.0 as usize, v.into())
        }
    };
}

impl<T: DeviceRegisterIo + Inspect> Bar0<T> {
    reg64!(cap, set_cap, CAP, spec::Cap);
    reg64!(asq, set_asq, ASQ, u64);
    reg64!(acq, set_acq, ACQ, u64);
    reg32!(cc, set_cc, CC, spec::Cc);
    reg32!(csts, set_csts, CSTS, spec::Csts);
    reg32!(aqa, set_aqa, AQA, spec::Aqa);

    #[instrument(skip_all)]
    pub async fn reset(&self, driver: &dyn Driver) -> Result<(), u32> {
        let cc = self.cc().with_en(false);
        self.set_cc(cc);
        let mut backoff = Backoff::new(driver);
        // Loop until either RDY bit is cleared
        // or CSTS read returns -1 which means
        // failure in emulation layer.
        loop {
            let csts = self.csts();
            if !csts.rdy() {
                break Ok(());
            }
            if u32::from(csts) == !0 {
                break Err(!0);
            }
            backoff.back_off().await;
        }
    }
}
