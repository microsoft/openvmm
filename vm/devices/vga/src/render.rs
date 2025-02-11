// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to render the VGA image.

use crate::emu::TextModeState;
use framebuffer::FramebufferLocalControl;
use guestmem::GuestMemory;
use inspect::Inspect;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTask;
use task_control::StopTask;
use task_control::TaskControl;
use video_core::FramebufferFormat;
use vmcore::vm_task::VmTaskDriver;
use zerocopy::IntoBytes;

const VRAM_RENDER_OFFSET: usize = 0x400000;

#[derive(Inspect, Clone, PartialEq, Eq)]
#[inspect(tag = "mode")]
pub enum RenderState {
    None,
    #[inspect(transparent)]
    Text(TextRenderState),
    #[inspect(transparent)]
    Graphics(GraphicsRenderState),
}

#[derive(Inspect, Clone, PartialEq, Eq)]
pub struct GraphicsRenderState {
    pub bits_per_pixel: u8,
    pub cur_page_start_offset: u32,
    pub line_offset_pixels: u16,
    pub width: u16,
    pub height: u16,
    pub pixel_double: bool,
    pub pixel_pan: u8,
    #[inspect(skip)]
    pub mapping_table: [u32; 256],
}

#[derive(Inspect, Clone, PartialEq, Eq)]
pub struct TextRenderState {
    pub text: TextModeState,
    pub cur_page_start_offset: u32,
    #[inspect(skip)]
    pub mapping_table: [u32; 256],
}

#[derive(Inspect)]
#[inspect(transparent)]
pub struct Renderer {
    task: TaskControl<RendererCore, Arc<Mutex<RenderState>>>,
    #[inspect(skip)]
    state: Arc<Mutex<RenderState>>,
}

pub struct RenderControl {
    last_update: Option<RenderState>,
    state: Arc<Mutex<RenderState>>,
}

#[derive(Inspect)]
struct RendererCore {
    #[inspect(skip)]
    control: FramebufferLocalControl,
    #[inspect(skip)]
    timer: PolledTimer,
    vram: GuestMemory,
}

impl Renderer {
    pub fn new(driver: &VmTaskDriver, control: FramebufferLocalControl, vram: GuestMemory) -> Self {
        let mut task = TaskControl::new(RendererCore {
            control,
            timer: PolledTimer::new(driver),
            vram,
        });
        let state = Arc::new(Mutex::new(RenderState::None));
        task.insert(driver, "vga-render", state.clone());
        Self { task, state }
    }

    pub fn start(&mut self) {
        self.task.start();
    }

    pub async fn stop(&mut self) {
        self.task.stop().await;
    }

    pub fn control(&self) -> RenderControl {
        RenderControl {
            last_update: None,
            state: self.state.clone(),
        }
    }
}

impl RenderControl {
    pub fn update(&mut self, state: RenderState) {
        // Only take the lock if the render state changed. That avoids lock
        // contention with the renderer thread, which is CPU bound on debug
        // builds, at least.
        if self.last_update.as_ref() != Some(&state) {
            self.last_update = Some(state.clone());
            *self.state.lock() = state;
        }
    }
}

impl AsyncRun<Arc<Mutex<RenderState>>> for RendererCore {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut Arc<Mutex<RenderState>>,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(self.render_loop(state)).await
    }
}

impl InspectTask<Arc<Mutex<RenderState>>> for RendererCore {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&Arc<Mutex<RenderState>>>) {
        req.respond().merge(self).merge(state);
    }
}

impl RendererCore {
    async fn render_loop(&mut self, state: &Mutex<RenderState>) {
        loop {
            self.render(&state.lock());
            self.timer.sleep(Duration::from_millis(100)).await;
        }
    }

    fn render(&mut self, state: &RenderState) {
        match state {
            RenderState::None => {}
            RenderState::Text(state) => {
                self.render_text(state);
            }
            RenderState::Graphics(
                state @ GraphicsRenderState {
                    bits_per_pixel: 32, ..
                },
            ) => {
                self.render_32bpp(state);
            }
            RenderState::Graphics(
                state @ GraphicsRenderState {
                    bits_per_pixel: 16, ..
                },
            ) => {
                self.render_16bpp(state);
            }
            RenderState::Graphics(state) => {
                self.render_4bpp(state);
            }
        }
    }

    fn render_4bpp(&mut self, state: &GraphicsRenderState) {
        let width = state.width.into();
        let height = state.height.into();
        let bytes_per_line = width * 4;
        self.control.set_format(FramebufferFormat {
            width,
            height,
            bytes_per_line,
            offset: VRAM_RENDER_OFFSET,
        });

        let params = crate::text_mode::Parameters {
            column_left: 0,
            column_right: width as i32,
            clip_left: 0,
            clip_right: width as i32,
            row_number_top: 0,
            row_number_bottom: height as i32,
        };

        let src_start = (state.cur_page_start_offset & 0xffff) * 4;
        match state.bits_per_pixel {
            4 => {
                crate::non_linear::blit_non_linear4to32(
                    &self.vram,
                    &params,
                    &state.mapping_table,
                    state.pixel_pan,
                    state.pixel_double,
                    state.pixel_double,
                    src_start as usize,
                    state.line_offset_pixels,
                    VRAM_RENDER_OFFSET,
                    bytes_per_line,
                );
            }
            _ => self
                .vram
                .fill_at(VRAM_RENDER_OFFSET as u64, 0xcc, bytes_per_line * height)
                .unwrap(),
        }
    }

    fn render_16bpp(&mut self, state: &GraphicsRenderState) {
        let width = state.width.into();
        let height = state.height.into();
        let bytes_per_line = width * 4;
        self.control.set_format(FramebufferFormat {
            width,
            height,
            bytes_per_line,
            offset: VRAM_RENDER_OFFSET,
        });
        let mut line = vec![0u16; width];
        let mut wide_line = vec![0u32; width];
        for row in 0..height {
            self.vram
                .read_at(
                    (row * state.line_offset_pixels as usize * 2) as u64,
                    line.as_mut_bytes(),
                )
                .unwrap();
            for (s, d) in line.iter().zip(&mut wide_line) {
                let s = *s as u32;
                let red = (s << 8) & 0x00F80000;
                let green = (s << 5) & 0x0000FC00;
                let blue = (s << 3) & 0x000000F8;
                let red = (red | (red >> 5)) & 0x00FF0000;
                let green = (green | (green >> 6)) & 0x0000FF00;
                let blue = (blue | (blue >> 5)) & 0x000000FF;
                *d = red | green | blue;
            }
            self.vram
                .write_at(
                    (VRAM_RENDER_OFFSET + row * bytes_per_line) as u64,
                    wide_line.as_slice().as_bytes(),
                )
                .unwrap();
        }
    }

    fn render_32bpp(&mut self, state: &GraphicsRenderState) {
        // No blitting necessary, just render the VRAM in-place.
        self.control.set_format(FramebufferFormat {
            width: state.width.into(),
            height: state.height.into(),
            bytes_per_line: (state.line_offset_pixels * 4).into(),
            offset: 0,
        });
    }

    fn render_text(&mut self, state: &TextRenderState) {
        let width = 640;
        let height = state.text.text_rows as usize * state.text.text_char_height as usize;
        self.control.set_format(FramebufferFormat {
            width,
            height,
            bytes_per_line: width * 4,
            offset: VRAM_RENDER_OFFSET,
        });
        let params = crate::text_mode::Parameters {
            column_left: 0,
            column_right: width as i32,
            clip_left: 0,
            clip_right: width as i32,
            row_number_top: 0,
            row_number_bottom: height as i32,
        };

        let text_start = (state.cur_page_start_offset & 0xffff) * 8;
        crate::text_mode::blit_text(
            &self.vram,
            &params,
            &state.text,
            &state.mapping_table,
            text_start as usize,
            VRAM_RENDER_OFFSET,
        );
    }
}
