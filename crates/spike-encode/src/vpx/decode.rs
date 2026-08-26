//! Decoding, for the one question the rest of the harness cannot answer.
//!
//! Nothing here is measured, and nothing here runs during a measurement. The
//! encoder side of this crate spends its whole life pricing an image that
//! nobody has seen: the harness encodes, counts the bytes and throws them away.
//! That is exactly right for "what does it cost" and useless for "is the text
//! still readable" — which, for a tool whose user is reading someone else's
//! screen, is the product.
//!
//! Both losses the pipeline inflicts are worth seeing apart from each other:
//! the downscale before the encoder, and the quantiser inside it. So the
//! caller dumps three pictures — source, encoder input, decoder output — and
//! looks at them.

use std::os::raw::c_void;

use super::{check, ffi};
use crate::Codec;

/// One decoded frame, copied out of libvpx's buffers into our own.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

pub struct VpxDecoder {
    ctx: ffi::vpx_codec_ctx,
}

impl VpxDecoder {
    pub fn new(codec: Codec) -> Result<Self, String> {
        // SAFETY: both return a static interface pointer and take no input.
        let iface = match codec {
            Codec::Vp8 => unsafe { ffi::vpx_codec_vp8_dx() },
            Codec::Vp9 => unsafe { ffi::vpx_codec_vp9_dx() },
            Codec::None => return Err("кодек не выбран".to_owned()),
        };
        if iface.is_null() {
            return Err(format!("libvpx собрана без декодера {}", codec.name()));
        }

        // Zero width and height ask the decoder to take them from the stream,
        // which is the only honest answer here: the point is to see what the
        // encoder actually produced, not to tell it what we expected.
        let cfg = ffi::vpx_codec_dec_cfg { threads: 1, w: 0, h: 0 };
        let mut ctx = ffi::vpx_codec_ctx::default();
        // SAFETY: ctx and cfg are live; the ABI version is expanded from the
        // same headers the structs were transcribed from.
        let err = unsafe {
            ffi::vpx_codec_dec_init_ver(&mut ctx, iface, &cfg, 0, ffi::VPX_DECODER_ABI_VERSION)
        };
        check(err, "vpx_codec_dec_init_ver")?;
        Ok(Self { ctx })
    }

    /// Decode one packet. `Ok(None)` means libvpx accepted it and had no frame
    /// to hand back yet, which is not an error.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<Decoded>, String> {
        // SAFETY: `data` outlives the call; libvpx copies what it needs.
        let err = unsafe {
            ffi::vpx_codec_decode(
                &mut self.ctx,
                data.as_ptr(),
                data.len() as std::os::raw::c_uint,
                std::ptr::null_mut(),
                0,
            )
        };
        check(err, "vpx_codec_decode")?;

        let mut iter: *const c_void = std::ptr::null();
        // SAFETY: the iterator protocol is libvpx's own — call until null.
        let img = unsafe { ffi::vpx_codec_get_frame(&mut self.ctx, &mut iter) };
        // SAFETY: a non-null image stays valid until the next call into this
        // decoder, and everything is copied out before then.
        let Some(img) = (unsafe { img.as_ref() }) else { return Ok(None) };
        if img.fmt != ffi::VPX_IMG_FMT_I420 {
            return Err(format!("декодер отдал формат {}, а ожидался I420", img.fmt));
        }

        let (w, h) = (img.d_w as usize, img.d_h as usize);
        let plane = |i: usize, pw: usize, ph: usize| -> Vec<u8> {
            let stride = img.stride[i] as usize;
            let mut out = Vec::with_capacity(pw * ph);
            for row in 0..ph {
                // SAFETY: libvpx allocated `stride` bytes for each of `ph` rows
                // of this plane, and `pw <= stride`.
                let src =
                    unsafe { std::slice::from_raw_parts(img.planes[i].add(row * stride), pw) };
                out.extend_from_slice(src);
            }
            out
        };

        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Ok(Some(Decoded {
            width: w as u32,
            height: h as u32,
            y: plane(0, w, h),
            u: plane(1, cw, ch),
            v: plane(2, cw, ch),
        }))
    }
}

impl Drop for VpxDecoder {
    fn drop(&mut self) {
        // SAFETY: the context was successfully initialised, and this runs once.
        unsafe { ffi::vpx_codec_destroy(&mut self.ctx) };
    }
}
