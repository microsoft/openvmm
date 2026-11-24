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

#[cfg(test)]
mod tests {
    use super::*;
    use framebuffer::FRAMEBUFFER_SIZE;
    use framebuffer::FramebufferDevice;
    use framebuffer::View;
    use guestmem::MappableGuestMemory;
    use guestmem::MappedMemoryRegion;
    use guestmem::MemoryMapper;
    use pal_async::DefaultDriver;
    use pal_async::DefaultPool;
    use sparse_mmap::alloc_shared_memory;
    use test_with_tracing::test;

    struct NoGuestMappings;

    impl MemoryMapper for NoGuestMappings {
        fn new_region(
            &self,
            _len: usize,
            _debug_name: String,
        ) -> std::io::Result<(Box<dyn MappableGuestMemory>, Arc<dyn MappedMemoryRegion>)> {
            Err(std::io::Error::other(
                "render tests do not map guest memory",
            ))
        }
    }

    fn renderer_fixture(driver: &DefaultDriver) -> (RendererCore, View) {
        let memory = alloc_shared_memory(FRAMEBUFFER_SIZE, "vga-render-test").unwrap();
        let (framebuffer, access) = framebuffer::framebuffer(memory, FRAMEBUFFER_SIZE, 0).unwrap();
        let device = FramebufferDevice::new(Box::new(NoGuestMappings), framebuffer, None).unwrap();
        let control = device.control();
        let vram = control.memory().unwrap();
        (
            RendererCore {
                control,
                timer: PolledTimer::new(driver),
                vram,
            },
            access.view().unwrap(),
        )
    }

    fn graphics_state(
        bits_per_pixel: u8,
        width: u16,
        height: u16,
        line_offset_pixels: u16,
    ) -> GraphicsRenderState {
        GraphicsRenderState {
            bits_per_pixel,
            cur_page_start_offset: 0,
            line_offset_pixels,
            width,
            height,
            pixel_double: false,
            pixel_pan: 0,
            mapping_table: [0; 256],
        }
    }

    fn screenshot(view: &mut View, expected_resolution: (u16, u16)) -> Vec<u32> {
        assert_eq!(view.resolution(), expected_resolution);
        let (width, height) = expected_resolution;
        let mut pixels = vec![0; width as usize * height as usize];
        for (row, line) in pixels.chunks_exact_mut(width as usize).enumerate() {
            view.read_line(row as u16, line.as_mut_bytes());
        }
        pixels
    }

    fn text_state() -> TextRenderState {
        let mut mapping_table = [0; 256];
        mapping_table[..5]
            .copy_from_slice(&[0x00000000, 0x00ff0000, 0x0000ff00, 0x00ffffff, 0x000000ff]);
        TextRenderState {
            text: TextModeState {
                current_text_columns: 80,
                lo_res_text_mode: false,
                mono_text_mode: false,
                character_set_512: false,
                char_set_1: 0,
                char_set_2: 0,
                text_char_width: 8,
                text_char_height: 2,
                text_rows: 1,
                blinking_enabled: false,
                blinking_state: false,
                draw_text_cursor: false,
                cursor_first_scanline: 0,
                cursor_last_scanline: 0,
                cursor_row: 0,
                cursor_col: 0,
                cursor_blink_state: true,
            },
            cur_page_start_offset: 0x1000,
            mapping_table,
        }
    }

    #[test]
    fn rgb565_expands_components_and_skips_scanline_padding() {
        let pool = DefaultPool::new();
        let (mut renderer, mut view) = renderer_fixture(&pool.driver());
        let source = [
            0xf800u16, 0x07e0, 0x001f, 0xffff, 0x1234, 0xabcd, 0, 0x8000, 0x0400, 0x0010,
        ];
        renderer.vram.write_at(0, source.as_bytes()).unwrap();

        renderer.render(&RenderState::Graphics(graphics_state(16, 4, 2, 6)));

        assert_eq!(
            screenshot(&mut view, (4, 2)),
            [
                0x00ff0000, 0x0000ff00, 0x000000ff, 0x00ffffff, 0x00000000, 0x00840000, 0x00008200,
                0x00000084,
            ]
        );
    }

    #[test]
    fn direct_color_uses_source_pitch_and_keeps_vram_live() {
        let pool = DefaultPool::new();
        let (mut renderer, mut view) = renderer_fixture(&pool.driver());
        let source = [
            0x00ff0000u32,
            0x0000ff00,
            0xdeadbeef,
            0x000000ff,
            0x00ffffff,
        ];
        renderer.vram.write_at(0, source.as_bytes()).unwrap();

        renderer.render(&RenderState::Graphics(graphics_state(32, 2, 2, 3)));

        assert_eq!(
            screenshot(&mut view, (2, 2)),
            [0x00ff0000, 0x0000ff00, 0x000000ff, 0x00ffffff]
        );

        renderer.vram.write_plain(12, &0x00123456u32).unwrap();
        assert_eq!(
            screenshot(&mut view, (2, 2)),
            [0x00ff0000, 0x0000ff00, 0x00123456, 0x00ffffff]
        );
    }

    #[test]
    fn text_renders_font_bits_attributes_and_selected_page() {
        let pool = DefaultPool::new();
        let (mut renderer, mut view) = renderer_fixture(&pool.driver());
        renderer.vram.write_at(0x8000, &[0x41, 0x21]).unwrap();
        renderer.vram.write_at(0x8008, &[0x42, 0x34]).unwrap();
        renderer.vram.write_plain(0x2082, &0x81u8).unwrap();
        renderer.vram.write_plain(0x2086, &0x42u8).unwrap();
        renderer.vram.write_plain(0x2102, &0x18u8).unwrap();
        renderer.vram.write_plain(0x2106, &0x24u8).unwrap();
        let state = text_state();

        renderer.render(&RenderState::Text(state.clone()));

        let mut expected = vec![0; 640 * 2];
        expected[..16].copy_from_slice(
            &[1, 2, 2, 2, 2, 2, 2, 1, 3, 3, 3, 4, 4, 3, 3, 3]
                .map(|index| state.mapping_table[index]),
        );
        expected[640..656].copy_from_slice(
            &[2, 1, 2, 2, 2, 2, 1, 2, 3, 3, 4, 3, 3, 4, 3, 3]
                .map(|index| state.mapping_table[index]),
        );
        assert_eq!(screenshot(&mut view, (640, 2)), expected);
    }
}
