//! Colour conversion and video encoding for the phase-0 harness.
//!
//! [`convert`] is platform-independent and under test on any host. The encoder
//! sits behind the `vpx` feature, because libvpx needs vcpkg, nasm and LLVM on
//! the build machine and none of that should stand between someone and a
//! capture measurement.

pub mod convert;

#[cfg(feature = "vpx")]
pub mod vpx;

/// One encoded frame.
#[derive(Debug, Clone, Copy)]
pub struct Encoded {
    pub bytes: usize,
    pub keyframe: bool,
    /// The encoder took the frame and emitted no packet for it.
    ///
    /// Worth its own flag because `bytes == 0` cannot say it. A frame libvpx
    /// dropped and a frame it coded into nothing arrive here identically, and
    /// the difference is the whole question phase 0 asks: a run delivering
    /// twelve frames a second because the screen was quiet and a run delivering
    /// twelve because it could not keep up look the same in every other number
    /// this harness prints.
    ///
    /// Always `false` today — rate control is configured with
    /// `rc_dropframe_thresh` at its default of zero, so libvpx never drops. The
    /// flag exists so that turning dropping on cannot be mistaken for the screen
    /// going quiet.
    pub dropped: bool,
    /// The quantizer the encoder actually chose for this frame, 0..=63.
    ///
    /// `None` for a dropped frame, and on any codec that will not answer the
    /// control. Reported because every conclusion this harness has reached about
    /// rate control was reached by watching byte counts and reasoning backwards
    /// to a number nobody was reading.
    pub quantizer: Option<u8>,
}

/// Which encoder to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// No encoding. Measures capture and conversion alone.
    #[default]
    None,
    /// Cheapest of the three, and the fallback for the weakest machines.
    Vp8,
    /// The planned baseline: better on screen content at a similar cost.
    Vp9,
}

impl Codec {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Codec::None,
            "vp8" => Codec::Vp8,
            "vp9" => Codec::Vp9,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::None => "без кодирования",
            Codec::Vp8 => "VP8",
            Codec::Vp9 => "VP9",
        }
    }

    /// The four bytes that name this codec inside a container.
    ///
    /// Lives here rather than next to the writer that needs it because the
    /// writer must not depend on the `vpx` feature, and this enum already does
    /// not. `None` for [`Codec::None`], which has no bitstream to name.
    pub fn fourcc(self) -> Option<[u8; 4]> {
        match self {
            Codec::None => None,
            Codec::Vp8 => Some(*b"VP80"),
            Codec::Vp9 => Some(*b"VP90"),
        }
    }
}
