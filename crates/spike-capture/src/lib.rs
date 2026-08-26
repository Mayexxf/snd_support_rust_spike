//! Frame sources for the phase-0 harness.
//!
//! One trait, three implementations, chosen at runtime:
//!
//! * [`synthetic::SyntheticSource`] — a fake desktop, works on any OS. It exists
//!   so the harness can be proven end to end before it ever meets a Windows box,
//!   and so the encoder can still be measured on a VM where desktop duplication
//!   is unavailable.
//! * `DdaSource` — DXGI Desktop Duplication. The real answer, and the only one
//!   that reports which parts of the screen changed.
//! * `GdiSource` — `BitBlt`. Slow and blind to change, but it works where DDA
//!   does not, which on a VM is often.
//!
//! **Failure posture.** `AccessLost` is not an error the run dies on. On a live
//! machine it fires whenever the desktop switches — a UAC prompt, Ctrl+Alt+Del,
//! the lock screen, a resolution change — and how often that happens is one of
//! the questions phase 0 was set up to answer. The runner counts it and
//! reinitialises.

pub mod image;
pub mod shot;
pub mod synthetic;

#[cfg(windows)]
pub mod dda;
#[cfg(windows)]
pub mod gdi;

use std::time::Duration;

/// A rectangle in screen coordinates, right/bottom exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        let w = (self.right - self.left).max(0) as u64;
        let h = (self.bottom - self.top).max(0) as u64;
        w * h
    }
}

/// What the backend can say about which pixels changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dirty {
    /// Regions the backend reported. May overlap; the harness does not
    /// deduplicate, and says so in the report.
    Rects(Vec<Rect>),
    /// The backend cannot tell. Accounted as the whole screen, which is the
    /// honest reading: a blind backend forces the encoder to treat it that way.
    Unknown,
}

impl Dirty {
    /// Total changed area, counting overlaps twice.
    pub fn area(&self, width: u32, height: u32) -> u64 {
        match self {
            Dirty::Rects(rects) => rects.iter().map(Rect::area).sum(),
            Dirty::Unknown => u64::from(width) * u64::from(height),
        }
    }

    pub fn count(&self) -> u32 {
        match self {
            Dirty::Rects(rects) => rects.len() as u32,
            Dirty::Unknown => 1,
        }
    }
}

/// How much of the frame to move out of GPU memory.
///
/// Selectable rather than fixed because the first Windows run made the question
/// worth measuring: capture work cost 0.1 ms and the copy cost 6.9 ms, while the
/// same run reported that only 27% of the screen had changed. Three quarters of
/// every copy was being thrown away.
///
/// Whether copying the changed regions instead is actually faster is not
/// obvious — many small transfers can lose to one large one, and drivers
/// optimise the whole-resource path. So both paths stay in the harness and the
/// target machine decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Readback {
    /// Do not touch the pixels. Measures capture alone.
    Off,
    /// Copy the whole frame every time.
    Full,
    /// Copy only the regions the backend reported as changed.
    #[default]
    Dirty,
    /// Do both, on the same frame, and time them separately.
    ///
    /// Two separate runs cannot settle this question on a live desktop: nobody
    /// scrolls twice the same way, so the two runs see different content, differ
    /// in poll count, and the slower path accumulates more change per frame. The
    /// first attempt produced 495 polls against 972 and matched on changed area
    /// only by luck.
    ///
    /// Here both paths copy the *same* acquired frame, so the comparison holds
    /// whatever the operator did with the mouse. The order alternates by frame
    /// so neither path always pays the cold-cache cost.
    ///
    /// Costs two copies per frame, so the achieved frame rate of a comparison
    /// run means nothing. The per-frame timings are the output.
    Compare,
}

impl Readback {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => Readback::Off,
            "full" => Readback::Full,
            "dirty" => Readback::Dirty,
            "compare" => Readback::Compare,
            _ => return None,
        })
    }

    pub fn wants_pixels(self) -> bool {
        self != Readback::Off
    }
}

/// One captured frame. Borrows the source's readback buffer.
#[derive(Debug)]
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, which is not always `width * 4`.
    pub stride: usize,
    /// BGRA pixels, present only when readback was requested.
    pub bgra: Option<&'a [u8]>,
    pub dirty: Dirty,
    /// Microseconds blocked waiting for the screen to produce a frame.
    ///
    /// Kept apart from `work_us` because it is not a cost. A source capped at
    /// 30 fps spends most of its time here by design, and folding the two
    /// together makes an idle desktop look like an overloaded one — which is
    /// precisely the wrong conclusion to draw about the target machine.
    pub wait_us: u64,
    /// Microseconds of actual capture work once a frame was available:
    /// metadata, dirty rectangles, handover. This one is a cost.
    pub work_us: u64,
    /// Microseconds spent copying pixels out of GPU memory into system memory.
    pub readback_us: u64,
    /// Pixels actually moved this frame.
    ///
    /// Reported separately from `dirty` because they are not the same number:
    /// a backend may be asked for the changed regions and still copy everything,
    /// either because it cannot do better or because the frame was the first
    /// after a reset. This is what was really paid for.
    pub copied_px: u64,
    /// In [`Readback::Compare`], what the whole-frame path cost on this same
    /// frame. `None` in every other mode.
    pub compare_us: Option<u64>,
}

#[derive(Debug)]
pub enum CaptureError {
    /// The desktop went away. Reinitialise and carry on.
    AccessLost,
    /// This backend cannot run here at all.
    Unavailable(String),
    Failed(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::AccessLost => write!(f, "доступ к рабочему столу потерян"),
            CaptureError::Unavailable(why) => write!(f, "источник недоступен: {why}"),
            CaptureError::Failed(why) => write!(f, "сбой захвата: {why}"),
        }
    }
}

impl std::error::Error for CaptureError {}

pub trait FrameSource {
    /// Operator-facing description, printed in the report header.
    fn describe(&self) -> String;

    fn dimensions(&self) -> (u32, u32);

    /// Wait up to `timeout` for a frame.
    ///
    /// `Ok(None)` means the screen did not change — not a failure, and the single
    /// most important measurement of the run.
    fn next_frame(
        &mut self,
        timeout: Duration,
        readback: Readback,
    ) -> Result<Option<Frame<'_>>, CaptureError>;

    /// Rebuild after [`CaptureError::AccessLost`].
    fn reinit(&mut self) -> Result<(), CaptureError>;

    /// Reasons this source's numbers do not describe the machine we care about.
    ///
    /// Separate from [`FrameSource::describe`] because these have to be shouted,
    /// not mentioned. A capture running on a software rasteriser produces
    /// perfectly plausible timings that mean nothing about a real GPU, and a
    /// plausible wrong number is worse than a missing one.
    fn caveats(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_area_ignores_inverted_rectangles() {
        assert_eq!(Rect { left: 0, top: 0, right: 10, bottom: 10 }.area(), 100);
        // A backend that hands us a degenerate rect must not make the total
        // negative or panic on the cast.
        assert_eq!(Rect { left: 10, top: 10, right: 0, bottom: 0 }.area(), 0);
    }

    #[test]
    fn unknown_dirty_counts_as_the_whole_screen() {
        assert_eq!(Dirty::Unknown.area(1920, 1080), 1920 * 1080);
        assert_eq!(Dirty::Unknown.count(), 1);
    }

    #[test]
    fn readback_modes_parse_and_off_means_no_pixels() {
        assert_eq!(Readback::parse("dirty"), Some(Readback::Dirty));
        assert_eq!(Readback::parse("full"), Some(Readback::Full));
        assert_eq!(Readback::parse("off"), Some(Readback::Off));
        assert_eq!(Readback::parse("compare"), Some(Readback::Compare));
        assert_eq!(Readback::parse("частично"), None);
        assert!(Readback::Compare.wants_pixels());
        assert!(!Readback::Off.wants_pixels());
        assert!(Readback::Full.wants_pixels());
        assert!(Readback::Dirty.wants_pixels());
        // Dirty is the default: it is the path the product would ship.
        assert_eq!(Readback::default(), Readback::Dirty);
    }

    #[test]
    fn rect_areas_sum_including_overlap() {
        let d = Dirty::Rects(vec![
            Rect { left: 0, top: 0, right: 10, bottom: 10 },
            Rect { left: 5, top: 5, right: 15, bottom: 15 },
        ]);
        // Overlap is deliberately counted twice: deduplicating would flatter the
        // backend, and the encoder pays for the rectangles it is handed.
        assert_eq!(d.area(1920, 1080), 200);
        assert_eq!(d.count(), 2);
    }
}
