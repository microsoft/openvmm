// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Framebuffer snapshots and dirty detection. Drains device-reported dirty
//! rects when available, falls back to tile-by-tile comparison otherwise.

use crate::DirtyBitmap;
use crate::DirtyRectReceiver;
use crate::Rect;
use crate::TILE_SIZE;
use crate::traits::Framebuffer;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use zerocopy::IntoBytes;

/// How dirty regions were determined this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirtySource {
    /// Full screen refresh (first frame, resolution change, client request).
    Full,
    /// Dirty rects provided by the guest video driver.
    Device,
    /// Tile-by-tile comparison against previous frame (fallback).
    Diff,
}

impl DirtySource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Device => "device",
            Self::Diff => "diff",
        }
    }
}

/// Result of a dirty collection cycle.
pub(crate) struct DirtyResult {
    pub(crate) rects: Vec<Rect>,
    pub(crate) source: DirtySource,
}

/// Tracks framebuffer state for determining which regions need updating.
pub(crate) struct UpdateState {
    pub(crate) cur_fb: Vec<u32>,
    pub(crate) prev_fb: Vec<u32>,
    pending_dirty: DirtyBitmap,
    /// Reusable buffer for merge results, avoids allocation per cycle.
    merged_rects: Vec<Rect>,
    width: u16,
    height: u16,
    /// Set once device dirty rects have been received. When true, an empty
    /// dirty channel means "nothing changed" and we skip the expensive full
    /// VRAM read and tile diff that would otherwise run every 30ms.
    device_dirty_seen: bool,
}

impl UpdateState {
    pub(crate) fn new() -> Self {
        Self {
            cur_fb: Vec::new(),
            prev_fb: Vec::new(),
            pending_dirty: DirtyBitmap::new(0, 0),
            merged_rects: Vec::new(),
            width: 0,
            height: 0,
            device_dirty_seen: false,
        }
    }

    /// Update resolution tracking when the framebuffer size changes.
    pub(crate) fn set_resolution(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Read the framebuffer and determine which rectangles are dirty.
    /// Returns merged dirty rects in pixel coordinates and the source
    /// that provided them (full refresh, device, or tile diff).
    pub(crate) fn collect_dirty(
        &mut self,
        fb: &mut impl Framebuffer,
        dirty_recv: &mut Option<DirtyRectReceiver>,
        force_full: bool,
        missed_dirty: &Option<Arc<AtomicBool>>,
    ) -> DirtyResult {
        let (width, height) = (self.width, self.height);
        let fb_size = width as usize * height as usize;
        let mut full_update = force_full || self.prev_fb.len() != fb_size;

        if full_update {
            self.pending_dirty.resize(width, height);
        }

        // Drain any device-reported dirty rects into our pending bitmap.
        let mut got_device_dirty = false;
        if let Some(recv) = dirty_recv {
            loop {
                match recv.try_recv() {
                    Ok(rects) => {
                        for r in rects.iter() {
                            self.pending_dirty
                                .mark_rect(r.left, r.top, r.right, r.bottom);
                        }
                        got_device_dirty = true;
                    }
                    Err(async_channel::TryRecvError::Empty) => break,
                    Err(async_channel::TryRecvError::Closed) => {
                        // Channel closed (upstream video device reset or
                        // coordinator dropped senders). Reset to tile diff
                        // and stop polling the dead channel.
                        if self.device_dirty_seen {
                            tracing::info!("dirty channel closed, falling back to tile diff");
                            self.device_dirty_seen = false;
                        }
                        *dirty_recv = None;
                        break;
                    }
                }
            }
        }
        if got_device_dirty {
            self.device_dirty_seen = true;
        }

        // If the coordinator flagged that it dropped a dirty broadcast
        // because our channel was full, force a full refresh to prevent
        // permanently stale regions.
        if let Some(missed) = missed_dirty {
            if missed.swap(false, Ordering::Relaxed) {
                full_update = true;
                tracing::debug!("missed dirty broadcast, forcing full refresh");
            }
        }

        let source = if full_update {
            self.pending_dirty.mark_all();
            self.read_full_framebuffer(fb);
            self.pending_dirty.merge_into(&mut self.merged_rects);
            DirtySource::Full
        } else if got_device_dirty {
            // Merge once: reuse for both partial VRAM reads and final output.
            self.pending_dirty.merge_into(&mut self.merged_rects);
            // Swap prev_fb into cur_fb (O(1) pointer swap) so non-dirty
            // regions are already correct, then overwrite dirty lines.
            std::mem::swap(&mut self.cur_fb, &mut self.prev_fb);
            for r in &self.merged_rects {
                for y in r.y..r.y + r.h {
                    let offset = y as usize * width as usize;
                    fb.read_line(
                        y,
                        self.cur_fb[offset..offset + width as usize].as_mut_bytes(),
                    );
                }
            }
            DirtySource::Device
        } else if self.device_dirty_seen {
            // Device supports dirty rects but sent nothing this cycle --
            // nothing changed. Skip the 8MB VRAM read entirely.
            self.merged_rects.clear();
            DirtySource::Device
        } else {
            // No device dirty support: full read + tile diff (hyperv_fb fallback).
            self.read_full_framebuffer(fb);
            self.tile_diff();
            self.pending_dirty.merge_into(&mut self.merged_rects);
            DirtySource::Diff
        };

        self.pending_dirty.clear();
        // Swap out the merged rects so caller owns them. The empty Vec
        // we swap in will be reused by merge_into next cycle.
        let mut rects = Vec::new();
        std::mem::swap(&mut rects, &mut self.merged_rects);
        DirtyResult { rects, source }
    }

    /// Read the entire framebuffer into cur_fb.
    fn read_full_framebuffer(&mut self, fb: &mut impl Framebuffer) {
        let fb_size = self.width as usize * self.height as usize;
        self.cur_fb.resize(fb_size, 0);
        for y in 0..self.height {
            let offset = y as usize * self.width as usize;
            fb.read_line(
                y,
                self.cur_fb[offset..offset + self.width as usize].as_mut_bytes(),
            );
        }
    }

    /// Compare cur_fb against prev_fb tile-by-tile and mark changed tiles
    /// in pending_dirty.
    fn tile_diff(&mut self) {
        let (width, height) = (self.width, self.height);
        let mut ty: u16 = 0;
        while ty < height {
            let tile_h = TILE_SIZE.min(height - ty);
            let mut tx: u16 = 0;
            while tx < width {
                let tile_w = TILE_SIZE.min(width - tx);
                let mut changed = false;
                for y in ty..ty + tile_h {
                    let start = y as usize * width as usize + tx as usize;
                    if self.cur_fb[start..start + tile_w as usize]
                        != self.prev_fb[start..start + tile_w as usize]
                    {
                        changed = true;
                        break;
                    }
                }
                if changed {
                    // Use set_tile directly — we already know the tile coords,
                    // no need for mark_rect's clamping and division.
                    self.pending_dirty.set_tile(tx / TILE_SIZE, ty / TILE_SIZE);
                }
                tx += TILE_SIZE;
            }
            ty += TILE_SIZE;
        }
    }

    /// Return a used rects Vec so its allocation can be reused next cycle.
    pub(crate) fn recycle_rects(&mut self, rects: Vec<Rect>) {
        // Swap to preserve the Vec's allocation capacity for next cycle.
        self.merged_rects = rects;
        self.merged_rects.clear();
    }

    /// Swap cur_fb into prev_fb for next cycle's comparison baseline.
    pub(crate) fn commit(&mut self) {
        std::mem::swap(&mut self.prev_fb, &mut self.cur_fb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Framebuffer;

    struct MockFramebuffer {
        pixels: Vec<u32>,
        width: u16,
        height: u16,
    }

    impl MockFramebuffer {
        fn new(width: u16, height: u16, fill: u32) -> Self {
            Self {
                pixels: vec![fill; width as usize * height as usize],
                width,
                height,
            }
        }

        /// Set a single pixel.
        fn set(&mut self, x: u16, y: u16, color: u32) {
            self.pixels[y as usize * self.width as usize + x as usize] = color;
        }
    }

    impl Framebuffer for MockFramebuffer {
        fn resolution(&mut self) -> (u16, u16) {
            (self.width, self.height)
        }

        fn read_line(&mut self, line: u16, data: &mut [u8]) {
            let start = line as usize * self.width as usize;
            let end = start + self.width as usize;
            data.copy_from_slice(self.pixels[start..end].as_bytes());
        }
    }

    #[test]
    fn update_state_first_frame_is_full() {
        let mut fb = MockFramebuffer::new(32, 32, 0);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First call with force_full=true: every tile dirty.
        let result = state.collect_dirty(&mut fb, &mut None, true, &None);
        assert_eq!(result.source, DirtySource::Full);
        // 32/16 = 2 tiles per axis = 4 tiles, merged into 1 rect.
        assert!(!result.rects.is_empty());
        let total_pixels: u32 = result.rects.iter().map(|r| r.w as u32 * r.h as u32).sum();
        assert_eq!(total_pixels, 32 * 32);
        state.commit();
    }

    #[test]
    fn update_state_no_change_produces_no_rects() {
        let mut fb = MockFramebuffer::new(32, 32, 0xAABBCCDD);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame: full.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Second frame: nothing changed, should produce no dirty rects.
        let result = state.collect_dirty(&mut fb, &mut None, false, &None);
        assert_eq!(result.source, DirtySource::Diff);
        assert!(result.rects.is_empty());
        state.commit();
    }

    #[test]
    fn update_state_detects_single_pixel_change() {
        let mut fb = MockFramebuffer::new(32, 32, 0);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Change one pixel in tile (1,1).
        fb.set(20, 20, 0xFFFFFFFF);

        let result = state.collect_dirty(&mut fb, &mut None, false, &None);
        assert_eq!(result.source, DirtySource::Diff);
        assert_eq!(result.rects.len(), 1);
        // The dirty rect should cover the tile containing pixel (20,20).
        let r = &result.rects[0];
        assert!(r.x <= 20 && r.x + r.w > 20);
        assert!(r.y <= 20 && r.y + r.h > 20);
        state.commit();
    }

    #[test]
    fn update_state_device_dirty_uses_partial_read() {
        let mut fb = MockFramebuffer::new(32, 32, 0);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Simulate device dirty rect via async-channel.
        let (tx, rx) = async_channel::bounded(4);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]));

        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);
        // Change the pixel so there's actually something different in VRAM.
        fb.set(5, 5, 0x12345678);

        let result = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(result.source, DirtySource::Device);
        assert!(!result.rects.is_empty());
        state.commit();
    }

    #[test]
    fn update_state_prev_fb_valid_after_device_dirty() {
        // Verify that after a device-dirty cycle, prev_fb is complete
        // (non-dirty regions preserved) so a subsequent tile-diff works.
        let mut fb = MockFramebuffer::new(32, 32, 0xAAAAAAAA);
        let mut state = UpdateState::new();
        state.set_resolution(32, 32);

        // First frame: full.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Device-dirty cycle: only tile (0,0) reported dirty.
        let (tx, rx) = async_channel::bounded(4);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]));
        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);
        let _ = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        state.commit();

        // Third cycle: device dirty was seen, so empty channel means
        // "nothing changed" — skips the 8MB VRAM read entirely.
        let result = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(result.source, DirtySource::Device);
        assert!(
            result.rects.is_empty(),
            "idle cycle should produce no dirty rects"
        );
        state.commit();
    }
}
