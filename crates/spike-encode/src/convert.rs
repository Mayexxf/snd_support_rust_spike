//! BGRA to I420, with optional downscaling.
//!
//! The stage between capture and encoding, and not a free one: libvpx wants
//! planar YUV and desktop duplication only ever hands out BGRA. RustDesk has the
//! same stage (`convert_to_yuv`) and the same obligation to pay for it.
//!
//! Like the copy before it, this walks only the regions that changed. The planes
//! persist between frames, so untouched pixels keep the previous frame's values
//! — which is correct precisely because those pixels did not change.
//!
//! **Downscaling lives here rather than in a stage of its own.** The first
//! encoder measurement put 47 ms of a 60 ms frame in libvpx, on a machine
//! considerably faster than the target — so the only lever that matters is
//! giving the encoder fewer pixels. Doing it during conversion means the
//! averaging that a correct downscale needs is the same pass that already reads
//! every changed pixel, and the conversion gets cheaper too instead of paying
//! for a separate resize.
//!
//! Integer factors only. A 1.5× resample needs real filtering to avoid aliasing
//! on text, and a spike that answers "how much does resolution buy" does not
//! need to answer "which resampler" at the same time.
//!
//! **Chroma forces even alignment.** I420 stores one U and one V sample per 2×2
//! block, so regions are grown outward to even *output* boundaries. Converting
//! an odd-aligned region would leave a chroma sample computed from two new
//! pixels and two stale ones.

use spike_capture::Rect;

/// A frame in I420, the layout libvpx expects.
pub struct I420 {
    /// Encoded width, after any downscale. Even.
    pub width: u32,
    /// Encoded height, after any downscale. Even.
    pub height: u32,
    /// Source dimensions this was built for.
    pub src_width: u32,
    pub src_height: u32,
    /// Integer downscale factor: 1 keeps the source size.
    pub scale: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(src_width: u32, src_height: u32, scale: u32) -> Self {
        let scale = scale.max(1);
        // Floored to even: I420 has no way to store half a chroma sample, and
        // libvpx is happier with even dimensions than with the edge cases.
        let width = ((src_width / scale) & !1).max(2);
        let height = ((src_height / scale) & !1).max(2);
        let cw = (width / 2) as usize;
        let ch = (height / 2) as usize;
        Self {
            width,
            height,
            src_width,
            src_height,
            scale,
            y: vec![16; width as usize * height as usize],
            u: vec![128; cw * ch],
            v: vec![128; cw * ch],
        }
    }

    pub fn y_stride(&self) -> usize {
        self.width as usize
    }

    pub fn uv_stride(&self) -> usize {
        (self.width / 2) as usize
    }

    /// Map a source rectangle to output coordinates, grown outward to even
    /// boundaries and clipped to the frame.
    ///
    /// Outward, never inward: shrinking would leave the odd edge column showing
    /// the previous frame.
    fn map(&self, r: Rect) -> Rect {
        let s = self.scale as i32;
        Rect {
            left: ((r.left.max(0) / s) & !1).min(self.width as i32),
            top: ((r.top.max(0) / s) & !1).min(self.height as i32),
            // Ceiling division written out: i32::div_ceil is still unstable,
            // and both operands are non-negative here.
            right: (((r.right.max(0) + s - 1) / s + 1) & !1).min(self.width as i32),
            bottom: (((r.bottom.max(0) + s - 1) / s + 1) & !1).min(self.height as i32),
        }
    }

    /// Average the `scale`×`scale` source block behind one output pixel.
    ///
    /// Averaging rather than sampling: a desktop is full of one-pixel lines, and
    /// point-sampling them produces exactly the shimmering that made the client
    /// complain about the wizard's screenshots.
    #[inline]
    fn block(&self, bgra: &[u8], src_stride: usize, ox: usize, oy: usize) -> (u32, u32, u32) {
        let s = self.scale as usize;
        let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
        let mut n = 0u32;
        for sy in oy * s..((oy + 1) * s).min(self.src_height as usize) {
            let row = sy * src_stride;
            for sx in ox * s..((ox + 1) * s).min(self.src_width as usize) {
                let i = row + sx * 4;
                b += bgra[i] as u32;
                g += bgra[i + 1] as u32;
                r += bgra[i + 2] as u32;
                n += 1;
            }
        }
        let n = n.max(1);
        (b / n, g / n, r / n)
    }

    /// Convert the given source-space BGRA regions into the planes.
    ///
    /// Returns the number of output pixels written.
    pub fn convert_bgra(&mut self, bgra: &[u8], src_stride: usize, rects: &[Rect]) -> u64 {
        let y_stride = self.y_stride();
        let uv_stride = self.uv_stride();
        let mut converted = 0u64;

        for rect in rects {
            let r = self.map(*rect);
            if r.right <= r.left || r.bottom <= r.top {
                continue;
            }
            let (x0, x1) = (r.left as usize, r.right as usize);
            let (y0, y1) = (r.top as usize, r.bottom as usize);

            // Two output rows at a time: a chroma sample covers a 2×2 output
            // block, so pairing the rows produces it from values already in
            // registers instead of reading them back.
            let mut oy = y0;
            while oy < y1 {
                let oy2 = (oy + 1).min(y1 - 1);
                let mut ox = x0;
                while ox < x1 {
                    let ox2 = (ox + 1).min(x1 - 1);

                    let mut sum = (0u32, 0u32, 0u32);
                    for row in [oy, oy2] {
                        for col in [ox, ox2] {
                            let (b, g, r_) = self.block(bgra, src_stride, col, row);
                            sum = (sum.0 + b, sum.1 + g, sum.2 + r_);
                            // Studio-swing BT.601, the range every decoder in
                            // this pipeline assumes by default.
                            self.y[row * y_stride + col] =
                                (((66 * r_ + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                        }
                    }

                    let (b, g, r_) = (sum.0 / 4, sum.1 / 4, sum.2 / 4);
                    let ci = (oy / 2) * uv_stride + ox / 2;
                    if ci < self.u.len() {
                        self.u[ci] = ((((112 * b) as i32 - (38 * r_) as i32 - (74 * g) as i32 + 128)
                            >> 8)
                            + 128) as u8;
                        self.v[ci] = ((((112 * r_) as i32 - (94 * g) as i32 - (18 * b) as i32 + 128)
                            >> 8)
                            + 128) as u8;
                    }
                    ox += 2;
                }
                oy += 2;
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
        let mut f = I420::new(w, h, 1);

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
            let mut f = I420::new(w, h, 1);
            f.convert_bgra(&solid(w, h, b, g, r), w as usize * 4, &[whole(w, h)]);
            assert!((f.y[0] as i32 - y as i32).abs() <= 1, "Y {} против {y}", f.y[0]);
            assert!((f.u[0] as i32 - u as i32).abs() <= 1, "U {} против {u}", f.u[0]);
            assert!((f.v[0] as i32 - v as i32).abs() <= 1, "V {} против {v}", f.v[0]);
        }
    }

    #[test]
    fn untouched_regions_keep_the_previous_frame() {
        let (w, h) = (16, 16);
        let mut f = I420::new(w, h, 1);
        f.convert_bgra(&solid(w, h, 255, 255, 255), w as usize * 4, &[whole(w, h)]);

        let left = Rect { left: 0, top: 0, right: 8, bottom: h as i32 };
        f.convert_bgra(&solid(w, h, 0, 0, 0), w as usize * 4, &[left]);

        assert_eq!(f.y[0], 16, "изменившаяся половина должна обновиться");
        assert_eq!(f.y[12], 235, "нетронутая половина должна сохраниться");
    }

    #[test]
    fn scaling_halves_the_planes_and_keeps_the_colour() {
        let (w, h) = (1920, 1080);
        let f = I420::new(w, h, 2);
        assert_eq!((f.width, f.height), (960, 540));
        // A quarter of the pixels — which is the whole point, since the encoder
        // charges by the pixel.
        assert_eq!(f.y.len(), 960 * 540);
        assert_eq!(f.uv_stride(), 480);

        let (w, h) = (64, 64);
        let mut f = I420::new(w, h, 2);
        f.convert_bgra(&solid(w, h, 0, 0, 255), w as usize * 4, &[whole(w, h)]);
        // Downscaling a solid colour must not shift it.
        assert!((f.y[0] as i32 - 82).abs() <= 1, "Y {}", f.y[0]);
        assert!((f.v[0] as i32 - 240).abs() <= 1, "V {}", f.v[0]);
    }

    #[test]
    fn a_scaled_block_averages_rather_than_samples() {
        // Two source columns, black and white. A point sample would return one
        // of them; the average is the midpoint. Desktops are full of one-pixel
        // lines and sampling them shimmers.
        let (w, h) = (2, 2);
        let mut bgra = Vec::new();
        for _ in 0..h {
            bgra.extend_from_slice(&[0, 0, 0, 255]);
            bgra.extend_from_slice(&[255, 255, 255, 255]);
        }
        let mut f = I420::new(w, h, 2);
        f.convert_bgra(&bgra, w as usize * 4, &[whole(w, h)]);
        // Mid-grey in studio swing sits between 16 and 235.
        assert!((100..=140).contains(&f.y[0]), "Y {}", f.y[0]);
    }

    #[test]
    fn source_rectangles_map_outward_into_output_space() {
        let f = I420::new(1920, 1080, 2);
        // Source x 3..9 covers output x 1.5..4.5: the left edge floors to 1 and
        // then to the even 0, the right edge ceils to 5 and then up to 6.
        // Source y 5..11 covers output y 2.5..5.5, giving 2..6 the same way.
        let m = f.map(Rect { left: 3, top: 5, right: 9, bottom: 11 });
        assert_eq!(m, Rect { left: 0, top: 2, right: 6, bottom: 6 });
    }

    #[test]
    fn mapping_never_escapes_the_frame() {
        let f = I420::new(1920, 1080, 2);
        let m = f.map(Rect { left: -50, top: -1, right: 99_999, bottom: 99_999 });
        assert_eq!(m, Rect { left: 0, top: 0, right: 960, bottom: 540 });
    }

    #[test]
    fn odd_sized_frames_convert_without_reading_past_the_edge() {
        let (w, h) = (7, 5);
        let mut f = I420::new(w, h, 1);
        assert_eq!((f.width, f.height), (6, 4));
        let n = f.convert_bgra(&solid(w, h, 10, 20, 30), w as usize * 4, &[whole(w, h)]);
        assert_eq!(n, 24);

        // And with a scale that does not divide evenly.
        let mut f = I420::new(w, h, 2);
        assert_eq!((f.width, f.height), (2, 2));
        f.convert_bgra(&solid(w, h, 10, 20, 30), w as usize * 4, &[whole(w, h)]);
    }
}
