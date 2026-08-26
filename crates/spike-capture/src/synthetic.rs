//! A fake desktop that runs on any OS.
//!
//! Two jobs. First, it lets the whole harness — pacing, accounting, percentiles,
//! report — be developed and tested on the development Mac, where no Windows
//! capture API exists. Second, on a VM where desktop duplication refuses to
//! start, it keeps the encoder measurable: `--source synthetic` still produces
//! frames to encode.
//!
//! It is not a substitute for a real screen and the report never pretends
//! otherwise. There is no GPU readback stage here, so that line is absent from
//! synthetic runs rather than filled with a flattering zero.

use std::time::{Duration, Instant};

use crate::{CaptureError, Dirty, Frame, FrameSource, Readback, Rect};

/// How much of the fake desktop moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// Nothing changes after the first frame. The idle case, and the one the
    /// whole encoding budget rests on.
    Still,
    /// A small block moves — a caret blinking, a mouse dragging.
    Cursor,
    /// A large band redraws every frame — a document being scrolled.
    Scroll,
    /// Every pixel changes. The worst case, and not a realistic support session.
    Full,
}

impl Motion {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "still" => Motion::Still,
            "cursor" => Motion::Cursor,
            "scroll" => Motion::Scroll,
            "full" => Motion::Full,
            _ => return None,
        })
    }
}

pub struct SyntheticSource {
    width: u32,
    height: u32,
    motion: Motion,
    interval: Duration,
    buf: Vec<u8>,
    frame_no: u64,
    next_due: Option<Instant>,
}

impl SyntheticSource {
    pub fn new(width: u32, height: u32, motion: Motion, target_fps: u32) -> Self {
        let stride = width as usize * 4;
        Self {
            width,
            height,
            motion,
            interval: Duration::from_secs_f64(1.0 / target_fps.max(1) as f64),
            buf: vec![0u8; stride * height as usize],
            frame_no: 0,
            next_due: None,
        }
    }

    /// Regions that change on frame `n`. Empty means a still screen.
    fn dirty_for(&self, n: u64) -> Vec<Rect> {
        let (w, h) = (self.width as i32, self.height as i32);
        if n == 0 {
            return vec![Rect { left: 0, top: 0, right: w, bottom: h }];
        }
        match self.motion {
            Motion::Still => Vec::new(),
            Motion::Cursor => {
                // A 32×32 block tracking a slow diagonal, wrapped into frame.
                let step = (n as i32 * 7) % (w - 32).max(1);
                let top = (n as i32 * 3) % (h - 32).max(1);
                vec![Rect { left: step, top, right: step + 32, bottom: top + 32 }]
            }
            Motion::Scroll => {
                // A band covering roughly 60% of the height, as a scrolled text
                // area redraws.
                let band = (h as f64 * 0.6) as i32;
                let top = ((n as i32 * 13) % (h - band).max(1)).max(0);
                vec![Rect { left: 0, top, right: w, bottom: top + band }]
            }
            Motion::Full => vec![Rect { left: 0, top: 0, right: w, bottom: h }],
        }
    }

    /// Write a frame-dependent pattern into the dirty regions.
    ///
    /// Deliberately touches the same bytes a real readback would, so a synthetic
    /// run is not free in a way that hides memory bandwidth entirely.
    fn paint(&mut self, rects: &[Rect]) {
        let stride = self.width as usize * 4;
        let tint = (self.frame_no % 251) as u8;
        for r in rects {
            let x0 = r.left.max(0) as usize;
            let x1 = (r.right.max(0) as usize).min(self.width as usize);
            let y0 = r.top.max(0) as usize;
            let y1 = (r.bottom.max(0) as usize).min(self.height as usize);
            for y in y0..y1 {
                let row = y * stride;
                for x in x0..x1 {
                    let i = row + x * 4;
                    self.buf[i] = tint;
                    self.buf[i + 1] = (x as u8).wrapping_add(tint);
                    self.buf[i + 2] = (y as u8).wrapping_sub(tint);
                    self.buf[i + 3] = 0xFF;
                }
            }
        }
    }
}

impl FrameSource for SyntheticSource {
    fn describe(&self) -> String {
        format!(
            "синтетический источник ({:?}, {}×{}) — НЕ настоящий экран",
            self.motion, self.width, self.height
        )
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
        readback: Readback,
    ) -> Result<Option<Frame<'_>>, CaptureError> {
        let rects = self.dirty_for(self.frame_no);

        // A still screen must cost the caller the same wait a real timeout would,
        // otherwise the runner spins and the CPU figure becomes nonsense.
        if rects.is_empty() {
            std::thread::sleep(timeout);
            return Ok(None);
        }

        let now = Instant::now();
        let due = self.next_due.unwrap_or(now);
        let waited = if let Some(wait) = due.checked_duration_since(now) {
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
            wait
        } else {
            Duration::ZERO
        };
        self.next_due = Some(due + self.interval);

        // Painting is this source's stand-in for a readback: it is the stage that
        // moves a frame's worth of bytes through memory. Timed as such, and named
        // as such in the report, so nobody reads it as a GPU measurement.
        //
        // The full/dirty distinction is honoured here too, so the two paths can
        // be compared on a machine with no real desktop to capture.
        let full = [Rect { left: 0, top: 0, right: self.width as i32, bottom: self.height as i32 }];
        let painted: &[Rect] = match readback {
            Readback::Off => &[],
            Readback::Full => &full,
            Readback::Dirty | Readback::Compare => &rects,
        };
        let paint_start = Instant::now();
        self.paint(painted);
        let painted_us = paint_start.elapsed().as_micros() as u64;
        let copied_px = painted.iter().map(Rect::area).sum();

        // In comparison mode the full path is timed on the same frame, exactly
        // as the real backend does it.
        let compare_us = (readback == Readback::Compare).then(|| {
            let started = Instant::now();
            self.paint(&full);
            started.elapsed().as_micros() as u64
        });
        self.frame_no += 1;

        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            stride: self.width as usize * 4,
            bgra: readback.wants_pixels().then_some(self.buf.as_slice()),
            dirty: Dirty::Rects(rects),
            wait_us: waited.as_micros() as u64,
            // No GPU is involved, so there is no capture work beyond producing
            // the rectangles, which is free here.
            work_us: 0,
            readback_us: painted_us,
            copied_px,
            compare_us,
        }))
    }

    fn stand_in(&self) -> Option<String> {
        Some(
            concat!(
                "Это нарисованная картинка, а не экран: ни захвата,\n",
                "ни копирования из памяти GPU здесь нет вовсе."
            )
            .to_owned(),
        )
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        self.frame_no = 0;
        self.next_due = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_always_a_full_repaint() {
        let mut src = SyntheticSource::new(640, 480, Motion::Still, 30);
        let frame = src.next_frame(Duration::from_millis(50), Readback::Dirty).unwrap().unwrap();
        assert_eq!(frame.dirty.area(640, 480), 640 * 480);
        assert_eq!(frame.bgra.map(<[u8]>::len), Some(640 * 480 * 4));
    }

    #[test]
    fn a_still_screen_reports_no_new_frame() {
        let mut src = SyntheticSource::new(320, 240, Motion::Still, 60);
        assert!(src.next_frame(Duration::from_millis(5), Readback::Off).unwrap().is_some());
        // Every later poll must come back empty — this is the path that produces
        // the "экран не менялся" share, the number the encoding budget rests on.
        for _ in 0..3 {
            assert!(src.next_frame(Duration::from_millis(1), Readback::Off).unwrap().is_none());
        }
    }

    #[test]
    fn cursor_motion_dirties_a_small_area_scroll_a_large_one() {
        let (w, h) = (1920, 1080);
        let full = u64::from(w) * u64::from(h);

        let mut cursor = SyntheticSource::new(w, h, Motion::Cursor, 240);
        cursor.next_frame(Duration::from_millis(20), Readback::Off).unwrap();
        let f = cursor.next_frame(Duration::from_millis(20), Readback::Off).unwrap().unwrap();
        assert_eq!(f.dirty.area(w, h), 32 * 32);

        let mut scroll = SyntheticSource::new(w, h, Motion::Scroll, 240);
        scroll.next_frame(Duration::from_millis(20), Readback::Off).unwrap();
        let f = scroll.next_frame(Duration::from_millis(20), Readback::Off).unwrap().unwrap();
        let share = f.dirty.area(w, h) as f64 / full as f64;
        assert!((0.55..0.65).contains(&share), "доля {share}");
    }

    #[test]
    fn readback_off_returns_no_pixels() {
        let mut src = SyntheticSource::new(64, 64, Motion::Full, 60);
        let f = src.next_frame(Duration::from_millis(20), Readback::Off).unwrap().unwrap();
        assert!(f.bgra.is_none());
    }

    #[test]
    fn painting_actually_writes_the_dirty_region() {
        let mut src = SyntheticSource::new(64, 64, Motion::Full, 240);
        src.next_frame(Duration::from_millis(20), Readback::Full).unwrap();
        let f = src.next_frame(Duration::from_millis(20), Readback::Full).unwrap().unwrap();
        let px = f.bgra.unwrap();
        // Alpha is written opaque everywhere the painter touched.
        assert!(px.chunks_exact(4).all(|p| p[3] == 0xFF));
    }

    #[test]
    fn reinit_restarts_from_a_full_frame() {
        let mut src = SyntheticSource::new(128, 128, Motion::Still, 60);
        src.next_frame(Duration::from_millis(5), Readback::Off).unwrap();
        assert!(src.next_frame(Duration::from_millis(1), Readback::Off).unwrap().is_none());

        src.reinit().unwrap();
        let f = src.next_frame(Duration::from_millis(5), Readback::Off).unwrap().unwrap();
        assert_eq!(f.dirty.area(128, 128), 128 * 128);
    }
}
