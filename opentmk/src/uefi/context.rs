use core::ops::Range;

use alloc::boxed::Box;
use hvdef::Vtl;



pub trait TestCtxTrait {
    fn get_vp_count(&self) -> u32;
    fn get_current_vp(&self) -> u32;
    fn get_current_vtl(&self) -> Vtl;
    
    fn start_on_vp(&mut self, cmd: VpExecutor);

    fn queue_command_vp(&mut self, cmd: VpExecutor);

    fn switch_to_high_vtl(&mut self);
    fn switch_to_low_vtl(&mut self);

    fn setup_partition_vtl(&mut self, vtl: Vtl);
    fn setup_interrupt_handler(&mut self);
    fn set_interupt_idx(&mut self, interrupt_idx: u8, handler: fn());

    fn setup_vtl_protection(&mut self);
    fn setup_secure_intercept(&mut self, interrupt_idx: u8);
    fn apply_vtl_protection_for_memory(&mut self, range: Range<u64>, vtl: Vtl);
    fn write_msr(&mut self, msr: u32, value: u64);
    fn read_msr(&mut self, msr: u32) -> u64;

    fn start_running_vp_with_default_context(&mut self, cmd: VpExecutor);
    fn set_default_ctx_to_vp(&mut self, vp_index: u32, vtl: Vtl);
    fn enable_vp_vtl_with_default_context(&mut self, vp_index: u32, vtl: Vtl);

    fn get_register(&mut self, reg: u32) -> u128;
}

pub struct VpExecutor {
    vp_index: u32,
    vtl: Vtl,
    cmd: Option<Box<dyn FnOnce(&mut dyn TestCtxTrait)>>,
}

impl VpExecutor {
    pub fn new(vp_index: u32, vtl: Vtl) -> Self {
        VpExecutor {
            vp_index,
            vtl,
            cmd: None,
        }
    }

    pub fn command(mut self, cmd: impl FnOnce(&mut dyn TestCtxTrait) + 'static) -> Self {
        self.cmd = Some(Box::new(cmd));
        self
    }

    pub fn get(mut self) -> (u32, Vtl, Option<Box<dyn FnOnce(&mut dyn TestCtxTrait)>>) {
        let cmd = self.cmd.take();
        (self.vp_index, self.vtl, cmd)
    }
}