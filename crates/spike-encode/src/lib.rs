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
}
