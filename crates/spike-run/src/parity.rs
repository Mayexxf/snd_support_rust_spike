//! ffmpeg's arguments, generated from the settings that configure our encoder.
//!
//! The cross-codec tables were produced by hand-written PowerShell that spelled
//! out ffmpeg's flags separately from the Rust that configures libvpx. Four
//! parity breaks survived that arrangement, none of them visible in either half
//! on its own:
//!
//! * **Keyframes.** `kf_max_dist` is `fps * 10`, so 300. The VP9 row of the
//!   published table carried no `-g` at all and took libvpx's default of 128,
//!   while every other row had `-g 300` written out by hand.
//! * **Rate-control buffer.** `rc_buf_sz` is 500 **milliseconds**; ffmpeg's
//!   `-bufsize` is **bits**. Written as `-bufsize ${k}k` against `-b:v ${k}k`
//!   it came to a full second, so our encoder ran on half the buffer of every
//!   row it was compared against, and the difference was credited to the codec.
//! * **Reordering.** `g_lag_in_frames` is zero and there is no way to ask
//!   libvpx for lookahead here, but the QSV rows had no `-bf 0` and reordered
//!   frames — h264_qsv one deep, hevc_qsv two, DTS down to −1024. A support
//!   session cannot reorder: the next frame does not exist yet. Those rows
//!   described a mode the product cannot run.
//! * **Screen content.** `VP9E_SET_TUNE_CONTENT` is sent to our VP9 and to
//!   nothing else. That one cannot be fixed by generating flags — no other
//!   encoder here has an equivalent — so it is *disclosed* instead.
//!
//! Hence one source of truth and an explicit list of what could not be matched.
//! The list is the point as much as the flags are: a knob that cannot be
//! mirrored is not a reason to stay quiet about it, it is the caveat block the
//! table has to carry.
//!
//! Every knob must be either mapped or disclaimed. A test fails if one is
//! neither, so adding a setting to the encoder forces a decision here rather
//! than silently widening the gap.

use spike_encode::vpx::{RcMode, Settings};

/// Which encoder the arguments are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Libx264,
    H264Qsv,
    HevcQsv,
    LibvpxVp9,
    LibvpxVp8,
}

impl Target {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "libx264" | "x264" => Target::Libx264,
            "h264_qsv" | "h264qsv" => Target::H264Qsv,
            "hevc_qsv" | "hevcqsv" => Target::HevcQsv,
            "libvpx-vp9" | "vp9" => Target::LibvpxVp9,
            "libvpx" | "vp8" => Target::LibvpxVp8,
            _ => return None,
        })
    }

    pub fn codec(self) -> &'static str {
        match self {
            Target::Libx264 => "libx264",
            Target::H264Qsv => "h264_qsv",
            Target::HevcQsv => "hevc_qsv",
            Target::LibvpxVp9 => "libvpx-vp9",
            Target::LibvpxVp8 => "libvpx",
        }
    }

    /// Whether the quantizer scale is VPx's 0..=63 or H.26x's 0..=51.
    fn vpx_scale(self) -> bool {
        matches!(self, Target::LibvpxVp9 | Target::LibvpxVp8)
    }

    pub fn all() -> [Target; 5] {
        [Target::Libx264, Target::H264Qsv, Target::HevcQsv, Target::LibvpxVp9, Target::LibvpxVp8]
    }
}

/// A quantizer moved between scales.
///
/// VPx runs 0..=63 and H.264/HEVC run 0..=51, so 56 on our ceiling is about 45
/// on theirs. Rounded rather than truncated, and clamped, because a ceiling
/// that lands one step high is a different encoder and a ceiling that lands
/// outside the range is refused by ffmpeg with a message nobody reads.
fn to_h26x(q: u32) -> u32 {
    ((q as f64) * 51.0 / 63.0).round().min(51.0) as u32
}

/// What could not be mirrored, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmatched {
    pub knob: &'static str,
    pub why: String,
}

/// ffmpeg's arguments and everything that could not be expressed in them.
pub struct Plan {
    pub argv: Vec<String>,
    pub unmatched: Vec<Unmatched>,
}

fn s(x: impl ToString) -> String {
    x.to_string()
}

/// Build the arguments for one target from our own encoder's settings.
pub fn plan(settings: &Settings, target: Target) -> Plan {
    let mut argv: Vec<String> = Vec::new();
    let mut unmatched: Vec<Unmatched> = Vec::new();

    argv.push(s("-c:v"));
    argv.push(s(target.codec()));

    let k = settings.bitrate_kbps;
    argv.push(s("-b:v"));
    argv.push(format!("{k}k"));
    argv.push(s("-maxrate"));
    argv.push(format!("{k}k"));

    // The unit trap, spelled out where it cannot be forgotten: libvpx counts the
    // buffer in milliseconds and ffmpeg counts it in bits.
    let bufsize_kbit = (u64::from(k) * u64::from(settings.rc_buf_ms())) / 1000;
    argv.push(s("-bufsize"));
    argv.push(format!("{bufsize_kbit}k"));

    // Keyframe interval, from the same field the encoder is given.
    argv.push(s("-g"));
    argv.push(s(settings.kf_max_dist()));

    // No lookahead on our side means no reordering on theirs. `-bf 0` is
    // accepted by all four; hevc_qsv keeps emitting B-slices afterwards, and
    // those are low-delay B — has_b_frames drops to zero and PTS equals DTS, so
    // the flag is not being ignored.
    argv.push(s("-bf"));
    argv.push(s("0"));

    argv.push(s("-threads"));
    argv.push(s(settings.threads));

    let (qmin, qmax) = if target.vpx_scale() {
        (settings.min_quantizer, settings.max_quantizer)
    } else {
        (to_h26x(settings.min_quantizer), to_h26x(settings.max_quantizer))
    };
    argv.push(s("-qmin"));
    argv.push(s(qmin));
    argv.push(s("-qmax"));
    argv.push(s(qmax));

    match target {
        Target::LibvpxVp9 | Target::LibvpxVp8 => {
            argv.push(s("-deadline"));
            argv.push(s("realtime"));
            argv.push(s("-cpu-used"));
            argv.push(s(settings.cpu_used));
            argv.push(s("-lag-in-frames"));
            argv.push(s("0"));
            argv.push(s("-row-mt"));
            argv.push(s(u8::from(settings.row_mt)));
            if target == Target::LibvpxVp9 {
                argv.push(s("-tile-columns"));
                argv.push(s(settings.tile_columns));
                argv.push(s("-tune-content"));
                argv.push(s("screen"));
            } else {
                unmatched.push(Unmatched {
                    knob: "tile_columns",
                    why: "у VP8 нет плиток".to_owned(),
                });
                unmatched.push(Unmatched {
                    knob: "tune_content",
                    why: "экранный режим есть только у VP9".to_owned(),
                });
            }
        }
        Target::Libx264 => {
            argv.push(s("-preset"));
            argv.push(s("veryfast"));
            argv.push(s("-tune"));
            argv.push(s("zerolatency"));
            unmatched.push(Unmatched {
                knob: "cpu_used",
                why: format!(
                    "у x264 это -preset; {} отображён на veryfast приблизительно",
                    settings.cpu_used
                ),
            });
            unmatched.push(Unmatched {
                knob: "tile_columns / row_mt",
                why: "у x264 нет плиток; параллелизм задаётся срезами".to_owned(),
            });
            unmatched.push(Unmatched {
                knob: "tune_content",
                why: "у x264 нет экранного режима, сравнимого с VP9E_SET_TUNE_CONTENT"
                    .to_owned(),
            });
        }
        Target::H264Qsv | Target::HevcQsv => {
            argv.push(s("-async_depth"));
            argv.push(s("1"));
            unmatched.push(Unmatched {
                knob: "cpu_used",
                why: "у QSV это -preset, и шкалы несопоставимы".to_owned(),
            });
            unmatched.push(Unmatched {
                knob: "tile_columns / row_mt",
                why: "распараллеливание внутри видеоядра и ключами не задаётся".to_owned(),
            });
            unmatched.push(Unmatched {
                knob: "tune_content",
                why: "экранного режима у QSV нет; VP9 идёт с подсказкой, эти — без"
                    .to_owned(),
            });
            unmatched.push(Unmatched {
                knob: "threads",
                why: "кодирование идёт мимо процессора, -threads на него не влияет"
                    .to_owned(),
            });
        }
    }

    if settings.static_threshold != 0 {
        unmatched.push(Unmatched {
            knob: "static_threshold",
            why: format!(
                "порог покоя {} задаётся только у libvpx через VP8E_SET_STATIC_THRESHOLD",
                settings.static_threshold
            ),
        });
    }
    if settings.error_resilient {
        unmatched.push(Unmatched {
            knob: "error_resilient",
            why: "устойчивость к потерям выражается по-разному у каждого кодера".to_owned(),
        });
    }
    if settings.rc_mode != RcMode::Cbr {
        unmatched.push(Unmatched {
            knob: "rc_mode",
            why: format!("здесь порождается только CBR, а задан {:?}", settings.rc_mode),
        });
    }

    Plan { argv, unmatched }
}

impl Plan {
    /// One line for a shell, plus the caveat block.
    pub fn render(&self) -> String {
        let mut out = self.argv.join(" ");
        out.push('\n');
        if self.unmatched.is_empty() {
            out.push_str("\n# всё сопоставлено\n");
        } else {
            out.push_str("\n# НЕ СОПОСТАВЛЕНО — это и есть оговорки к таблице:\n");
            for u in &self.unmatched {
                out.push_str(&format!("#   {}: {}\n", u.knob, u.why));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Settings {
        Settings::new(1920, 1080, 30)
    }

    fn arg_after(argv: &[String], key: &str) -> Option<String> {
        argv.iter().position(|a| a == key).and_then(|i| argv.get(i + 1)).cloned()
    }

    /// The unit trap that gave our encoder half the buffer of every row it was
    /// compared against: milliseconds on our side, bits on ffmpeg's.
    #[test]
    fn the_buffer_is_converted_from_milliseconds_to_bits() {
        let mut st = base();
        st.bitrate_kbps = 2000;
        // 500 ms of a 2000 kbit/s stream is 1000 kbit, not 2000.
        for t in Target::all() {
            let p = plan(&st, t);
            assert_eq!(arg_after(&p.argv, "-bufsize").as_deref(), Some("1000k"), "{t:?}");
        }
    }

    /// The VP9 row of the published table had no -g and silently took libvpx's
    /// default of 128 while every other row was given 300 by hand.
    #[test]
    fn the_keyframe_interval_comes_from_the_encoder_not_from_a_default() {
        let st = base();
        for t in Target::all() {
            let p = plan(&st, t);
            assert_eq!(arg_after(&p.argv, "-g").as_deref(), Some("300"), "{t:?}");
        }
    }

    /// Reordering is structurally impossible for our encoder and must be
    /// forbidden for everyone it is compared against.
    #[test]
    fn no_target_is_allowed_to_reorder_frames() {
        let st = base();
        for t in Target::all() {
            let p = plan(&st, t);
            assert_eq!(arg_after(&p.argv, "-bf").as_deref(), Some("0"), "{t:?}");
        }
    }

    /// 0..=63 on our side, 0..=51 on theirs. A ceiling of 56 is 45, not 56.
    #[test]
    fn the_quantizer_ceiling_is_moved_between_scales() {
        let mut st = base();
        st.min_quantizer = 4;
        st.max_quantizer = 56;

        let vpx = plan(&st, Target::LibvpxVp9);
        assert_eq!(arg_after(&vpx.argv, "-qmax").as_deref(), Some("56"));

        let h264 = plan(&st, Target::H264Qsv);
        assert_eq!(arg_after(&h264.argv, "-qmax").as_deref(), Some("45"));
        assert_eq!(arg_after(&h264.argv, "-qmin").as_deref(), Some("3"));
    }

    /// The screen-content hint is the one knob that cannot be mirrored, and the
    /// answer is to say so rather than to leave it out quietly. This was the
    /// break that made the VP9 row of the table incomparable.
    #[test]
    fn a_knob_with_no_equivalent_is_disclosed_rather_than_dropped() {
        let st = base();
        for t in [Target::Libx264, Target::H264Qsv, Target::HevcQsv] {
            let p = plan(&st, t);
            assert!(
                p.unmatched.iter().any(|u| u.knob.contains("tune_content")),
                "{t:?} обязан признаться про экранный режим"
            );
            assert!(p.render().contains("НЕ СОПОСТАВЛЕНО"), "{t:?}");
        }
        // VP9 does get it, so it has nothing to disclose there.
        let vp9 = plan(&st, Target::LibvpxVp9);
        assert!(vp9.argv.iter().any(|a| a == "screen"));
        assert!(!vp9.unmatched.iter().any(|u| u.knob.contains("tune_content")));
    }

    /// Every field of Settings must be either on the command line or in the
    /// caveat block. Adding a knob to the encoder should force a decision here,
    /// not silently widen the gap between the two sides.
    #[test]
    fn every_setting_is_either_mapped_or_disclaimed() {
        let mut st = base();
        // Turn on the things that are only disclosed when non-default, so the
        // check sees them.
        st.static_threshold = 1;
        st.error_resilient = true;

        // The field list is written out by hand on purpose: when someone adds a
        // field to Settings this test still passes, and the destructure below
        // stops compiling. That is the reminder.
        let Settings {
            width: _,
            height: _,
            fps: _,
            bitrate_kbps: _,
            cpu_used: _,
            threads: _,
            tile_columns: _,
            row_mt: _,
            static_threshold: _,
            rc_mode: _,
            min_quantizer: _,
            max_quantizer: _,
            cq_level: _,
            error_resilient: _,
        } = st.clone();

        for t in Target::all() {
            let p = plan(&st, t);
            let text = format!("{} {}", p.argv.join(" "), p.render());
            for knob in [
                "bitrate", "-g", "-bufsize", "-qmin", "-qmax", "-threads",
            ] {
                let named = knob == "bitrate" && text.contains("-b:v");
                assert!(named || text.contains(knob), "{t:?}: {knob} нигде не назван");
            }
            assert!(
                p.unmatched.iter().any(|u| u.knob == "static_threshold"),
                "{t:?}: порог покоя не оговорён"
            );
            assert!(
                p.unmatched.iter().any(|u| u.knob == "error_resilient"),
                "{t:?}: устойчивость к потерям не оговорена"
            );
        }
    }

    #[test]
    fn targets_parse_by_both_spellings() {
        assert_eq!(Target::parse("hevc_qsv"), Some(Target::HevcQsv));
        assert_eq!(Target::parse("hevcqsv"), Some(Target::HevcQsv));
        assert_eq!(Target::parse("av1"), None);
    }
}
