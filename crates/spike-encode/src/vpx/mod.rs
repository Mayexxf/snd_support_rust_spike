//! VP8/VP9 through libvpx.
//!
//! Behind the `vpx` feature. Declarations are in [`ffi`], written by hand
//! against the 1.15.1 headers — see the note there for why they are not
//! generated.
//!
//! **Settings are chosen for a support session on a weak machine, not for
//! quality.** Realtime deadline, no lookahead, the fastest speed setting, and
//! VP9's screen-content tuning. The point of the exercise is to find out what a
//! Celeron N3150 can do, and a configuration tuned for anything else answers a
//! different question.

pub mod ffi;

use std::ffi::CStr;
use std::os::raw::c_int;

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
            // spends less on a still screen without being asked.
            bitrate_kbps: 2_000,
            cpu_used: 8,
            // The target machine has four cores. Taking all of them would
            // measure a machine with nothing else to do, which a client's
            // machine is not.
            threads: 2,
        }
    }
}

pub struct VpxEncoder {
    ctx: ffi::vpx_codec_ctx,
    img: ffi::vpx_image,
    codec: Codec,
    settings: Settings,
    frame_no: i64,
}

impl VpxEncoder {
    pub fn new(codec: Codec, settings: Settings) -> Result<Self, String> {
        // SAFETY: both return a static interface pointer and take no input.
        let iface = match codec {
            Codec::Vp8 => unsafe { ffi::vpx_codec_vp8_cx() },
            Codec::Vp9 => unsafe { ffi::vpx_codec_vp9_cx() },
            Codec::None => return Err("кодек не выбран".to_owned()),
        };
        if iface.is_null() {
            return Err(format!("libvpx собрана без {}", codec.name()));
        }

        let mut cfg = ffi::vpx_codec_enc_cfg::default();
        // SAFETY: `cfg` is live and correctly sized; `iface` is non-null.
        let err = unsafe { ffi::vpx_codec_enc_config_default(iface, &mut cfg, 0) };
        check(err, "vpx_codec_enc_config_default")?;
        sanity_check(&cfg)?;

        cfg.g_w = settings.width;
        cfg.g_h = settings.height;
        cfg.g_timebase = ffi::vpx_rational { num: 1, den: settings.fps as c_int };
        cfg.g_threads = settings.threads;
        // No lookahead. It buys compression by delaying frames, and a remote
        // session pays for that delay with the thing it is judged on.
        cfg.g_lag_in_frames = 0;
        cfg.g_error_resilient = 0;
        cfg.rc_end_usage = ffi::VPX_CBR;
        cfg.rc_target_bitrate = settings.bitrate_kbps;
        cfg.rc_min_quantizer = 4;
        cfg.rc_max_quantizer = 56;
        // A small buffer keeps rate control reacting quickly, which matters more
        // than a smooth bitrate when the screen alternates between still and
        // scrolling.
        cfg.rc_buf_sz = 500;
        cfg.rc_buf_initial_sz = 250;
        cfg.rc_buf_optimal_sz = 300;
        cfg.kf_mode = ffi::VPX_KF_AUTO;
        // Keyframes are the most expensive thing this encoder does — on screen
        // content they can be fifty times a delta frame. Ten seconds apart is a
        // compromise; a lossy link may force more.
        cfg.kf_max_dist = settings.fps * 10;

        let mut ctx = ffi::vpx_codec_ctx::default();
        // SAFETY: ctx and cfg are live. The ABI version is expanded from the
        // same headers the structs were transcribed from, so libvpx rejecting
        // it is exactly the signal we want if that transcription is wrong.
        let err = unsafe {
            ffi::vpx_codec_enc_init_ver(
                &mut ctx,
                iface,
                &cfg,
                0,
                ffi::VPX_ENCODER_ABI_VERSION,
            )
        };
        check(err, "vpx_codec_enc_init_ver")?;

        let mut img = ffi::vpx_image { ..Default::default() };
        img.fmt = ffi::VPX_IMG_FMT_I420;
        img.w = settings.width;
        img.h = settings.height;
        img.d_w = settings.width;
        img.d_h = settings.height;
        img.r_w = settings.width;
        img.r_h = settings.height;
        img.x_chroma_shift = 1;
        img.y_chroma_shift = 1;
        img.bit_depth = 8;
        img.bps = 12;

        let mut enc = Self { ctx, img, codec, settings, frame_no: 0 };

        enc.control(ffi::VP8E_SET_CPUUSED, settings.cpu_used)?;
        // Tells the encoder the picture is a desktop: sharp edges, flat areas,
        // large regions identical between frames. Worth measuring precisely
        // because it is the one setting aimed at our actual content.
        if codec == Codec::Vp9 {
            enc.control(ffi::VP9E_SET_TUNE_CONTENT, ffi::VP9E_CONTENT_SCREEN)?;
        }

        Ok(enc)
    }

    fn control(&mut self, id: c_int, value: i32) -> Result<(), String> {
        // SAFETY: vpx_codec_control_ is variadic; every control used here takes
        // a single int, which is what is passed.
        let err = unsafe { ffi::vpx_codec_control_(&mut self.ctx, id, value) };
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

        // SAFETY: the image points at buffers that outlive the call.
        let err = unsafe {
            ffi::vpx_codec_encode(
                &mut self.ctx,
                &self.img,
                self.frame_no,
                1,
                0,
                ffi::VPX_DL_REALTIME,
            )
        };
        if err != ffi::VPX_CODEC_OK {
            let base = err_text(err);
            return Err(format!("vpx_codec_encode: {}", self.detail(base)));
        }
        self.frame_no += 1;

        let mut bytes = 0usize;
        let mut keyframe = false;
        let mut iter: *const std::os::raw::c_void = std::ptr::null();
        loop {
            // SAFETY: the iterator protocol is libvpx's own — call until null.
            let pkt = unsafe { ffi::vpx_codec_get_cx_data(&mut self.ctx, &mut iter) };
            let Some(pkt) = (unsafe { pkt.as_ref() }) else { break };
            if pkt.kind == ffi::VPX_CODEC_CX_FRAME_PKT {
                // SAFETY: the active union member is determined by `kind`, which
                // was just checked.
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
        let text = unsafe { CStr::from_ptr(detail) }.to_string_lossy();
        format!("{msg}: {text}")
    }
}

impl Drop for VpxEncoder {
    fn drop(&mut self) {
        // SAFETY: the context was successfully initialised, and this runs once.
        unsafe { ffi::vpx_codec_destroy(&mut self.ctx) };
    }
}

/// Does the filled-in config look like something libvpx wrote?
///
/// The defence against a transcription mistake that `vpx_codec_enc_init_ver`
/// would not catch. libvpx fills every field here; if our field order were
/// wrong, these would read as values from neighbouring fields — usually zero or
/// something enormous. Cheap, and it fires before any of it reaches an encoder.
fn sanity_check(cfg: &ffi::vpx_codec_enc_cfg) -> Result<(), String> {
    let complaints = [
        (cfg.g_timebase.den <= 0, "g_timebase.den"),
        (cfg.rc_target_bitrate == 0 || cfg.rc_target_bitrate > 1_000_000, "rc_target_bitrate"),
        (cfg.rc_max_quantizer > 63, "rc_max_quantizer"),
        (cfg.g_pass != ffi::VPX_RC_ONE_PASS, "g_pass"),
        (cfg.rc_end_usage < ffi::VPX_VBR || cfg.rc_end_usage > 3, "rc_end_usage"),
    ];
    let bad: Vec<&str> = complaints.iter().filter(|(c, _)| *c).map(|(_, n)| *n).collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(format!(
        "libvpx заполнила конфигурацию значениями, которых там быть не может: {}. \
         Почти наверняка раскладка структуры в ffi.rs разошлась с установленной \
         версией libvpx — сверьте VPX_VERSION с тем, что стоит на самом деле",
        bad.join(", ")
    ))
}

fn err_text(err: c_int) -> String {
    // SAFETY: returns a static NUL-terminated string for every value.
    let text = unsafe { CStr::from_ptr(ffi::vpx_codec_err_to_string(err)) };
    text.to_string_lossy().into_owned()
}

fn check(err: c_int, what: &str) -> Result<(), String> {
    if err == ffi::VPX_CODEC_OK {
        return Ok(());
    }
    Err(format!("{what}: {}", err_text(err)))
}
