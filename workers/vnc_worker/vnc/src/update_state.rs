// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Framebuffer snapshots and dirty detection. Drains device-reported dirty
//! rects when available, falls back to tile-by-tile comparison otherwise.

use crate::DirtyBitmap;
use crate::DirtyRectReceiver;
use crate::Rect;
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
    /// Tile edge length in pixels: the granularity of dirty tracking. Fixed for
    /// the connection's lifetime; the bitmap is (re)built at this size.
    tile_size: u16,
    /// Reusable buffer for merge results.
    merged_rects: Vec<Rect>,
    width: u16,
    height: u16,
    /// Set once device dirty rects have been received. When true, an empty
    /// dirty channel means "nothing changed" and we skip the full VRAM read
    /// and tile diff.
    device_dirty_seen: bool,
}

impl UpdateState {
    pub(crate) fn new(tile_size: u16) -> Self {
        Self {
            cur_fb: Vec::new(),
            prev_fb: Vec::new(),
            pending_dirty: DirtyBitmap::new(0, 0, tile_size),
            tile_size,
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

    /// Change the dirty-tracking tile size, rebuilding the bitmap. The caller
    /// must force a full refresh on the next `collect_dirty` so the whole
    /// framebuffer is re-read and re-sent at the new grid. Used by the `cycle`
    /// measurement mode.
    pub(crate) fn set_tile_size(&mut self, tile_size: u16) {
        self.tile_size = tile_size;
        self.pending_dirty.set_tile_size(tile_size);
    }

    /// Read the framebuffer and determine which rectangles are dirty.
    /// Returns merged dirty rects in pixel coordinates and the source
    /// that provided them (full refresh, device, or tile diff).
    ///
    /// Post-condition: on return cur_fb holds the current complete frame, which
    /// commit() rotates into prev_fb. Every branch below upholds this.
    pub(crate) fn collect_dirty(
        &mut self,
        fb: &mut impl Framebuffer,
        dirty_recv: &mut Option<DirtyRectReceiver>,
        force_full: bool,
        missed_dirty: &Option<Arc<AtomicBool>>,
    ) -> DirtyResult {
        let (width, height) = (self.width, self.height);
        // Compare dimensions, not pixel count: a same-area transpose (e.g.
        // 1024x768 -> 768x1024) keeps width*height constant but changes the row
        // stride, and the partial read below indexes cur_fb with the new width,
        // so an area-only check would index past the old buffer and panic. A
        // dimension change forces a full re-read; other full-refresh reasons
        // leave the dimensions unchanged. The bitmap's dimensions track what
        // cur_fb/prev_fb were last built at.
        let size_mismatch = self.pending_dirty.dimensions() != (width, height);
        let mut full_update = force_full || size_mismatch;
        let mut cur_fb_stale = size_mismatch;

        if size_mismatch {
            // Resolution changed (or first frame): rebuild the bitmap at the new
            // size. Not done for a same-size full refresh, which would re-read
            // the whole framebuffer instead of serving the device-maintained
            // cur_fb; those branches mark_all for their own output.
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

        // A dropped dirty broadcast (our channel was full) forces a full
        // refresh to prevent permanently stale regions.
        if let Some(missed) = missed_dirty {
            if missed.swap(false, Ordering::Relaxed) {
                full_update = true;
                // A dropped broadcast left gaps in cur_fb, so re-read in full
                // rather than serving the incomplete copy.
                cur_fb_stale = true;
                // A dropped broadcast means the device is producing dirt, so
                // mark it as driving cur_fb (otherwise a client recovering only
                // via this path reverts to the full tile-diff scan each idle
                // cycle).
                self.device_dirty_seen = true;
                tracing::debug!("missed dirty broadcast, forcing full refresh");
            }
        }

        let source = if cur_fb_stale {
            // cur_fb is unusable (first frame, resolution change, or a dropped
            // broadcast left gaps): re-read the whole framebuffer.
            self.full_refresh_from_vram(fb);
            DirtySource::Full
        } else if got_device_dirty || (full_update && self.device_dirty_seen) {
            // Bring the complete previous frame into cur_fb so non-dirty regions
            // are already correct, then read only this cycle's dirty columns. A
            // client-requested full refresh lands here too when the device keeps
            // cur_fb current, served without re-reading all of VRAM.
            if full_update {
                // The whole frame goes out via mark_all_merged below, so drive
                // the read straight from the dirty tiles.
                self.pending_dirty.dirty_tiles_into(&mut self.merged_rects);
            } else {
                // Device-only update: the merged rects are both the read driver
                // and the wire output.
                self.pending_dirty.merge_into(&mut self.merged_rects);
            }
            std::mem::swap(&mut self.cur_fb, &mut self.prev_fb);
            for r in &self.merged_rects {
                for y in r.y..r.y + r.h {
                    let row = y as usize * width as usize;
                    let start = row + r.x as usize;
                    let end = start + r.w as usize;
                    fb.read_line(y, r.x, self.cur_fb[start..end].as_mut_bytes());
                }
            }
            if full_update {
                // Client asked for a full frame: send the whole up-to-date
                // buffer.
                self.mark_all_merged();
                DirtySource::Full
            } else {
                DirtySource::Device
            }
        } else if full_update {
            // Full refresh with no device-maintained buffer (e.g. the tile-diff
            // fallback): re-read the whole framebuffer.
            self.full_refresh_from_vram(fb);
            DirtySource::Full
        } else if self.device_dirty_seen {
            // Device supports dirty rects but sent nothing this cycle: nothing
            // changed, so skip the VRAM read. The current frame is in prev_fb;
            // swap it into cur_fb to uphold the post-condition (commit()'s swap
            // then restores prev_fb = current). Without this swap an idle cycle
            // inverts the buffers and the next cycle serves a stale frame.
            std::mem::swap(&mut self.cur_fb, &mut self.prev_fb);
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
        // Swap out the merged rects so the caller owns them.
        let mut rects = Vec::new();
        std::mem::swap(&mut rects, &mut self.merged_rects);
        DirtyResult { rects, source }
    }

    /// Re-read all of VRAM into cur_fb and emit the whole screen as dirty.
    fn full_refresh_from_vram(&mut self, fb: &mut impl Framebuffer) {
        self.read_full_framebuffer(fb);
        self.mark_all_merged();
    }

    /// Mark the whole screen dirty and merge it into the output rects. The
    /// caller has already brought cur_fb up to date.
    fn mark_all_merged(&mut self) {
        self.pending_dirty.mark_all();
        self.pending_dirty.merge_into(&mut self.merged_rects);
    }

    /// Read the entire framebuffer into cur_fb.
    fn read_full_framebuffer(&mut self, fb: &mut impl Framebuffer) {
        let fb_size = self.width as usize * self.height as usize;
        self.cur_fb.resize(fb_size, 0);
        for y in 0..self.height {
            let offset = y as usize * self.width as usize;
            fb.read_line(
                y,
                0,
                self.cur_fb[offset..offset + self.width as usize].as_mut_bytes(),
            );
        }
    }

    /// Compare cur_fb against prev_fb tile-by-tile and mark changed tiles
    /// in pending_dirty.
    fn tile_diff(&mut self) {
        let (width, height) = (self.width, self.height);
        let tile_size = self.tile_size;
        let mut ty: u16 = 0;
        while ty < height {
            let tile_h = tile_size.min(height - ty);
            let mut tx: u16 = 0;
            while tx < width {
                let tile_w = tile_size.min(width - tx);
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
                    self.pending_dirty.set_tile(tx / tile_size, ty / tile_size);
                }
                tx += tile_size;
            }
            ty += tile_size;
        }
    }

    /// Hand back a used rects Vec so its allocation can be reused next cycle.
    pub(crate) fn recycle_rects(&mut self, rects: Vec<Rect>) {
        self.merged_rects = rects;
        self.merged_rects.clear();
    }

    /// Swap cur_fb into prev_fb for next cycle's comparison baseline.
    ///
    /// Relies on collect_dirty's post-condition that cur_fb holds the current
    /// complete frame; after the swap prev_fb holds it and cur_fb becomes
    /// scratch.
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
        /// Pixels read via read_line since the last reset. Lets tests assert
        /// how much of VRAM a collect_dirty cycle actually touched.
        pixels_read: usize,
    }

    impl MockFramebuffer {
        fn new(width: u16, height: u16, fill: u32) -> Self {
            Self {
                pixels: vec![fill; width as usize * height as usize],
                width,
                height,
                pixels_read: 0,
            }
        }

        /// Set a single pixel.
        fn set(&mut self, x: u16, y: u16, color: u32) {
            self.pixels[y as usize * self.width as usize + x as usize] = color;
        }

        /// Simulate a guest resolution change: reshape the backing store to new
        /// dimensions, filled with `fill`.
        fn set_dimensions(&mut self, width: u16, height: u16, fill: u32) {
            self.width = width;
            self.height = height;
            self.pixels = vec![fill; width as usize * height as usize];
        }
    }

    impl Framebuffer for MockFramebuffer {
        fn resolution(&mut self) -> (u16, u16) {
            (self.width, self.height)
        }

        fn read_line(&mut self, line: u16, x: u16, data: &mut [u8]) {
            let start = line as usize * self.width as usize + x as usize;
            let pixels = data.len() / 4;
            data.copy_from_slice(self.pixels[start..start + pixels].as_bytes());
            self.pixels_read += pixels;
        }
    }

    #[test]
    fn update_state_first_frame_is_full() {
        let mut fb = MockFramebuffer::new(32, 32, 0);
        let mut state = UpdateState::new(16);
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
        let mut state = UpdateState::new(16);
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
        let mut state = UpdateState::new(16);
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
        let mut state = UpdateState::new(16);
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
    fn full_refresh_serves_current_device_buffer_without_vram_reread() {
        // Once the synth device is driving cur_fb, a client-requested full
        // refresh serves the current buffer instead of re-reading all of VRAM.
        // Proof: a VRAM change with no dirty rect must NOT appear in the served
        // frame (a full re-read would have picked it up).
        let mut fb = MockFramebuffer::new(32, 32, 0x11111111);
        let mut state = UpdateState::new(16);
        state.set_resolution(32, 32);

        // First frame (full read), then a device-dirty cycle so the device is
        // marked as driving cur_fb.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();
        let (tx, rx) = async_channel::bounded(4);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]));
        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);
        fb.set(5, 5, 0x22222222); // inside the reported region
        let c2 = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        // Device path ran, so the device is now marked as driving cur_fb.
        assert_eq!(c2.source, DirtySource::Device);
        state.commit();

        // Client asks for a full refresh with no new dirt: the worker serves the
        // device-maintained cur_fb and reads no VRAM at all.
        fb.pixels_read = 0;
        let result = state.collect_dirty(&mut fb, &mut dirty_recv, true, &None);
        assert_eq!(result.source, DirtySource::Full);
        assert_eq!(fb.pixels_read, 0);
        state.commit();
    }

    #[test]
    fn full_refresh_after_idle_serves_current_frame() {
        // Regression: an idle cycle (device dirty seen, nothing reported) must
        // not invert cur_fb/prev_fb, or a following client full refresh serves
        // a stale buffer. Both buffers must be full-size for the inversion to
        // surface (a half-sized buffer trips size_mismatch), so prime both with
        // two full reads first.
        let mut fb = MockFramebuffer::new(32, 32, 0x11111111);
        let mut state = UpdateState::new(16);
        state.set_resolution(32, 32);

        // Two full reads size both the cur_fb and prev_fb buffers.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Device-dirty cycle: report a region and change a pixel inside it so the
        // current frame differs from the primed fill.
        let (tx, rx) = async_channel::bounded(4);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]));
        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);
        fb.set(5, 5, 0x22222222);
        let c = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(c.source, DirtySource::Device);
        state.commit();

        // Idle cycle: device seen, nothing reported.
        let idle = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(idle.source, DirtySource::Device);
        assert!(idle.rects.is_empty());
        state.commit();

        // Client full refresh with no new dirt: serve the current frame from
        // cur_fb without re-reading VRAM, and that frame must carry the earlier
        // change rather than a stale buffer.
        fb.pixels_read = 0;
        let result = state.collect_dirty(&mut fb, &mut dirty_recv, true, &None);
        assert_eq!(result.source, DirtySource::Full);
        assert_eq!(
            fb.pixels_read, 0,
            "full refresh re-read VRAM instead of serving cur_fb"
        );
        assert_eq!(
            state.cur_fb[5 * 32 + 5],
            0x22222222,
            "full refresh after idle served a stale buffer"
        );
        assert_eq!(state.cur_fb[0], 0x11111111);
        state.commit();
    }

    #[test]
    fn resolution_transpose_after_device_dirty_no_panic() {
        // A guest resolution change that preserves pixel area but transposes
        // dimensions (32x64 -> 64x32) must re-read at the new geometry, not take
        // the device partial-read path. An area-based staleness check would
        // index the old 32-wide buffer with the new stride 64 and panic.
        let mut fb = MockFramebuffer::new(32, 64, 0x11111111);
        let mut state = UpdateState::new(16);
        state.set_resolution(32, 64);

        let (tx, rx) = async_channel::bounded(8);
        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);

        // First frame, then a device-dirty cycle so device_dirty_seen is set.
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 0,
            right: 16,
            bottom: 16,
        }]));
        let c = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(c.source, DirtySource::Device);
        state.commit();

        // Transpose to 64x32 (same area, wider line) with a bottom-region dirty
        // rect that only exists in the old 64-tall geometry. The area-based
        // check would index cur_fb[63*64+..] past the 2048-long buffer -> panic.
        fb.set_dimensions(64, 32, 0x22222222);
        state.set_resolution(64, 32);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 0,
            top: 48,
            right: 16,
            bottom: 64,
        }]));
        let result = state.collect_dirty(&mut fb, &mut dirty_recv, true, &None);
        assert_eq!(result.source, DirtySource::Full);
        assert_eq!(state.cur_fb.len(), 64 * 32);
        assert_eq!(state.cur_fb[0], 0x22222222);
        state.commit();
    }

    #[test]
    fn update_state_device_dirty_reads_only_dirty_columns() {
        let mut fb = MockFramebuffer::new(64, 32, 0);
        let mut state = UpdateState::new(16);
        state.set_resolution(64, 32);

        // First frame establishes prev_fb (all zero).
        let _ = state.collect_dirty(&mut fb, &mut None, true, &None);
        state.commit();

        // Device reports only the tile at columns [32, 48) of rows [0, 16).
        let (tx, rx) = async_channel::bounded(4);
        let _ = tx.try_send(Arc::new(vec![video_core::DirtyRect {
            left: 32,
            top: 0,
            right: 48,
            bottom: 16,
        }]));
        let mut dirty_recv: Option<DirtyRectReceiver> = Some(rx);

        // Change one pixel inside the dirty region and one outside it on the
        // same row. Only the inside one should be read.
        fb.set(40, 5, 0x00aabbcc); // inside [32, 48)
        fb.set(5, 5, 0x00112233); // outside the dirty rect

        let result = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(result.source, DirtySource::Device);

        // The inside pixel was read from VRAM.
        assert_eq!(state.cur_fb[5 * 64 + 40], 0x00aabbcc);
        // The outside pixel was not read (we only read the dirty columns), so
        // it keeps the previous frame's value from the cur_fb/prev_fb swap.
        assert_eq!(state.cur_fb[5 * 64 + 5], 0);
    }

    #[test]
    fn update_state_prev_fb_valid_after_device_dirty() {
        // Verify that after a device-dirty cycle, prev_fb is complete
        // (non-dirty regions preserved) so a subsequent tile-diff works.
        let mut fb = MockFramebuffer::new(32, 32, 0xAAAAAAAA);
        let mut state = UpdateState::new(16);
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

        // Third cycle: device dirty was seen, so an empty channel means
        // "nothing changed" and skips the VRAM read.
        let result = state.collect_dirty(&mut fb, &mut dirty_recv, false, &None);
        assert_eq!(result.source, DirtySource::Device);
        assert!(
            result.rects.is_empty(),
            "idle cycle should produce no dirty rects"
        );
        state.commit();
    }
}
