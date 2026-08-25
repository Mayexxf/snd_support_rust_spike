//! VP8/VP9 through libvpx.
//!
//! Behind the `vpx` feature. Everything here was checked against the generated
//! bindings rather than recalled — including `VP9E_CONTENT_SCREEN`, which is 1
//! and not the 2 that guessing would have produced. An ABI mistake in this file
//! does not fail to compile; it corrupts memory and prints plausible numbers.
//!
//! **Settings are chosen for a support session on a weak machine, not for
//! quality.** Realtime deadline, no lookahead, the fastest speed setting, and
//! VP9's screen-content tuning. The point of the exercise is to find out what a
//! Celeron N3150 can do, and a configuration tuned for anything else answers a
//! different question.

use std::os::raw::{c_int, c_ulong};

// The package is `env-libvpx-sys`, but its `[lib] name` is `vpx_sys` — so that
// is what the compiler knows it by. Not a typo to be tidied back.
use vpx_sys as ffi;

use crate::convert::I420;
use crate::{Codec, Encoded};

/// How the encoder is set up. Every field here changes the answer, so all of
/// them are visible rather than buried.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Target bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// libvpx speed setting. Higher is faster and worse; 8 is near the fastest
    /// realtime setting VP9 offers, which is where a Celeron has to live.
    pub cpu_used: i32,
    pub threads: u32,
}

impl Settings {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            // Roughly what a scrolling document costs at 1080p. Rate control
            // will spend less on a still screen without being asked.
            bitrate_kbps: 2_000,
            cpu_used: 8,
            // Four is the target machine's core count. Taking all of them would
            // measure a machine that has nothing else to do, which the client's
            // machine is not.
            threads: 2,
        }
    }
}

pub struct VpxEncoder {
    ctx: ffi::vpx_codec_ctx_t,
    img: ffi::vpx_image_t,
    codec: Codec,
    settings: Settings,
    frame_no: i64,
}

impl VpxEncoder {
    pub fn new(codec: Codec, settings: Settings) -> Result<Self, String> {
        let iface = match codec {
            // SAFETY: both return a static interface pointer and take no input.
            Codec::Vp8 => unsafe { ffi::vpx_codec_vp8_cx() },
            Codec::Vp9 => unsafe { ffi::vpx_codec_vp9_cx() },
            Codec::None => return Err("кодек не выбран".to_owned()),
        };
        if iface.is_null() {
            return Err(format!("libvpx собрана без {}", codec.name()));
        }

        // SAFETY: a zeroed config is what vpx_codec_enc_config_default expects
        // to fill; it is all plain integers and small structs.
        let mut cfg: ffi::vpx_codec_enc_cfg_t = unsafe { std::mem::zeroed() };
        // SAFETY: `cfg` is live and `iface` non-null.
        let err = unsafe { ffi::vpx_codec_enc_config_default(iface, &mut cfg, 0) };
        check(err, "vpx_codec_enc_config_default")?;

        cfg.g_w = settings.width;
        cfg.g_h = settings.height;
        cfg.g_timebase = ffi::vpx_rational { num: 1, den: settings.fps as c_int };
        cfg.g_threads = settings.threads;
        // No lookahead. Lookahead buys compression by delaying frames, and a
        // remote session pays for that delay with the thing it is judged on.
        cfg.g_lag_in_frames = 0;
        cfg.g_error_resilient = 0;
        cfg.rc_end_usage = ffi::vpx_rc_mode::VPX_CBR;
        cfg.rc_target_bitrate = settings.bitrate_kbps;
        cfg.rc_min_quantizer = 4;
        cfg.rc_max_quantizer = 56;
        // A small buffer keeps rate control reacting quickly, which matters more
        // than smooth bitrate when the screen alternates between still and
        // scrolling.
        cfg.rc_buf_sz = 500;
        cfg.rc_buf_initial_sz = 250;
        cfg.rc_buf_optimal_sz = 300;
        cfg.kf_mode = ffi::vpx_kf_mode::VPX_KF_AUTO;
        // Keyframes are the most expensive thing this encoder does — on screen
        // content they can be fifty times a delta frame. Ten seconds apart is
        // a compromise; a lossy link may force more.
        cfg.kf_max_dist = settings.fps * 10;

        // SAFETY: a zeroed context is the documented starting state for
        // vpx_codec_enc_init_ver.
        let mut ctx: ffi::vpx_codec_ctx_t = unsafe { std::mem::zeroed() };
        // SAFETY: ctx and cfg are live; the ABI version comes from the same
        // bindings as the struct layout, which is what makes it meaningful.
        let err = unsafe {
            ffi::vpx_codec_enc_init_ver(
                &mut ctx,
                iface,
                &cfg,
                0,
                ffi::VPX_ENCODER_ABI_VERSION as c_int,
            )
        };
        check(err, "vpx_codec_enc_init_ver")?;

        let mut enc = Self {
            ctx,
            // SAFETY: filled in below; every field this encoder relies on is set.
            img: unsafe { std::mem::zeroed() },
            codec,
            settings,
            frame_no: 0,
        };

        enc.img.fmt = ffi::vpx_img_fmt::VPX_IMG_FMT_I420;
        enc.img.w = settings.width;
        enc.img.h = settings.height;
        enc.img.d_w = settings.width;
        enc.img.d_h = settings.height;
        enc.img.r_w = settings.width;
        enc.img.r_h = settings.height;
        enc.img.x_chroma_shift = 1;
        enc.img.y_chroma_shift = 1;
        enc.img.bit_depth = 8;
        enc.img.bps = 12;

        enc.control(ffi::vp8e_enc_control_id::VP8E_SET_CPUUSED, settings.cpu_used)?;
        // Tells the encoder the picture is a desktop: sharp edges, flat areas,
        // large identical regions between frames. Worth measuring precisely
        // because it is the one setting aimed at our actual content.
        if codec == Codec::Vp9 {
            enc.control(
                ffi::vp8e_enc_control_id::VP9E_SET_TUNE_CONTENT,
                ffi::vp9e_tune_content::VP9E_CONTENT_SCREEN as i32,
            )?;
        }

        Ok(enc)
    }

    fn control(&mut self, id: ffi::vp8e_enc_control_id, value: i32) -> Result<(), String> {
        // SAFETY: vpx_codec_control_ is variadic; every control used here takes
        // a single int, which is what is passed.
        let err = unsafe { ffi::vpx_codec_control_(&mut self.ctx, id as c_int, value) };
        check(err, "vpx_codec_control_")
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Encode one frame.
    pub fn encode(&mut self, frame: &I420) -> Result<Encoded, String> {
        if frame.width != self.settings.width || frame.height != self.settings.height {
            return Err(format!(
                "кадр {}×{} не совпадает с настройкой {}×{}",
                frame.width, frame.height, self.settings.width, self.settings.height
            ));
        }

        // Point the image at the planes for this call only. libvpx reads them
        // during vpx_codec_encode and keeps nothing afterwards.
        self.img.planes[0] = frame.y.as_ptr() as *mut u8;
        self.img.planes[1] = frame.u.as_ptr() as *mut u8;
        self.img.planes[2] = frame.v.as_ptr() as *mut u8;
        self.img.stride[0] = frame.y_stride() as c_int;
        self.img.stride[1] = frame.uv_stride() as c_int;
        self.img.stride[2] = frame.uv_stride() as c_int;

        // SAFETY: the image points at buffers that outlive the call, and the
        // deadline is the realtime constant libvpx defines.
        let err = unsafe {
            ffi::vpx_codec_encode(
                &mut self.ctx,
                &self.img,
                self.frame_no,
                1,
                0,
                ffi::VPX_DL_REALTIME as c_ulong,
            )
        };
        check(err, "vpx_codec_encode").map_err(|e| self.detail(e))?;
        self.frame_no += 1;

        let mut bytes = 0usize;
        let mut keyframe = false;
        let mut iter: ffi::vpx_codec_iter_t = std::ptr::null();
        loop {
            // SAFETY: the iterator protocol is libvpx's own: call until null.
            let pkt = unsafe { ffi::vpx_codec_get_cx_data(&mut self.ctx, &mut iter) };
            let Some(pkt) = (unsafe { pkt.as_ref() }) else { break };
            if pkt.kind == ffi::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                // SAFETY: the union member is determined by `kind`, which was
                // just checked.
                let f = unsafe { pkt.data.frame };
                bytes += f.sz;
                keyframe |= f.flags & ffi::VPX_FRAME_IS_KEY != 0;
            }
        }

        Ok(Encoded { bytes, keyframe })
    }

    /// Append libvpx's own explanation, which is usually the useful half.
    fn detail(&mut self, msg: String) -> String {
        // SAFETY: returns a pointer to a static or context-owned string, or null.
        let detail = unsafe { ffi::vpx_codec_error_detail(&mut self.ctx) };
        if detail.is_null() {
            return msg;
        }
        // SAFETY: non-null, and libvpx guarantees a NUL-terminated string.
        let text = unsafe { std::ffi::CStr::from_ptr(detail) }.to_string_lossy();
        format!("{msg}: {text}")
    }
}

impl Drop for VpxEncoder {
    fn drop(&mut self) {
        // SAFETY: the context was successfully initialised, and this runs once.
        unsafe { ffi::vpx_codec_destroy(&mut self.ctx) };
    }
}

fn check(err: ffi::vpx_codec_err_t, what: &str) -> Result<(), String> {
    if err == ffi::vpx_codec_err_t::VPX_CODEC_OK {
        return Ok(());
    }
    // SAFETY: vpx_codec_err_to_string returns a static NUL-terminated string
    // for every value, including unknown ones.
    let text = unsafe { std::ffi::CStr::from_ptr(ffi::vpx_codec_err_to_string(err)) };
    Err(format!("{what}: {}", text.to_string_lossy()))
}
