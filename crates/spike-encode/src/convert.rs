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
    /// Two output rows of averaged BGRA, reused frame after frame.
    ///
    /// Only touched when the frame is downscaled; at scale 1 the source rows go
    /// to the colour kernel where they already are.
    scratch: Vec<u8>,
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
            scratch: Vec::new(),
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
    /// **The divisor is never left to a runtime division.** Written the obvious
    /// way, this function ends in three `div` instructions per *output* pixel,
    /// against a count the compiler cannot see the value of. That it dominated
    /// was measurable before it was fixed: scale 1 and scale 2 read exactly the
    /// same source pixels — one sample each for four times as many outputs,
    /// against four samples each for a quarter as many — so they move identical
    /// bytes through memory, and yet conversion cost 15.0 ms against 5.0 ms.
    /// Threefold, on equal memory traffic, is the per-output-pixel arithmetic
    /// and nothing else.
    #[inline]
    fn block(&self, bgra: &[u8], src_stride: usize, ox: usize, oy: usize) -> (u32, u32, u32) {
        let s = self.scale as usize;

        // Scale 1 is not an average at all, and paying for the general path's
        // accumulator and divide-by-one is the single worst case above.
        //
        // The bounds check is not paranoia: `I420::new` floors both dimensions
        // at 2, so a frame narrower than `2 * scale` has output pixels whose
        // source block starts outside the picture. Real frames never do this
        // and never reach here — `convert_bgra` sends whole rows down a faster
        // path — but the tests do, deliberately.
        if s == 1 {
            if ox >= self.src_width as usize || oy >= self.src_height as usize {
                return (0, 0, 0);
            }
            let i = oy * src_stride + ox * 4;
            let px = &bgra[i..i + 3];
            return (px[0] as u32, px[1] as u32, px[2] as u32);
        }

        let (x0, y0) = (ox * s, oy * s);
        let x1 = ((ox + 1) * s).min(self.src_width as usize);
        let y1 = ((oy + 1) * s).min(self.src_height as usize);
        let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
        for sy in y0..y1 {
            // Slice the row once. Indexing per channel asks for three bounds
            // checks per source pixel, and at scale 2 there are four source
            // pixels behind every output one.
            let base = sy * src_stride;
            for px in bgra[base + x0 * 4..base + x1 * 4].chunks_exact(4) {
                b += px[0] as u32;
                g += px[1] as u32;
                r += px[2] as u32;
            }
        }

        let n = ((x1 - x0) * (y1 - y0)).max(1) as u32;
        if n.is_power_of_two() {
            let k = n.trailing_zeros();
            (b >> k, g >> k, r >> k)
        } else {
            // Only scales 3, 5, 6 and 7 land here, and only ever on whole
            // blocks: a block clipped by the right or bottom edge has a smaller
            // count, which may well be a power of two again.
            (b / n, g / n, r / n)
        }
    }

    /// One output row of averaged BGRA into `out`, one quadruple per pixel.
    ///
    /// Scale 2 gets its own kernel because it is the working point, and because
    /// after the colour step was vectorised the averaging was what remained:
    /// the SIMD conversion was 2.2× faster at scale 1 and 1.09× at scale 2,
    /// which is the shape of a stage that is no longer where the time goes.
    fn fill_row(
        &self,
        bgra: &[u8],
        src_stride: usize,
        x0: usize,
        oy: usize,
        inside: bool,
        out: &mut [u8],
    ) {
        #[cfg(target_arch = "x86_64")]
        if inside && self.scale == 2 {
            let r0 = oy * 2 * src_stride + x0 * 8;
            // SAFETY: `inside` says every 2×2 block of this row lands within
            // the picture, so both source rows hold the `8 * n` bytes read.
            unsafe { sse2::downscale2(&bgra[r0..], &bgra[r0 + src_stride..], out) };
            return;
        }
        let _ = inside;
        for (i, px) in out.chunks_exact_mut(4).enumerate() {
            let (b, g, r) = self.block(bgra, src_stride, x0 + i, oy);
            px[0] = b as u8;
            px[1] = g as u8;
            px[2] = r as u8;
        }
    }

    /// Convert the given source-space BGRA regions into the planes.
    ///
    /// Returns the number of output pixels written.
    ///
    /// **Two stages, on purpose.** The downscale depends on `scale` and the
    /// colour conversion does not, so they are separated: the first produces
    /// one averaged BGRA pixel per *output* pixel into scratch, the second is
    /// the same BT.601 arithmetic whatever the scale was. That is what lets a
    /// single SIMD kernel serve every scale instead of one per factor, and it
    /// keeps the scalar reference next to it for the test that pins them
    /// together.
    ///
    /// At scale 1 there is nothing to average, so the source rows are handed to
    /// the kernel where they lie. Copying them into scratch would be about
    /// 8 MB of memmove per 1080p frame — a fifth of what the whole stage costs.
    pub fn convert_bgra(&mut self, bgra: &[u8], src_stride: usize, rects: &[Rect]) -> u64 {
        let y_stride = self.y_stride();
        let uv_stride = self.uv_stride();
        let scale = self.scale as usize;
        let (src_w, src_h) = (self.src_width as usize, self.src_height as usize);
        let mut converted = 0u64;

        // Taken out of `self` so the planes can be borrowed mutably alongside
        // it, and put back at the end so the allocation survives the frame.
        let mut scratch = std::mem::take(&mut self.scratch);

        for rect in rects {
            let r = self.map(*rect);
            if r.right <= r.left || r.bottom <= r.top {
                continue;
            }
            let (x0, x1) = (r.left as usize, r.right as usize);
            let (y0, y1) = (r.top as usize, r.bottom as usize);
            let width = x1 - x0;

            let mut oy = y0;
            while oy + 1 < y1 {
                let oy2 = oy + 1;

                // Does every source block behind this row pair land inside the
                // picture? `I420::new` floors both dimensions at 2, so for a
                // frame smaller than 2·scale the answer is no and the clamping
                // scalar path has to run. Decided once per row pair rather than
                // once per pixel, which is what makes the fast paths fast.
                let inside = x1 * scale <= src_w && (oy2 + 1) * scale <= src_h;

                if scale > 1 || !inside {
                    scratch.resize(width * 8, 0);
                    for (k, row) in [oy, oy2].into_iter().enumerate() {
                        let out = &mut scratch[k * width * 4..(k + 1) * width * 4];
                        self.fill_row(bgra, src_stride, x0, row, inside, out);
                    }
                }

                let (top, bot) = if scale == 1 && inside {
                    let a = oy * src_stride + x0 * 4;
                    let b = oy2 * src_stride + x0 * 4;
                    (&bgra[a..a + width * 4], &bgra[b..b + width * 4])
                } else {
                    let (a, b) = scratch.split_at(width * 4);
                    (&a[..width * 4], &b[..width * 4])
                };

                let (y_head, y_tail) = self.y.split_at_mut(oy2 * y_stride);
                let y_top = &mut y_head[oy * y_stride + x0..][..width];
                let y_bot = &mut y_tail[x0..][..width];
                let ci = (oy / 2) * uv_stride + x0 / 2;
                let u_row = &mut self.u[ci..][..width / 2];
                let v_row = &mut self.v[ci..][..width / 2];

                rows_to_planes(top, bot, y_top, y_bot, u_row, v_row);
                oy += 2;
            }
            converted += r.area();
        }

        self.scratch = scratch;
        converted
    }
}

/// Two rows of BGRA to two rows of Y plus one row of U and V.
///
/// `top` and `bot` hold one BGRA quadruple per output pixel — already averaged
/// if the frame is being downscaled. Alpha is ignored.
fn rows_to_planes(
    top: &[u8],
    bot: &[u8],
    y_top: &mut [u8],
    y_bot: &mut [u8],
    u_row: &mut [u8],
    v_row: &mut [u8],
) {
    #[cfg(target_arch = "x86_64")]
    {
        // SSE2 needs no runtime check: it is part of the x86-64 baseline that
        // `.cargo/config.toml` pins, so it is present on every machine this can
        // run on — the target Braswell included, which has SSE4.2 and no AVX.
        // SAFETY: every slice length is checked inside, and the kernel reads
        // and writes nothing outside them.
        unsafe { sse2::rows_to_planes(top, bot, y_top, y_bot, u_row, v_row) };
        return;
    }
    #[cfg(not(target_arch = "x86_64"))]
    rows_to_planes_scalar(top, bot, y_top, y_bot, u_row, v_row);
}

/// The reference the SIMD kernel is held to.
///
/// Kept compiled in on every platform rather than behind `cfg(test)`: it is
/// what runs where there is no SSE2, and a reference that only exists in test
/// builds is a reference nobody has run.
fn rows_to_planes_scalar(
    top: &[u8],
    bot: &[u8],
    y_top: &mut [u8],
    y_bot: &mut [u8],
    u_row: &mut [u8],
    v_row: &mut [u8],
) {
    let width = y_top.len().min(y_bot.len()).min(top.len() / 4).min(bot.len() / 4);
    for i in 0..width {
        let px = &top[i * 4..i * 4 + 3];
        let (b, g, r) = (px[0] as u32, px[1] as u32, px[2] as u32);
        // Studio-swing BT.601, the range every decoder in this pipeline
        // assumes by default.
        y_top[i] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        let px = &bot[i * 4..i * 4 + 3];
        let (b, g, r) = (px[0] as u32, px[1] as u32, px[2] as u32);
        y_bot[i] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
    }

    for (j, (u, v)) in u_row.iter_mut().zip(v_row.iter_mut()).enumerate() {
        let (i0, i1) = (j * 2, j * 2 + 1);
        if i1 >= width {
            break;
        }
        let mut sum = (0u32, 0u32, 0u32);
        for row in [top, bot] {
            for i in [i0, i1] {
                sum.0 += row[i * 4] as u32;
                sum.1 += row[i * 4 + 1] as u32;
                sum.2 += row[i * 4 + 2] as u32;
            }
        }
        // Truncating, and twice over when the frame is downscaled: each of the
        // four values was already an average. Written out because the SIMD
        // kernel has to lose exactly the same bits.
        let (b, g, r) = (sum.0 / 4, sum.1 / 4, sum.2 / 4);
        *u = ((((112 * b) as i32 - (38 * r) as i32 - (74 * g) as i32 + 128) >> 8) + 128) as u8;
        *v = ((((112 * r) as i32 - (94 * g) as i32 - (18 * b) as i32 + 128) >> 8) + 128) as u8;
    }
}

#[cfg(target_arch = "x86_64")]
mod sse2 {
    use std::arch::x86_64::*;

    /// Four output pixels per row per pass, which is one 16-byte load each and
    /// two chroma samples out.
    ///
    /// # Safety
    ///
    /// Reads `4 * width` bytes from `top` and `bot` and writes `width` bytes to
    /// each Y row and `width / 2` to each chroma row, where `width` is the
    /// smallest of what the slices allow.
    pub unsafe fn rows_to_planes(
        top: &[u8],
        bot: &[u8],
        y_top: &mut [u8],
        y_bot: &mut [u8],
        u_row: &mut [u8],
        v_row: &mut [u8],
    ) {
        let width = y_top.len().min(y_bot.len()).min(top.len() / 4).min(bot.len() / 4);
        let pairs = width / 2;
        let chroma = pairs.min(u_row.len()).min(v_row.len());
        // Whole groups of four pixels; the remainder goes to the scalar path,
        // which is the same arithmetic and therefore cannot disagree.
        let vector_groups = if chroma * 2 == width { width / 4 } else { 0 };

        // SAFETY: SSE2 is guaranteed on x86-64.
        unsafe {
            let zero = _mm_setzero_si128();
            // Coefficients in BGRA order, matching the load. Y wants
            // 25·B + 129·G + 66·R; `madd` multiplies i16 pairs and adds them in
            // pairs, so each pixel arrives as two partial sums to be folded.
            let cy = _mm_setr_epi16(25, 129, 66, 0, 25, 129, 66, 0);
            let cu = _mm_setr_epi16(112, -74, -38, 0, 112, -74, -38, 0);
            let cv = _mm_setr_epi16(-18, -94, 112, 0, -18, -94, 112, 0);
            let c128 = _mm_set1_epi32(128);
            let c16 = _mm_set1_epi32(16);

            for g in 0..vector_groups {
                let off = g * 16;
                let vt = _mm_loadu_si128(top.as_ptr().add(off) as *const __m128i);
                let vb = _mm_loadu_si128(bot.as_ptr().add(off) as *const __m128i);

                let tlo = _mm_unpacklo_epi8(vt, zero);
                let thi = _mm_unpackhi_epi8(vt, zero);
                let blo = _mm_unpacklo_epi8(vb, zero);
                let bhi = _mm_unpackhi_epi8(vb, zero);

                let yt = luma(tlo, thi, cy, c128, c16);
                let yb = luma(blo, bhi, cy, c128, c16);
                std::ptr::copy_nonoverlapping(&yt as *const u32 as *const u8, y_top.as_mut_ptr().add(g * 4), 4);
                std::ptr::copy_nonoverlapping(&yb as *const u32 as *const u8, y_bot.as_mut_ptr().add(g * 4), 4);

                // Horizontal pixel pairs, then the two rows: the sum of the
                // four pixels behind one chroma sample, channel by channel.
                let sa = _mm_add_epi16(pair_sum(tlo), pair_sum(blo));
                let sb = _mm_add_epi16(pair_sum(thi), pair_sum(bhi));
                // Truncating divide by four, exactly as the scalar does.
                let avg = _mm_packus_epi16(_mm_srli_epi16(sa, 2), _mm_srli_epi16(sb, 2));
                let alo = _mm_unpacklo_epi8(avg, zero);
                let ahi = _mm_unpackhi_epi8(avg, zero);
                // Lanes 0..3 of `alo` are block A, lanes 0..3 of `ahi` block B.
                let blocks = _mm_unpacklo_epi64(alo, ahi);

                let u = chroma_pair(blocks, cu, c128);
                let v = chroma_pair(blocks, cv, c128);
                *u_row.get_unchecked_mut(g * 2) = u.0;
                *u_row.get_unchecked_mut(g * 2 + 1) = u.1;
                *v_row.get_unchecked_mut(g * 2) = v.0;
                *v_row.get_unchecked_mut(g * 2 + 1) = v.1;
            }
        }

        let done_px = vector_groups * 4;
        if done_px < width {
            super::rows_to_planes_scalar(
                &top[done_px * 4..],
                &bot[done_px * 4..],
                &mut y_top[done_px..],
                &mut y_bot[done_px..],
                &mut u_row[done_px / 2..],
                &mut v_row[done_px / 2..],
            );
        }
    }

    /// Four Y samples, packed into the low four bytes of a `u32`.
    #[inline]
    unsafe fn luma(lo: __m128i, hi: __m128i, cy: __m128i, c128: __m128i, c16: __m128i) -> u32 {
        // SAFETY: SSE2, and every argument is a register value.
        unsafe {
            let a = fold_pairs(_mm_madd_epi16(lo, cy));
            let b = fold_pairs(_mm_madd_epi16(hi, cy));
            let s = _mm_unpacklo_epi64(a, b);
            // `+ 128`, then a shift that is logical because the sum of three
            // non-negative products cannot be negative, then `+ 16`.
            let y = _mm_add_epi32(_mm_srli_epi32(_mm_add_epi32(s, c128), 8), c16);
            let packed = _mm_packus_epi16(_mm_packs_epi32(y, y), _mm_setzero_si128());
            _mm_cvtsi128_si32(packed) as u32
        }
    }

    /// Average each 2×2 source block into one BGRA pixel.
    ///
    /// # Safety
    ///
    /// Reads `8 * (out.len() / 4)` bytes from each of `row0` and `row1`.
    pub unsafe fn downscale2(row0: &[u8], row1: &[u8], out: &mut [u8]) {
        let n = out.len() / 4;
        let groups = n / 4;
        // SAFETY: SSE2 is part of the x86-64 baseline; the caller guarantees
        // both rows hold `8 * n` bytes, and `groups * 32 <= 8 * n`.
        unsafe {
            let zero = _mm_setzero_si128();
            for g in 0..groups {
                let off = g * 32;
                let a = _mm_loadu_si128(row0.as_ptr().add(off) as *const __m128i);
                let b = _mm_loadu_si128(row0.as_ptr().add(off + 16) as *const __m128i);
                let c = _mm_loadu_si128(row1.as_ptr().add(off) as *const __m128i);
                let d = _mm_loadu_si128(row1.as_ptr().add(off + 16) as *const __m128i);
                let packed = _mm_unpacklo_epi64(block_pair(a, c, zero), block_pair(b, d, zero));
                _mm_storeu_si128(out.as_mut_ptr().add(g * 16) as *mut __m128i, packed);
            }
        }

        // The tail is the same arithmetic written out, so it cannot disagree
        // with the vector path about a pixel they both could have handled.
        for i in groups * 4..n {
            let s0 = i * 8;
            let mut acc = [0u32; 3];
            for row in [row0, row1] {
                for k in [0usize, 4] {
                    for (ch, a) in acc.iter_mut().enumerate() {
                        *a += row[s0 + k + ch] as u32;
                    }
                }
            }
            for (ch, a) in acc.iter().enumerate() {
                out[i * 4 + ch] = (a >> 2) as u8;
            }
        }
    }

    /// Two output pixels: four source pixels across, two rows down.
    #[inline]
    unsafe fn block_pair(top: __m128i, bot: __m128i, zero: __m128i) -> __m128i {
        // SAFETY: SSE2, register values.
        unsafe {
            let s0 = _mm_add_epi16(
                pair_sum(_mm_unpacklo_epi8(top, zero)),
                pair_sum(_mm_unpacklo_epi8(bot, zero)),
            );
            let s1 = _mm_add_epi16(
                pair_sum(_mm_unpackhi_epi8(top, zero)),
                pair_sum(_mm_unpackhi_epi8(bot, zero)),
            );
            let avg = _mm_packus_epi16(_mm_srli_epi16(s0, 2), _mm_srli_epi16(s1, 2));
            // Lanes 0 and 2 hold the two pixels; the others are the discarded
            // upper halves of each sum.
            _mm_shuffle_epi32(avg, 0b00_00_10_00)
        }
    }

    /// `madd` leaves two partial sums per pixel. Add them and leave the pixel
    /// totals in lanes 0 and 1.
    #[inline]
    unsafe fn fold_pairs(x: __m128i) -> __m128i {
        // SAFETY: SSE2, register value.
        unsafe {
            let swapped = _mm_shuffle_epi32(x, 0b10_11_00_01);
            let summed = _mm_add_epi32(x, swapped);
            _mm_shuffle_epi32(summed, 0b00_00_10_00)
        }
    }

    /// Channel sums of the two horizontally adjacent pixels, in lanes 0..3.
    #[inline]
    unsafe fn pair_sum(x: __m128i) -> __m128i {
        // SAFETY: SSE2, register value.
        unsafe { _mm_add_epi16(x, _mm_srli_si128(x, 8)) }
    }

    /// Two chroma samples from two 2×2 block averages.
    #[inline]
    unsafe fn chroma_pair(blocks: __m128i, coeff: __m128i, c128: __m128i) -> (u8, u8) {
        // SAFETY: SSE2, register values.
        unsafe {
            let m = fold_pairs(_mm_madd_epi16(blocks, coeff));
            // Arithmetic shift: these sums go negative, and the scalar shifts a
            // signed value too.
            let s = _mm_add_epi32(_mm_srai_epi32(_mm_add_epi32(m, c128), 8), c128);
            let packed = _mm_packus_epi16(_mm_packs_epi32(s, s), _mm_setzero_si128());
            let both = _mm_cvtsi128_si32(packed) as u32;
            (both as u8, (both >> 8) as u8)
        }
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

    /// xorshift64*, so the awkward pixel values are the same awkward pixel
    /// values on every machine and in every run. A test that fails one time in
    /// forty is worse than no test.
    struct Rng(u64);

    impl Rng {
        fn byte(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 33) as u8
        }
    }

    #[test]
    fn the_simd_kernel_agrees_with_the_scalar_one_byte_for_byte() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        // Widths either side of the four-pixel group, so the remainder path is
        // exercised as well as the vector one.
        for width in [2usize, 4, 6, 8, 10, 16, 30, 32, 64, 100, 254, 256] {
            let mut top = vec![0u8; width * 4];
            let mut bot = vec![0u8; width * 4];
            for b in top.iter_mut().chain(bot.iter_mut()) {
                *b = rng.byte();
            }

            let mut want = (vec![0u8; width], vec![0u8; width], vec![0u8; width / 2], vec![
                0u8;
                width / 2
            ]);
            rows_to_planes_scalar(&top, &bot, &mut want.0, &mut want.1, &mut want.2, &mut want.3);

            let mut got = (vec![0u8; width], vec![0u8; width], vec![0u8; width / 2], vec![
                0u8;
                width / 2
            ]);
            rows_to_planes(&top, &bot, &mut got.0, &mut got.1, &mut got.2, &mut got.3);

            assert_eq!(want.0, got.0, "Y верхней строки, ширина {width}");
            assert_eq!(want.1, got.1, "Y нижней строки, ширина {width}");
            assert_eq!(want.2, got.2, "U, ширина {width}");
            assert_eq!(want.3, got.3, "V, ширина {width}");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_scale_two_downscale_matches_a_plain_box_filter() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for n in [1usize, 2, 3, 4, 5, 8, 12, 13, 64, 129] {
            let mut row0 = vec![0u8; n * 8];
            let mut row1 = vec![0u8; n * 8];
            for b in row0.iter_mut().chain(row1.iter_mut()) {
                *b = rng.byte();
            }

            let mut want = vec![0u8; n * 4];
            for i in 0..n {
                for ch in 0..3 {
                    let s: u32 = [&row0, &row1]
                        .iter()
                        .flat_map(|r| [r[i * 8 + ch] as u32, r[i * 8 + 4 + ch] as u32])
                        .sum();
                    want[i * 4 + ch] = (s >> 2) as u8;
                }
            }

            let mut got = vec![0u8; n * 4];
            // SAFETY: both rows hold the 8 * n bytes the kernel reads.
            unsafe { super::sse2::downscale2(&row0, &row1, &mut got) };
            for i in 0..n {
                // Alpha is averaged by one path and left alone by the other,
                // and read by neither.
                assert_eq!(want[i * 4..i * 4 + 3], got[i * 4..i * 4 + 3], "пиксель {i} из {n}");
            }
        }
    }

    /// The extremes matter more than the middle: saturation and the sign of the
    /// chroma shift are where a vector kernel and a scalar one part company.
    #[test]
    fn the_two_kernels_agree_on_the_corners_of_the_range() {
        for (b, g, r) in
            [(0u8, 0u8, 0u8), (255, 255, 255), (255, 0, 0), (0, 255, 0), (0, 0, 255), (1, 254, 128)]
        {
            let width = 8;
            let row: Vec<u8> = (0..width).flat_map(|_| [b, g, r, 255]).collect();
            let mut want = (vec![0u8; width], vec![0u8; width], vec![0u8; width / 2], vec![
                0u8;
                width / 2
            ]);
            let mut got = want.clone();
            rows_to_planes_scalar(&row, &row, &mut want.0, &mut want.1, &mut want.2, &mut want.3);
            rows_to_planes(&row, &row, &mut got.0, &mut got.1, &mut got.2, &mut got.3);
            assert_eq!(want, got, "BGR ({b},{g},{r})");
        }
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
