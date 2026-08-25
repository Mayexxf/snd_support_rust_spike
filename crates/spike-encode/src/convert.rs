//! BGRA to I420.
//!
//! The stage between capture and encoding, and not a free one: libvpx wants
//! planar YUV and desktop duplication only ever hands out BGRA. RustDesk has the
//! same stage (`convert_to_yuv`) and the same obligation to pay for it.
//!
//! Like the copy before it, this walks only the regions that changed. The
//! planes persist between frames, so untouched pixels keep the previous frame's
//! values — which is correct precisely because those pixels did not change.
//!
//! **Chroma forces even alignment.** I420 stores one U and one V sample per 2×2
//! block of pixels, so a region has to be grown outward to even boundaries
//! before conversion. Converting an odd-aligned rectangle would leave a chroma
//! sample half-updated: computed from two new pixels and two stale ones.

use spike_capture::Rect;

/// A frame in I420, the layout libvpx expects.
pub struct I420 {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: u32, height: u32) -> Self {
        // Odd dimensions round up: a 1919-wide frame still needs 960 chroma
        // columns, and truncating would drop the last one.
        let cw = width.div_ceil(2) as usize;
        let ch = height.div_ceil(2) as usize;
        Self {
            width,
            height,
            y: vec![16; width as usize * height as usize],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    pub fn y_stride(&self) -> usize {
        self.width as usize
    }

    pub fn uv_stride(&self) -> usize {
        (self.width as usize).div_ceil(2)
    }

    /// Grow a rectangle outward to even boundaries and clip it to the frame.
    ///
    /// Outward, never inward: shrinking would leave the odd edge column showing
    /// the previous frame.
    fn align(&self, r: Rect) -> Rect {
        Rect {
            left: (r.left.max(0) & !1).min(self.width as i32),
            top: (r.top.max(0) & !1).min(self.height as i32),
            right: ((r.right.max(0) + 1) & !1).min(self.width as i32),
            bottom: ((r.bottom.max(0) + 1) & !1).min(self.height as i32),
        }
    }

    /// Convert the given BGRA regions into the planes.
    ///
    /// Returns the number of pixels converted, which is not the area asked for:
    /// alignment grows it and clipping shrinks it.
    pub fn convert_bgra(&mut self, bgra: &[u8], src_stride: usize, rects: &[Rect]) -> u64 {
        let y_stride = self.y_stride();
        let uv_stride = self.uv_stride();
        let mut converted = 0u64;

        for rect in rects {
            let r = self.align(*rect);
            if r.right <= r.left || r.bottom <= r.top {
                continue;
            }
            let (x0, x1) = (r.left as usize, r.right as usize);
            let (y0, y1) = (r.top as usize, r.bottom as usize);

            // Two rows at a time: a chroma sample covers a 2×2 block, so pairing
            // the rows lets it be produced from pixels already in registers
            // instead of read back a second time.
            let mut row = y0;
            while row < y1 {
                let row2 = (row + 1).min(y1 - 1);
                let top_base = row * src_stride;
                let bot_base = row2 * src_stride;

                let mut x = x0;
                while x < x1 {
                    let x2 = (x + 1).min(x1 - 1);

                    let mut sum_b = 0u32;
                    let mut sum_g = 0u32;
                    let mut sum_r = 0u32;

                    for (base, out_row) in [(top_base, row), (bot_base, row2)] {
                        for col in [x, x2] {
                            let i = base + col * 4;
                            let b = bgra[i] as u32;
                            let g = bgra[i + 1] as u32;
                            let r_ = bgra[i + 2] as u32;
                            sum_b += b;
                            sum_g += g;
                            sum_r += r_;
                            // Studio-swing BT.601, the range every decoder in
                            // this pipeline assumes by default.
                            self.y[out_row * y_stride + col] =
                                (((66 * r_ + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                        }
                    }

                    // Average the block before converting rather than after:
                    // one conversion instead of four, and it is what a correct
                    // downsample does anyway.
                    let (b, g, r_) = (sum_b / 4, sum_g / 4, sum_r / 4);
                    let ci = (row / 2) * uv_stride + x / 2;
                    if ci < self.u.len() {
                        self.u[ci] = ((((112 * b) as i32 - (38 * r_) as i32 - (74 * g) as i32 + 128)
                            >> 8)
                            + 128) as u8;
                        self.v[ci] = ((((112 * r_) as i32 - (94 * g) as i32 - (18 * b) as i32 + 128)
                            >> 8)
                            + 128) as u8;
                    }

                    x += 2;
                }
                row += 2;
            }
            converted += r.area();
        }

        converted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, b: u8, g: u8, r: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width * height {
            v.extend_from_slice(&[b, g, r, 255]);
        }
        v
    }

    fn whole(width: u32, height: u32) -> Rect {
        Rect { left: 0, top: 0, right: width as i32, bottom: height as i32 }
    }

    #[test]
    fn white_and_black_land_on_the_studio_swing_limits() {
        let (w, h) = (16, 16);
        let mut f = I420::new(w, h);

        f.convert_bgra(&solid(w, h, 255, 255, 255), w as usize * 4, &[whole(w, h)]);
        assert!(f.y.iter().all(|&y| y == 235), "белый должен давать Y=235");
        assert!(f.u.iter().all(|&u| u == 128) && f.v.iter().all(|&v| v == 128));

        f.convert_bgra(&solid(w, h, 0, 0, 0), w as usize * 4, &[whole(w, h)]);
        assert!(f.y.iter().all(|&y| y == 16), "чёрный должен давать Y=16");
    }

    #[test]
    fn primaries_land_where_bt601_says() {
        let (w, h) = (8, 8);
        for (b, g, r, y, u, v) in [
            (0u8, 0u8, 255u8, 82u8, 90u8, 240u8),
            (0, 255, 0, 145, 54, 34),
            (255, 0, 0, 41, 240, 110),
        ] {
            let mut f = I420::new(w, h);
            f.convert_bgra(&solid(w, h, b, g, r), w as usize * 4, &[whole(w, h)]);
            assert!((f.y[0] as i32 - y as i32).abs() <= 1, "Y {} против {y}", f.y[0]);
            assert!((f.u[0] as i32 - u as i32).abs() <= 1, "U {} против {u}", f.u[0]);
            assert!((f.v[0] as i32 - v as i32).abs() <= 1, "V {} против {v}", f.v[0]);
        }
    }

    #[test]
    fn untouched_regions_keep_the_previous_frame() {
        let (w, h) = (16, 16);
        let mut f = I420::new(w, h);
        f.convert_bgra(&solid(w, h, 255, 255, 255), w as usize * 4, &[whole(w, h)]);

        // Repaint black, but declare only the left half as changed.
        let left = Rect { left: 0, top: 0, right: 8, bottom: h as i32 };
        f.convert_bgra(&solid(w, h, 0, 0, 0), w as usize * 4, &[left]);

        assert_eq!(f.y[0], 16, "изменившаяся половина должна обновиться");
        // The right half must still hold white — that is the whole point of
        // converting only what changed.
        assert_eq!(f.y[12], 235, "нетронутая половина должна сохраниться");
    }

    #[test]
    fn odd_rectangles_grow_outward_not_inward() {
        let (w, h) = (16, 16);
        let f = I420::new(w, h);
        let aligned = f.align(Rect { left: 3, top: 5, right: 9, bottom: 11 });
        // Left and top round down, right and bottom round up: an odd edge left
        // outside would show the previous frame through a half-updated chroma
        // sample.
        assert_eq!(aligned, Rect { left: 2, top: 4, right: 10, bottom: 12 });
    }

    #[test]
    fn alignment_never_escapes_the_frame() {
        let (w, h) = (15, 15);
        let f = I420::new(w, h);
        let aligned = f.align(Rect { left: -4, top: -1, right: 99, bottom: 99 });
        assert_eq!(aligned, Rect { left: 0, top: 0, right: 15, bottom: 15 });
        // Odd dimensions still need the trailing chroma column and row.
        assert_eq!(f.uv_stride(), 8);
        assert_eq!(f.u.len(), 8 * 8);
    }

    #[test]
    fn odd_sized_frames_convert_without_reading_past_the_edge() {
        let (w, h) = (7, 5);
        let mut f = I420::new(w, h);
        // Would panic on an out-of-bounds index if the odd last column or row
        // were paired with a neighbour that does not exist.
        let n = f.convert_bgra(&solid(w, h, 10, 20, 30), w as usize * 4, &[whole(w, h)]);
        assert_eq!(n, 35);
    }
}
