//! libvpx declarations, written out by hand.
//!
//! Not generated. bindgen was tried first and failed in the worst possible way:
//! when clang cannot reach the headers it does not stop, it emits incomplete
//! types, and the build dies fifteen field accesses later in errors that point
//! at our code and say nothing about the include path. Getting it working
//! needs vcpkg or a prebuilt SDK, plus LLVM, plus a Visual Studio environment
//! for clang to find `stdint.h` — three moving parts, on a network that blocks
//! two of them.
//!
//! What we actually need from libvpx is nine functions and four structs. Those
//! are transcribed here from the 1.15.1 headers, field for field and in order.
//! `repr(C)` then computes the same padding the C compiler does, so the layout
//! follows from the declarations rather than from arithmetic anyone has to
//! check.
//!
//! **The guard against a transcription mistake is libvpx's own.**
//! `vpx_codec_enc_init_ver` takes [`VPX_ENCODER_ABI_VERSION`], which the library
//! compares against what it was built with. That number is not a constant we
//! invented — it is `18 + VPX_CODEC_ABI_VERSION + VPX_EXT_RATECTRL_ABI_VERSION`
//! expanded from the same headers, and libvpx changes it precisely when these
//! structures change. A mismatch returns an error instead of corrupting memory.
//!
//! Verified constants, because two of them are not what guessing produces:
//! `VPX_IMG_FMT_I420` is `PLANAR | 2` = 258, and `VP9E_CONTENT_SCREEN` is 1.

#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_long, c_uint, c_ulong, c_void};

pub const VPX_SS_MAX_LAYERS: usize = 5;
pub const VPX_TS_MAX_LAYERS: usize = 5;
pub const VPX_TS_MAX_PERIODICITY: usize = 16;
pub const VPX_MAX_LAYERS: usize = 12;

/// `18 + VPX_CODEC_ABI_VERSION(9) + VPX_EXT_RATECTRL_ABI_VERSION(10)`, expanded
/// from the 1.15.1 headers. libvpx rejects a caller whose value disagrees.
pub const VPX_ENCODER_ABI_VERSION: c_int = 37;

/// `3 + VPX_CODEC_ABI_VERSION(9)`, and that one is `4 + VPX_IMAGE_ABI_VERSION(5)`
/// — all three expanded from the 1.15.1 headers. Guards the decoder the same
/// way the number above guards the encoder.
pub const VPX_DECODER_ABI_VERSION: c_int = 12;

pub const VPX_CODEC_OK: c_int = 0;
pub const VPX_CODEC_CX_FRAME_PKT: c_int = 0;
pub const VPX_FRAME_IS_KEY: u32 = 0x1;
pub const VPX_DL_REALTIME: c_ulong = 1;

/// `VPX_IMG_FMT_PLANAR (0x100) | 2`.
pub const VPX_IMG_FMT_I420: c_int = 258;

pub const VPX_RC_ONE_PASS: c_int = 0;
pub const VPX_VBR: c_int = 0;
pub const VPX_CBR: c_int = 1;
pub const VPX_CQ: c_int = 2;
pub const VPX_KF_FIXED: c_int = 0;
pub const VPX_KF_AUTO: c_int = 1;

// Control ids are positions in `vp8e_enc_control_id`, an enum that assigns only
// four of its members explicitly (`VP8E_SET_ROI_MAP = 8`, `VP8E_SET_SCALEMODE
// = 11`, `VP8E_SET_CPUUSED = 13`, `VP9E_SET_MIN_GF_INTERVAL = 48`) and lets the
// rest fall out of the order. Walking that order from the 1.15.1 header
// reproduces the four values already written here, which is what makes the two
// added below trustworthy rather than remembered.
pub const VP8E_SET_CPUUSED: c_int = 13;
pub const VP8E_SET_STATIC_THRESHOLD: c_int = 17;
pub const VP8E_SET_CQ_LEVEL: c_int = 25;
pub const VP9E_SET_TILE_COLUMNS: c_int = 33;
pub const VP9E_SET_TUNE_CONTENT: c_int = 43;
pub const VP9E_SET_ROW_MT: c_int = 55;

/// Second member of `vp9e_tune_content`, which starts at zero — so 1, not 2.
pub const VP9E_CONTENT_SCREEN: c_int = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct vpx_rational {
    pub num: c_int,
    pub den: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct vpx_fixed_buf {
    pub buf: *mut c_void,
    pub sz: usize,
}

impl Default for vpx_fixed_buf {
    fn default() -> Self {
        Self { buf: std::ptr::null_mut(), sz: 0 }
    }
}

/// Transcribed from `vpx_encoder.h`, all sixty-one fields in order. Fields this
/// harness never sets are still present: leaving one out would shift every
/// field after it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct vpx_codec_enc_cfg {
    pub g_usage: c_uint,
    pub g_threads: c_uint,
    pub g_profile: c_uint,
    pub g_w: c_uint,
    pub g_h: c_uint,
    pub g_bit_depth: c_int,
    pub g_input_bit_depth: c_uint,
    pub g_timebase: vpx_rational,
    pub g_error_resilient: u32,
    pub g_pass: c_int,
    pub g_lag_in_frames: c_uint,
    pub rc_dropframe_thresh: c_uint,
    pub rc_resize_allowed: c_uint,
    pub rc_scaled_width: c_uint,
    pub rc_scaled_height: c_uint,
    pub rc_resize_up_thresh: c_uint,
    pub rc_resize_down_thresh: c_uint,
    pub rc_end_usage: c_int,
    pub rc_twopass_stats_in: vpx_fixed_buf,
    pub rc_firstpass_mb_stats_in: vpx_fixed_buf,
    pub rc_target_bitrate: c_uint,
    pub rc_min_quantizer: c_uint,
    pub rc_max_quantizer: c_uint,
    pub rc_undershoot_pct: c_uint,
    pub rc_overshoot_pct: c_uint,
    pub rc_buf_sz: c_uint,
    pub rc_buf_initial_sz: c_uint,
    pub rc_buf_optimal_sz: c_uint,
    pub rc_2pass_vbr_bias_pct: c_uint,
    pub rc_2pass_vbr_minsection_pct: c_uint,
    pub rc_2pass_vbr_maxsection_pct: c_uint,
    pub rc_2pass_vbr_corpus_complexity: c_uint,
    pub kf_mode: c_int,
    pub kf_min_dist: c_uint,
    pub kf_max_dist: c_uint,
    pub ss_number_layers: c_uint,
    pub ss_enable_auto_alt_ref: [c_int; VPX_SS_MAX_LAYERS],
    pub ss_target_bitrate: [c_uint; VPX_SS_MAX_LAYERS],
    pub ts_number_layers: c_uint,
    pub ts_target_bitrate: [c_uint; VPX_TS_MAX_LAYERS],
    pub ts_rate_decimator: [c_uint; VPX_TS_MAX_LAYERS],
    pub ts_periodicity: c_uint,
    pub ts_layer_id: [c_uint; VPX_TS_MAX_PERIODICITY],
    pub layer_target_bitrate: [c_uint; VPX_MAX_LAYERS],
    pub temporal_layering_mode: c_int,
    pub use_vizier_rc_params: c_int,
    pub active_wq_factor: vpx_rational,
    pub err_per_mb_factor: vpx_rational,
    pub sr_default_decay_limit: vpx_rational,
    pub sr_diff_factor: vpx_rational,
    pub kf_err_per_mb_factor: vpx_rational,
    pub kf_frame_min_boost_factor: vpx_rational,
    pub kf_frame_max_boost_first_factor: vpx_rational,
    pub kf_frame_max_boost_subs_factor: vpx_rational,
    pub kf_max_total_boost_factor: vpx_rational,
    pub gf_max_total_boost_factor: vpx_rational,
    pub gf_frame_max_boost_factor: vpx_rational,
    pub zm_factor: vpx_rational,
    pub rd_mult_inter_qp_fac: vpx_rational,
    pub rd_mult_arf_qp_fac: vpx_rational,
    pub rd_mult_key_qp_fac: vpx_rational,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct vpx_codec_ctx {
    pub name: *const c_char,
    pub iface: *mut c_void,
    pub err: c_int,
    pub err_detail: *const c_char,
    pub init_flags: c_long,
    /// A union of three pointers in C; only ever read by libvpx.
    pub config: *const c_void,
    pub priv_: *mut c_void,
}

impl Default for vpx_codec_ctx {
    fn default() -> Self {
        Self {
            name: std::ptr::null(),
            iface: std::ptr::null_mut(),
            err: 0,
            err_detail: std::ptr::null(),
            init_flags: 0,
            config: std::ptr::null(),
            priv_: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct vpx_image {
    pub fmt: c_int,
    pub cs: c_int,
    pub range: c_int,
    pub w: c_uint,
    pub h: c_uint,
    pub bit_depth: c_uint,
    pub d_w: c_uint,
    pub d_h: c_uint,
    pub r_w: c_uint,
    pub r_h: c_uint,
    pub x_chroma_shift: c_uint,
    pub y_chroma_shift: c_uint,
    pub planes: [*mut u8; 4],
    pub stride: [c_int; 4],
    pub bps: c_int,
    pub user_priv: *mut c_void,
    pub img_data: *mut u8,
    pub img_data_owner: c_int,
    pub self_allocd: c_int,
    pub fb_priv: *mut c_void,
}

impl Default for vpx_image {
    fn default() -> Self {
        Self {
            fmt: 0,
            cs: 0,
            range: 0,
            w: 0,
            h: 0,
            bit_depth: 8,
            d_w: 0,
            d_h: 0,
            r_w: 0,
            r_h: 0,
            x_chroma_shift: 0,
            y_chroma_shift: 0,
            planes: [std::ptr::null_mut(); 4],
            stride: [0; 4],
            bps: 0,
            user_priv: std::ptr::null_mut(),
            img_data: std::ptr::null_mut(),
            img_data_owner: 0,
            self_allocd: 0,
            fb_priv: std::ptr::null_mut(),
        }
    }
}

/// The `frame` arm of the packet union.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct vpx_codec_cx_pkt_frame {
    pub buf: *mut c_void,
    pub sz: usize,
    pub pts: i64,
    pub duration: c_ulong,
    pub flags: u32,
    pub partition_id: c_int,
    pub width: [c_uint; VPX_SS_MAX_LAYERS],
    pub height: [c_uint; VPX_SS_MAX_LAYERS],
    pub spatial_layer_encoded: [u8; VPX_SS_MAX_LAYERS],
}

/// `char pad[128 - sizeof(enum)]` in C, which fixes the union at 124 bytes
/// before alignment. Declared here so the union cannot come out smaller than
/// the one libvpx writes into.
#[repr(C)]
pub union vpx_codec_cx_pkt_data {
    pub frame: vpx_codec_cx_pkt_frame,
    pub pad: [u8; 124],
}

#[repr(C)]
pub struct vpx_codec_cx_pkt {
    pub kind: c_int,
    pub data: vpx_codec_cx_pkt_data,
}

/// Decoder configuration. Three fields in the 1.15.1 header, all `unsigned int`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct vpx_codec_dec_cfg {
    pub threads: c_uint,
    pub w: c_uint,
    pub h: c_uint,
}

unsafe extern "C" {
    pub fn vpx_codec_vp8_cx() -> *mut c_void;
    pub fn vpx_codec_vp9_cx() -> *mut c_void;
    pub fn vpx_codec_vp8_dx() -> *mut c_void;
    pub fn vpx_codec_vp9_dx() -> *mut c_void;

    pub fn vpx_codec_dec_init_ver(
        ctx: *mut vpx_codec_ctx,
        iface: *mut c_void,
        cfg: *const vpx_codec_dec_cfg,
        flags: c_long,
        ver: c_int,
    ) -> c_int;

    pub fn vpx_codec_decode(
        ctx: *mut vpx_codec_ctx,
        data: *const u8,
        data_sz: c_uint,
        user_priv: *mut c_void,
        deadline: c_long,
    ) -> c_int;

    pub fn vpx_codec_get_frame(
        ctx: *mut vpx_codec_ctx,
        iter: *mut *const c_void,
    ) -> *mut vpx_image;

    pub fn vpx_codec_enc_config_default(
        iface: *mut c_void,
        cfg: *mut vpx_codec_enc_cfg,
        usage: c_uint,
    ) -> c_int;

    pub fn vpx_codec_enc_init_ver(
        ctx: *mut vpx_codec_ctx,
        iface: *mut c_void,
        cfg: *const vpx_codec_enc_cfg,
        flags: c_long,
        ver: c_int,
    ) -> c_int;

    pub fn vpx_codec_encode(
        ctx: *mut vpx_codec_ctx,
        img: *const vpx_image,
        pts: i64,
        duration: c_ulong,
        flags: c_long,
        deadline: c_ulong,
    ) -> c_int;

    pub fn vpx_codec_get_cx_data(
        ctx: *mut vpx_codec_ctx,
        iter: *mut *const c_void,
    ) -> *const vpx_codec_cx_pkt;

    pub fn vpx_codec_destroy(ctx: *mut vpx_codec_ctx) -> c_int;

    /// Variadic in C. Every control this harness uses takes a single `int`.
    pub fn vpx_codec_control_(ctx: *mut vpx_codec_ctx, ctrl_id: c_int, ...) -> c_int;

    pub fn vpx_codec_err_to_string(err: c_int) -> *const c_char;
    pub fn vpx_codec_error_detail(ctx: *mut vpx_codec_ctx) -> *const c_char;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_has_the_size_the_header_implies() {
        // 504 bytes, computed by walking the sixty-one fields of the 1.15.1
        // declaration under x86_64 alignment rules: thirty-seven 4-byte
        // scalars, two 16-byte vpx_fixed_buf (which force 8-byte alignment and
        // the padding around them), six integer arrays totalling 192 bytes, and
        // sixteen vpx_rational at 8 bytes each.
        //
        // An exact figure rather than a floor, because the failure this guards
        // against is a dropped or reordered field, and that shifts everything
        // after it whichever direction the size moves.
        assert_eq!(size_of::<vpx_codec_enc_cfg>(), 504);
        assert_eq!(align_of::<vpx_codec_enc_cfg>(), 8);
    }

    #[test]
    fn the_packet_union_is_at_least_as_large_as_libvpx_writes() {
        // libvpx pads the union to 124 bytes; a smaller one here would let it
        // write past the end of ours.
        assert!(size_of::<vpx_codec_cx_pkt_data>() >= 124);
        assert!(size_of::<vpx_codec_cx_pkt_frame>() <= size_of::<vpx_codec_cx_pkt_data>());
    }

    #[test]
    fn planes_are_where_the_header_puts_them() {
        // Three enums plus nine unsigned ints precede the plane pointers, which
        // is 48 bytes and already 8-aligned — so no padding, and any transcribed
        // field lost before this point would show up here.
        assert_eq!(std::mem::offset_of!(vpx_image, planes), 48);
    }
}
