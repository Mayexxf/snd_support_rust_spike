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

pub mod decode;
pub mod ffi;

use std::ffi::CStr;
use std::os::raw::c_int;

use crate::convert::I420;
use crate::{Codec, Encoded};

/// How rate control spends the bitrate.
///
/// CBR is the default because a support session runs over a link with a
/// ceiling, not over a disk. The other two are here to be measured against it:
/// they buy time on easy frames by letting the bitrate move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcMode {
    Cbr,
    Vbr,
    Cq,
}

impl RcMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "cbr" => Some(Self::Cbr),
            "vbr" => Some(Self::Vbr),
            "cq" => Some(Self::Cq),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Cbr => "cbr",
            Self::Vbr => "vbr",
            Self::Cq => "cq",
        }
    }

    fn to_ffi(self) -> c_int {
        match self {
            Self::Cbr => ffi::VPX_CBR,
            Self::Vbr => ffi::VPX_VBR,
            Self::Cq => ffi::VPX_CQ,
        }
    }
}

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
    /// Tile columns, in log2 units: 0 is one column, 1 is two, up to 6.
    ///
    /// Threads have nothing to divide unless the frame is split. libvpx's own
    /// default is 6, meaning "as many as the width allows" — and a tile column
    /// may not be narrower than 256 pixels, so at 960 that is two, not sixty.
    /// The lever therefore runs *downward*: fewer tiles cost a little less
    /// compression each. Set to the same 6 by default so the knob is visible
    /// rather than implied. VP9 only.
    pub tile_columns: u32,
    /// Row-level multithreading. Off in libvpx by default.
    ///
    /// Without it the only parallelism VP9 has is the tile columns above, which
    /// at 960×540 is two — so the fourth thread had nothing to do. That is
    /// exactly what the first thread sweep on this harness measured before this
    /// field existed: two threads bought 1.37×, four bought a further 4%, and
    /// the conclusion drawn was that VP9 does not scale. It does. With row-mt
    /// on, four threads take p95 from 15.7 ms to 12.1 ms on the same 300
    /// frames, and the budget from 61% to 50%. VP9 only.
    pub row_mt: bool,
    /// How different a block must be before it is encoded again.
    ///
    /// Zero — libvpx's default — looks at every block on every frame, including
    /// the ones a blinking caret never touched. Understood by both codecs.
    pub static_threshold: u32,
    pub rc_mode: RcMode,
    /// Floor on the quantizer. This harness has used 4 since the encoder went
    /// in.
    ///
    /// **Measured, and it is not a lever.** Raising it to 20 and to 32 on 300
    /// frames of dense text moved encoding by 0.2 ms — inside the noise — and
    /// the delta frame by 35 bytes. On this content the rate cap binds long
    /// before the floor does: the quantizer the encoder actually picks is far
    /// above 4 already, so the floor is never what stops it. Kept because it
    /// costs nothing to expose and the answer changes with the bitrate.
    pub min_quantizer: u32,
    /// The other end, and unlike the floor this one binds.
    ///
    /// VP9 quantizes up to 63; this harness capped it at 56 from the first
    /// commit, unasked and unexposed. At `--scale 2` the cap never comes into
    /// play and the run hits its bitrate. At `--scale 1` it does: the encoder
    /// reaches 56, cannot spend any more quality to save bytes, and overshoots
    /// the target instead — 24 KB per delta frame against a 8.3 KB budget.
    ///
    /// Exposed so the trade can be measured rather than inherited. Default
    /// stays 56, so every number taken before this field existed still stands.
    pub max_quantizer: u32,
    /// Quality target for [`RcMode::Cq`]. Ignored by the other two modes.
    pub cq_level: u32,
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
            // Chosen so that adding these fields changes no measurement:
            // tile_columns, row_mt, static_threshold and cq_level repeat
            // libvpx's own defaults, rc_mode and min_quantizer repeat what this
            // harness already hard-coded below. Every number in the reports
            // taken before they existed stays comparable.
            tile_columns: 6,
            row_mt: false,
            static_threshold: 0,
            rc_mode: RcMode::Cbr,
            min_quantizer: 4,
            max_quantizer: 56,
            cq_level: 10,
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
        cfg.rc_end_usage = settings.rc_mode.to_ffi();
        cfg.rc_target_bitrate = settings.bitrate_kbps;
        cfg.rc_min_quantizer = settings.min_quantizer;
        cfg.rc_max_quantizer = settings.max_quantizer;
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
        // Skip blocks that barely moved. Both codecs understand it despite the
        // VP8E_ name, and it is the only setting here aimed at the case a
        // support session spends most of its time in: a screen that is almost,
        // but not quite, still.
        enc.control(ffi::VP8E_SET_STATIC_THRESHOLD, settings.static_threshold as c_int)?;
        // Tells the encoder the picture is a desktop: sharp edges, flat areas,
        // large regions identical between frames. Worth measuring precisely
        // because it is the one setting aimed at our actual content.
        if codec == Codec::Vp9 {
            enc.control(ffi::VP9E_SET_TUNE_CONTENT, ffi::VP9E_CONTENT_SCREEN)?;
            enc.control(ffi::VP9E_SET_TILE_COLUMNS, settings.tile_columns as c_int)?;
            enc.control(ffi::VP9E_SET_ROW_MT, c_int::from(settings.row_mt))?;
        }
        // The same thing said in VP8's vocabulary. It was never said, so every
        // comparison between the two codecs here has been partly a comparison
        // between a codec told what it was looking at and one that was not.
        //
        // Mode 1 is "on". The header offers a 2 — "on with more aggressive rate
        // control" — which is aimed squarely at this content and is not sent
        // here: one change at a time, and the first question is what VP8 does
        // when it is merely told the truth.
        //
        // The failure is not swallowed. Tolerating a refusal would leave no way
        // to tell "the hint was applied" from "the hint was refused", and the
        // whole reason this line exists is that a difference between the two
        // codecs turned out to be a setting one of them never received.
        if codec == Codec::Vp8 {
            enc.control(ffi::VP8E_SET_SCREEN_CONTENT_MODE, 1)?;
        }
        // Meaningless outside CQ, and libvpx rejects it for VP9 in some builds
        // rather than ignoring it, so it is only sent when it is asked for.
        if settings.rc_mode == RcMode::Cq {
            enc.control(ffi::VP8E_SET_CQ_LEVEL, settings.cq_level as c_int)?;
        }

        Ok(enc)
    }

    fn control(&mut self, id: c_int, value: i32) -> Result<(), String> {
        // SAFETY: vpx_codec_control_ is variadic; every control used here takes
        // a single int, which is what is passed.
        let err = unsafe { ffi::vpx_codec_control_(&mut self.ctx, id, value) };
        check(err, "vpx_codec_control_")
    }

    /// Read a control that answers into an `int` the caller owns.
    ///
    /// Separate from [`Self::control`] because the variadic argument is a
    /// pointer rather than a value, and passing one where the other is expected
    /// is exactly the kind of mistake a hand-written FFI exists to make. The
    /// getters are a distinct half of `vp8e_enc_control_id` — every `*_GET_*`
    /// member — and none of them was reachable before.
    fn control_get(&mut self, id: c_int) -> Result<i32, String> {
        let mut out: c_int = 0;
        // SAFETY: vpx_codec_control_ is variadic; the GET controls take `int *`,
        // and `out` is a live, correctly typed local that outlives the call.
        let err = unsafe { ffi::vpx_codec_control_(&mut self.ctx, id, &mut out as *mut c_int) };
        check(err, "vpx_codec_control_")?;
        Ok(out)
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Encode one frame.
    pub fn encode(&mut self, frame: &I420) -> Result<Encoded, String> {
        self.encode_inner(frame, None)
    }

    /// Encode one frame and keep the bytes.
    ///
    /// Separate from [`Self::encode`] so the measured path never allocates: the
    /// only caller that wants the payload is the one that has to *look* at the
    /// picture, and it does not care what it cost.
    pub fn encode_keeping(
        &mut self,
        frame: &I420,
        payload: &mut Vec<u8>,
    ) -> Result<Encoded, String> {
        self.encode_inner(frame, Some(payload))
    }

    fn encode_inner(
        &mut self,
        frame: &I420,
        mut payload: Option<&mut Vec<u8>>,
    ) -> Result<Encoded, String> {
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
        let mut packets = 0usize;
        let mut iter: *const std::os::raw::c_void = std::ptr::null();
        loop {
            // SAFETY: the iterator protocol is libvpx's own — call until null.
            let pkt = unsafe { ffi::vpx_codec_get_cx_data(&mut self.ctx, &mut iter) };
            let Some(pkt) = (unsafe { pkt.as_ref() }) else { break };
            if pkt.kind == ffi::VPX_CODEC_CX_FRAME_PKT {
                // SAFETY: the active union member is determined by `kind`, which
                // was just checked.
                let f = unsafe { pkt.data.frame };
                packets += 1;
                bytes += f.sz;
                keyframe |= f.flags & ffi::VPX_FRAME_IS_KEY != 0;
                if let Some(out) = payload.as_deref_mut() {
                    // SAFETY: libvpx owns `sz` bytes at `buf` and keeps them
                    // valid until the next call into this codec instance; they
                    // are copied out before that happens.
                    out.extend_from_slice(unsafe {
                        std::slice::from_raw_parts(f.buf as *const u8, f.sz)
                    });
                }
            }
        }

        // Read after the packets, not before: the control reports the quantizer
        // of the frame just encoded, and there is no frame to ask about until
        // this one has been. A dropped frame has none, and a codec that refuses
        // the control at all must not take the run down with it — the quantizer
        // is worth knowing, not worth dying for.
        let quantizer = (packets > 0)
            .then(|| self.control_get(ffi::VP8E_GET_LAST_QUANTIZER_64).ok())
            .flatten()
            .and_then(|q| u8::try_from(q).ok());

        // No packet at all means libvpx took the frame and emitted nothing for
        // it. With `lag_in_frames` at zero there is no lookahead to hold it, so
        // the only way here is rate control dropping the frame. Reported rather
        // than folded into a zero-byte frame — see `Encoded::dropped`.
        Ok(Encoded { bytes, keyframe, dropped: packets == 0, quantizer })
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
