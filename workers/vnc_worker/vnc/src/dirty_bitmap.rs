// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dirty tile bitmap for tracking framebuffer regions that need updating.

use bitvec::prelude::*;

/// 16x16 pixel tiles balance precision vs overhead for dirty tracking.
pub(crate) const TILE_SIZE: u16 = 16;

/// A rectangle in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge, in pixels.
    pub x: u16,
    /// Top edge, in pixels.
    pub y: u16,
    /// Width, in pixels.
    pub w: u16,
    /// Height, in pixels.
    pub h: u16,
}

/// Bitmap that tracks which 16x16 pixel tiles of the framebuffer are dirty
/// (need sending to a VNC client). At 1920x1080, this is 120x68 = 8160 tiles,
/// fitting in ~1KB of bitmap data.
pub struct DirtyBitmap {
    bits: BitVec<u64, Lsb0>,
    tiles_per_row: u16,
    tiles_per_col: u16,
    width: u16,
    height: u16,
}

impl DirtyBitmap {
    /// Create a new bitmap for the given framebuffer dimensions. All tiles
    /// start dirty so the first update sends the full screen.
    pub fn new(width: u16, height: u16) -> Self {
        let tiles_per_row = width.div_ceil(TILE_SIZE);
        let tiles_per_col = height.div_ceil(TILE_SIZE);
        let total_tiles = tiles_per_row as usize * tiles_per_col as usize;
        // BitVec is sized to exactly `total_tiles`, so there are no padding
        // bits to mask off after setting everything dirty.
        let bits = bitvec![u64, Lsb0; 1; total_tiles];
        Self {
            bits,
            tiles_per_row,
            tiles_per_col,
            width,
            height,
        }
    }

    /// Resize the bitmap for a new framebuffer resolution. Marks everything
    /// dirty since the client needs a full refresh after a resolution change.
    pub fn resize(&mut self, width: u16, height: u16) {
        *self = Self::new(width, height);
    }

    /// Mark the tiles overlapping a pixel rectangle as dirty. Coordinates use
    /// i32 because the synthetic video protocol::Rectangle uses i32. Values
    /// outside the framebuffer are clamped.
    pub fn mark_rect(&mut self, left: i32, top: i32, right: i32, bottom: i32) {
        // Clamp to framebuffer bounds. Clamp to u16 range before cast to
        // avoid wrapping (e.g. i32 70000 would wrap to 4464 as u16).
        let left = left.clamp(0, self.width as i32) as u16;
        let top = top.clamp(0, self.height as i32) as u16;
        let right = right.clamp(0, self.width as i32) as u16;
        let bottom = bottom.clamp(0, self.height as i32) as u16;

        if left >= right || top >= bottom {
            return;
        }

        let tile_left = left / TILE_SIZE;
        // Subtract 1 before dividing so that a rect ending exactly on a tile
        // boundary doesn't spill into the next tile row/column.
        let tile_right = (right - 1) / TILE_SIZE;
        let tile_top = top / TILE_SIZE;
        let tile_bottom = (bottom - 1) / TILE_SIZE;

        for ty in tile_top..=tile_bottom {
            let row_start = ty as usize * self.tiles_per_row as usize;
            let first = row_start + tile_left as usize;
            let last = row_start + tile_right as usize;
            self.bits[first..=last].fill(true);
        }
    }

    /// Set a single tile's dirty bit directly by tile coordinates.
    /// Skips the clamping and division in `mark_rect` — use when the
    /// caller already knows the tile index (e.g., tile_diff).
    pub fn set_tile(&mut self, tile_x: u16, tile_y: u16) {
        debug_assert!(
            tile_x < self.tiles_per_row && tile_y < self.tiles_per_col,
            "tile ({}, {}) out of bounds ({}x{})",
            tile_x,
            tile_y,
            self.tiles_per_row,
            self.tiles_per_col
        );
        let idx = tile_y as usize * self.tiles_per_row as usize + tile_x as usize;
        if idx < self.bits.len() {
            self.bits.set(idx, true);
        }
    }

    /// Mark every tile dirty (e.g. for a full screen refresh request).
    pub fn mark_all(&mut self) {
        self.bits.fill(true);
    }

    /// Clear all dirty bits (nothing needs updating).
    pub fn clear(&mut self) {
        self.bits.fill(false);
    }

    /// Returns true if no tiles are dirty.
    pub fn is_empty(&self) -> bool {
        !self.bits.any()
    }

    /// Accumulate dirty regions from another bitmap: `self |= other`.
    /// Both bitmaps must have the same dimensions.
    pub fn or_from(&mut self, other: &DirtyBitmap) {
        debug_assert_eq!(self.bits.len(), other.bits.len());
        self.bits |= &other.bits;
    }

    /// Accumulate dirty regions from source, then clear the source:
    /// `self |= source; source.clear()`. Useful for moving a shared dirty
    /// accumulator into a per-client bitmap.
    pub fn take_from(&mut self, source: &mut DirtyBitmap) {
        debug_assert_eq!(self.bits.len(), source.bits.len());
        self.bits |= &source.bits;
        source.bits.fill(false);
    }

    /// Iterate over individual dirty tiles, yielding `(x, y, w, h)` in pixel
    /// coordinates. Width/height may be smaller than TILE_SIZE at the right
    /// and bottom edges of the framebuffer.
    pub fn iter_dirty_tiles(&self) -> impl Iterator<Item = Rect> + '_ {
        let tiles_per_row = self.tiles_per_row;
        let width = self.width;
        let height = self.height;
        self.bits.iter_ones().map(move |idx| {
            let tx = (idx % tiles_per_row as usize) as u16;
            let ty = (idx / tiles_per_row as usize) as u16;
            let x = tx * TILE_SIZE;
            let y = ty * TILE_SIZE;
            // Edge tiles may be narrower/shorter than TILE_SIZE.
            let w = TILE_SIZE.min(width - x);
            let h = TILE_SIZE.min(height - y);
            Rect { x, y, w, h }
        })
    }

    /// Merges adjacent dirty tiles into larger rectangles. Returns a new
    /// `Vec<Rect>` in pixel coordinates. See `merge_into` for algorithm
    /// details.
    pub fn merge_dirty_rects(&self) -> Vec<Rect> {
        let mut out = Vec::new();
        self.merge_into(&mut out);
        out
    }

    /// Like `merge_dirty_rects` but appends into a caller-provided Vec,
    /// reusing its allocation across update cycles. Clears `out` first.
    ///
    /// Algorithm: QEMU-style single-pass greedy merge. Walk the bitmap
    /// row-by-row, left-to-right; when a dirty tile is found, extend right
    /// to the end of its horizontal run, then extend down as long as every
    /// column in the run stays dirty in the next row. Emit one rectangle
    /// covering the full area, clear those tiles in a scratch copy so they
    /// aren't re-emitted, and continue.
    ///
    /// Compared with a two-pass row-then-column merge this produces fewer
    /// rectangles for shapes where a wide row is bracketed by narrower
    /// rows of the same horizontal extent (the vertical scan absorbs the
    /// bracket into the wider rect), at the cost of cloning the bitmap
    /// once per call (~1KB for a 1080p framebuffer).
    pub fn merge_into(&self, out: &mut Vec<Rect>) {
        out.clear();
        let mut scratch = self.bits.clone();
        let tiles_per_row = self.tiles_per_row as usize;
        let tiles_per_col = self.tiles_per_col as usize;

        for ty in 0..tiles_per_col {
            let row_base = ty * tiles_per_row;
            let mut tx = 0;
            while tx < tiles_per_row {
                if !scratch[row_base + tx] {
                    tx += 1;
                    continue;
                }
                // Find the horizontal run starting at `tx`.
                let run_start = tx;
                let mut run_end = tx + 1;
                while run_end < tiles_per_row && scratch[row_base + run_end] {
                    run_end += 1;
                }
                // Extend downward while every column in the run stays dirty.
                let mut h_tiles = 1usize;
                'extend: while ty + h_tiles < tiles_per_col {
                    let below = (ty + h_tiles) * tiles_per_row;
                    for c in run_start..run_end {
                        if !scratch[below + c] {
                            break 'extend;
                        }
                    }
                    h_tiles += 1;
                }
                // Emit in pixel coordinates. Compute in u32 so multiplying
                // a full-width tile index by TILE_SIZE can't overflow u16
                // before we clamp to `self.width` / `self.height`.
                let x_u32 = run_start as u32 * TILE_SIZE as u32;
                let y_u32 = ty as u32 * TILE_SIZE as u32;
                let right_u32 = (run_end as u32 * TILE_SIZE as u32).min(self.width as u32);
                let bottom_u32 = ((ty + h_tiles) as u32 * TILE_SIZE as u32).min(self.height as u32);
                out.push(Rect {
                    x: x_u32 as u16,
                    y: y_u32 as u16,
                    w: (right_u32 - x_u32) as u16,
                    h: (bottom_u32 - y_u32) as u16,
                });
                // Clear the covered tiles so later iterations skip them.
                for dy in 0..h_tiles {
                    let row = (ty + dy) * tiles_per_row;
                    scratch[row + run_start..row + run_end].fill(false);
                }
                tx = run_end;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_marks_all_dirty() {
        let bm = DirtyBitmap::new(320, 240);
        // 320/16=20 cols, 240/16=15 rows = 300 tiles, all dirty
        assert!(!bm.is_empty());
        let count = bm.iter_dirty_tiles().count();
        assert_eq!(count, 20 * 15);
    }

    #[test]
    fn test_clear_makes_empty() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        assert!(bm.is_empty());
        assert_eq!(bm.iter_dirty_tiles().count(), 0);
    }

    #[test]
    fn test_mark_single_pixel_dirties_one_tile() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // Mark a single pixel in the middle of the framebuffer.
        bm.mark_rect(100, 100, 101, 101);
        assert_eq!(bm.iter_dirty_tiles().count(), 1);
        let r = bm.iter_dirty_tiles().next().unwrap();
        // Pixel 100 falls in tile column 100/16=6, row 100/16=6
        assert_eq!(r.x, 6 * 16);
        assert_eq!(r.y, 6 * 16);
        assert_eq!(r.w, 16);
        assert_eq!(r.h, 16);
    }

    #[test]
    fn test_mark_rect_spanning_multiple_tiles() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // Rect from pixel (0,0) to (33,33) should span tiles (0,0)..(2,2) = 9 tiles.
        // tile cols: 0..32 and 32..33 => cols 0,1,2
        // tile rows: 0..32 and 32..33 => rows 0,1,2
        bm.mark_rect(0, 0, 33, 33);
        let tiles: Vec<_> = bm.iter_dirty_tiles().collect();
        // 3 columns x 3 rows = 9 tiles
        assert_eq!(tiles.len(), 9);
    }

    #[test]
    fn test_mark_rect_clamps_to_bounds() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // Negative and out-of-bounds coordinates should be clamped.
        bm.mark_rect(-10, -10, 500, 500);
        // Should have dirtied every tile, same as mark_all.
        let all_count = 20 * 15;
        assert_eq!(bm.iter_dirty_tiles().count(), all_count);
    }

    #[test]
    fn test_merge_adjacent_tiles_horizontally() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // Dirty three adjacent tiles in row 0: columns 1, 2, 3
        bm.mark_rect(16, 0, 17, 1); // tile (1,0)
        bm.mark_rect(32, 0, 33, 1); // tile (2,0)
        bm.mark_rect(48, 0, 49, 1); // tile (3,0)
        let rects = bm.merge_dirty_rects();
        // Should merge into a single rect spanning 3 tiles.
        assert_eq!(rects.len(), 1);
        assert_eq!(
            rects[0],
            Rect {
                x: 16,
                y: 0,
                w: 48,
                h: 16
            }
        );
    }

    #[test]
    fn test_resize_marks_all_dirty() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        assert!(bm.is_empty());
        bm.resize(640, 480);
        assert!(!bm.is_empty());
        // 640/16=40 cols, 480/16=30 rows = 1200 tiles
        assert_eq!(bm.iter_dirty_tiles().count(), 40 * 30);
    }

    #[test]
    fn test_take_from_clears_source() {
        let mut src = DirtyBitmap::new(320, 240);
        src.clear();
        src.mark_rect(0, 0, 16, 16); // dirty tile (0,0)

        let mut dst = DirtyBitmap::new(320, 240);
        dst.clear();

        dst.take_from(&mut src);

        // Source should now be empty.
        assert!(src.is_empty());
        // Destination should have the tile from the source.
        assert_eq!(dst.iter_dirty_tiles().count(), 1);
    }

    #[test]
    fn test_or_from_accumulates() {
        let mut a = DirtyBitmap::new(320, 240);
        a.clear();
        a.mark_rect(0, 0, 16, 16); // tile (0,0)

        let mut b = DirtyBitmap::new(320, 240);
        b.clear();
        b.mark_rect(16, 0, 32, 16); // tile (1,0)

        a.or_from(&b);

        // `a` should now have both tiles dirty.
        assert_eq!(a.iter_dirty_tiles().count(), 2);
        // `b` should be unchanged.
        assert_eq!(b.iter_dirty_tiles().count(), 1);
    }

    #[test]
    fn test_merge_adjacent_tiles_vertically() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // Dirty a 2x3 block of tiles starting at tile (1,1).
        bm.mark_rect(16, 16, 48, 64);
        let rects = bm.merge_dirty_rects();
        // Horizontal merge produces 2-tile-wide spans on 3 rows.
        // Vertical merge collapses them into one rect.
        assert_eq!(rects.len(), 1);
        assert_eq!(
            rects[0],
            Rect {
                x: 16,
                y: 16,
                w: 32,
                h: 48
            }
        );
    }

    #[test]
    fn test_merge_does_not_merge_different_widths() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // Row 0: tiles 0,1 (32px wide)
        bm.mark_rect(0, 0, 32, 16);
        // Row 1: tiles 0,1,2 (48px wide) - different width, should NOT merge vertically
        bm.mark_rect(0, 16, 48, 32);
        let rects = bm.merge_dirty_rects();
        assert_eq!(rects.len(), 2);
    }

    #[test]
    fn test_merge_empty_bitmap() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        let rects = bm.merge_dirty_rects();
        assert!(rects.is_empty());
    }

    #[test]
    fn test_mark_zero_area_rect_is_noop() {
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // left == right: zero width
        bm.mark_rect(10, 10, 10, 20);
        assert!(bm.is_empty());
        // top == bottom: zero height
        bm.mark_rect(10, 10, 20, 10);
        assert!(bm.is_empty());
    }

    #[test]
    fn test_merge_same_column_with_vertical_gap() {
        // Two dirty tile stacks at the same (x, w) separated by a clean row
        // must produce two rects. This guards against the Pass-2 (x, w)
        // index incorrectly extending a closed rect across the gap.
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        // Row 0 dirty at tile (1, 0).
        bm.mark_rect(16, 0, 32, 16);
        // Row 1 clean.
        // Row 2 dirty at tile (1, 2).
        bm.mark_rect(16, 32, 32, 48);
        let rects = bm.merge_dirty_rects();
        assert_eq!(rects.len(), 2);
        // Order: whichever came first in top-to-bottom scan.
        let mut sorted = rects.clone();
        sorted.sort_by_key(|r| (r.y, r.x));
        assert_eq!(
            sorted[0],
            Rect {
                x: 16,
                y: 0,
                w: 16,
                h: 16
            }
        );
        assert_eq!(
            sorted[1],
            Rect {
                x: 16,
                y: 32,
                w: 16,
                h: 16
            }
        );
    }

    #[test]
    fn test_merge_bump_shape_greedy_vertical_absorb() {
        // Shape (first 3 tile columns, 3 tile rows):
        //   XX.
        //   XXX   <- "bump" tile at column 2
        //   XX.
        // The single-pass greedy extends the 2-tile column down through
        // all three rows and leaves the bump as its own 1x1 rect: 2 rects.
        // A two-pass row-then-column merge would produce 3 (each row a
        // separate span with different width, no vertical merge possible).
        let mut bm = DirtyBitmap::new(320, 240);
        bm.clear();
        bm.mark_rect(0, 0, 32, 16); // row 0: tiles (0,0), (1,0)
        bm.mark_rect(0, 16, 48, 32); // row 1: tiles (0,1), (1,1), (2,1)
        bm.mark_rect(0, 32, 32, 48); // row 2: tiles (0,2), (1,2)
        let rects = bm.merge_dirty_rects();
        assert_eq!(rects.len(), 2);
        let mut sorted = rects.clone();
        sorted.sort_by_key(|r| (r.y, r.x));
        // The 2-tile column spanning all three rows.
        assert_eq!(
            sorted[0],
            Rect {
                x: 0,
                y: 0,
                w: 32,
                h: 48,
            }
        );
        // The bump tile on the right in row 1.
        assert_eq!(
            sorted[1],
            Rect {
                x: 32,
                y: 16,
                w: 16,
                h: 16,
            }
        );
    }

    #[test]
    fn test_merge_checkerboard_produces_one_rect_per_tile() {
        // Checkerboard of single dirty tiles — the pathological input for
        // the old reverse-linear Pass 2. Every dirty tile is isolated, so
        // no horizontal or vertical merging can happen; output must have
        // exactly one rect per dirty tile.
        let mut bm = DirtyBitmap::new(320, 240); // 20x15 tile grid
        bm.clear();
        let mut expected = 0;
        for ty in 0..15u16 {
            for tx in 0..20u16 {
                if (tx + ty) % 2 == 0 {
                    let x = tx * 16;
                    let y = ty * 16;
                    bm.mark_rect(x as i32, y as i32, (x + 1) as i32, (y + 1) as i32);
                    expected += 1;
                }
            }
        }
        let rects = bm.merge_dirty_rects();
        assert_eq!(rects.len(), expected);
        // Every rect is a single 16x16 tile.
        for r in &rects {
            assert_eq!(r.w, 16);
            assert_eq!(r.h, 16);
        }
    }

    #[test]
    fn test_edge_tiles_have_correct_size() {
        // 17x17 framebuffer: 2x2 tiles, but the rightmost column and bottom
        // row tiles are only 1 pixel wide/tall.
        let bm = DirtyBitmap::new(17, 17);
        let tiles: Vec<_> = bm.iter_dirty_tiles().collect();
        assert_eq!(tiles.len(), 4);
        // Sort by (y, x) for deterministic order.
        let mut sorted = tiles.clone();
        sorted.sort_by_key(|r| (r.y, r.x));
        assert_eq!(
            sorted[0],
            Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 16
            }
        );
        assert_eq!(
            sorted[1],
            Rect {
                x: 16,
                y: 0,
                w: 1,
                h: 16
            }
        );
        assert_eq!(
            sorted[2],
            Rect {
                x: 0,
                y: 16,
                w: 16,
                h: 1
            }
        );
        assert_eq!(
            sorted[3],
            Rect {
                x: 16,
                y: 16,
                w: 1,
                h: 1
            }
        );
    }
}
