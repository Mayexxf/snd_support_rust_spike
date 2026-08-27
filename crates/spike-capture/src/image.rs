//! A real screenshot, moved on a fixed script.
//!
//! The pipeline below this point — conversion, encoder, metrics — is handed two
//! things per frame: a BGRA buffer and a list of changed rectangles. Desktop
//! duplication produces them from a live screen. This source produces the same
//! two things from one still image plus arithmetic on the frame number, and
//! nothing downstream can tell the difference.
//!
//! What that buys is the one thing a live screen cannot give: **the same frames
//! twice.** Nobody scrolls a document the same way on two machines, so a number
//! from the VM and a number from the Celeron currently cannot be divided by one
//! another. Here they can.
//!
//! What it does not buy is realism in the motion. Real scrolling is not a pure
//! translation: the scrollbar thumb moves, sticky headers stay put, and text
//! antialiasing is re-rendered slightly differently at a new offset. Real frames
//! are therefore a little more expensive than these. The texture is real — real
//! glyphs, real antialiasing, real flat panels — because it came off a real
//! screen; only the motion is ours.
//!
//! So this source answers "how much slower is that machine than this one" and
//! "did that change help". It does not answer "does the target keep up". That
//! one is still settled on a live desktop, on the machine in question.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::shot::{Fingerprint, Shot};
use crate::{CaptureError, Dirty, Frame, FrameSource, Readback, Rect};

/// What happens to the image between frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Nothing after the first frame. A support session is mostly this, and it
    /// is the case the whole encoding budget rests on.
    Still,
    /// A caret blinks: about forty pixels, twice a second, nothing in between.
    /// The cheapest frame that is still a frame.
    Caret,
    /// A small region is rewritten every frame — a field being filled in, a
    /// counter ticking. Small dirty rectangle, real content inside it.
    Edit,
    /// A document window scrolls. The expensive case, and the one a support
    /// session actually spends its busy moments in.
    Scroll,
    /// A dialog is dragged across the desktop: two medium rectangles per frame,
    /// where it was and where it is.
    Drag,
    /// Scrolled, then stopped, over and over. The one a support session is
    /// actually made of, and the only one where "is the screen readable" is a
    /// question with an answer.
    ///
    /// [`Scenario::Scroll`] never stops, and at speed no encoder keeps text
    /// legible — measured: every codec tried lands between 58% and 70% of
    /// stroke pixels damaged in its worst frames, whatever the bitrate. So a
    /// readability bar cannot live there. [`Scenario::Still`] is the opposite
    /// and just as useless: after the keyframe every codec is perfect.
    ///
    /// What a person actually does is scroll to a place and then read it, and
    /// what matters is how many frames the text takes to come back once the
    /// motion ends. Bursts rather than one stop, so a run holds several
    /// settling events and the figure can have a spread instead of being one
    /// observation.
    Settle,
}

impl Scenario {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "still" => Scenario::Still,
            "caret" => Scenario::Caret,
            "edit" => Scenario::Edit,
            "scroll" => Scenario::Scroll,
            "drag" => Scenario::Drag,
            "settle" => Scenario::Settle,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Still => "still",
            Scenario::Caret => "caret",
            Scenario::Edit => "edit",
            Scenario::Scroll => "scroll",
            Scenario::Drag => "drag",
            Scenario::Settle => "settle",
        }
    }
}

/// Frames of scrolling in one settle cycle, then frames of holding still.
///
/// Twenty at 30 fps is two thirds of a second of scrolling — about one flick of
/// a wheel — and forty is the one and a third seconds after it, which is long
/// enough that a codec still smearing at the end of it is not settling at all.
/// A 300-frame run holds five of these.
pub const SETTLE_SCROLL: u64 = 20;
pub const SETTLE_HOLD: u64 = 40;
pub const SETTLE_CYCLE: u64 = SETTLE_SCROLL + SETTLE_HOLD;

/// Frames between caret toggles. Fifteen at 30 fps is a blink twice a second,
/// which is what Windows does.
const CARET_PERIOD: u64 = 15;

/// Default scroll step, in pixels per frame.
///
/// A wheel notch moves about this far, and the size matters more than it looks:
/// a translation the encoder's motion search can follow is nearly free, and one
/// it cannot is coded from scratch. Exposed so the sensitivity can be measured
/// rather than assumed.
pub const DEFAULT_STEP: u32 = 60;

pub struct ImageSource {
    shot: Shot,
    name: String,
    fingerprint: Fingerprint,
    scenario: Scenario,
    step: u32,
    /// The frame handed out, always tightly packed.
    buf: Vec<u8>,
    frame_no: u64,
    /// `None` runs flat out, for a fixed-frame benchmark where pacing would only
    /// add idle time and hide how fast the machine really is.
    interval: Option<Duration>,
    next_due: Option<Instant>,
    /// The "document window" that scrolls. Everything outside it holds still,
    /// as the taskbar and window chrome of a real desktop do.
    window: Rect,
    dialog: Rect,
    edit: Rect,
    caret: Rect,
}

impl ImageSource {
    pub fn open(
        path: &Path,
        scenario: Scenario,
        step: u32,
        interval: Option<Duration>,
    ) -> Result<Self, CaptureError> {
        let shot = Shot::load(path).map_err(CaptureError::Unavailable)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Ok(Self::from_shot(shot, name, scenario, step, interval))
    }

    pub fn from_shot(
        shot: Shot,
        name: String,
        scenario: Scenario,
        step: u32,
        interval: Option<Duration>,
    ) -> Self {
        let fingerprint = shot.fingerprint();
        let (w, h) = (shot.width as i32, shot.height as i32);

        // The scrolling window covers 36% of the screen. Not an arbitrary
        // fraction: the live runs on the VM reported 32–38% of the screen
        // changing per frame while a document was scrolled, and a stand-in that
        // moved the whole screen would be measuring a different workload.
        let window = centred(w, h, 0.60, 0.60);
        let dialog = centred(w, h, 0.28, 0.32);
        let edit = Rect {
            left: window.left + 24,
            top: window.top + 24,
            right: (window.left + 264).min(w),
            bottom: (window.top + 48).min(h),
        };
        let caret = Rect {
            left: edit.left + 8,
            top: edit.top + 2,
            right: edit.left + 10,
            bottom: (edit.top + 22).min(h),
        };

        let buf = vec![0u8; shot.bgra.len()];
        let mut src = Self {
            shot,
            name,
            fingerprint,
            scenario,
            step: step.max(1),
            buf,
            frame_no: 0,
            interval,
            next_due: None,
            window,
            dialog,
            edit,
            caret,
        };
        // Frame zero is the untouched image; every scenario starts from there.
        let whole = src.whole();
        src.render(&[whole]);
        src
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// How many distinct pictures a scrolling scenario can ever show.
    ///
    /// The window wraps within itself, so the offset advances modulo the window
    /// height and comes back round after `win_h / gcd(step, win_h)` steps. At
    /// the default 60 pixels into a 648-pixel window that is **54** — a
    /// 300-frame run is fifty-four pictures shown five and a half times each,
    /// and by the third repeat the encoder has every one of them in its
    /// reference frames.
    ///
    /// That is not a defect, it is a property, and it was invisible: the settle
    /// measurement showed later stops resolving instantly and the reason was
    /// this, not the codec. Printed so nobody has to rediscover it. A step
    /// coprime to the window height — 61 rather than 60 — visits all 648.
    ///
    /// `None` for scenarios that do not scroll.
    pub fn period(&self) -> Option<u32> {
        if !matches!(self.scenario, Scenario::Scroll | Scenario::Settle) {
            return None;
        }
        let win_h = (self.window.bottom - self.window.top).max(1) as u32;
        Some(win_h / gcd(self.step % win_h.max(1), win_h))
    }

    fn whole(&self) -> Rect {
        Rect { left: 0, top: 0, right: self.shot.width as i32, bottom: self.shot.height as i32 }
    }

    /// Where the dialog sits on frame `n`, moving on a diagonal and bouncing off
    /// the edges. Bouncing rather than wrapping so it never teleports: a jump
    /// across the screen is not a drag, and would be coded as one enormous
    /// change.
    fn dialog_at(&self, n: u64) -> Rect {
        let (w, h) = (self.shot.width as i32, self.shot.height as i32);
        let (dw, dh) = (self.dialog.right - self.dialog.left, self.dialog.bottom - self.dialog.top);
        let left = bounce(n as i32 * 8, (w - dw).max(1));
        let top = bounce(n as i32 * 5, (h - dh).max(1));
        Rect { left, top, right: left + dw, bottom: top + dh }
    }

    /// Regions that differ from the previous frame. Empty means a still screen,
    /// which is not a failure but the most important reading in the report.
    fn dirty_for(&self, n: u64) -> Vec<Rect> {
        if n == 0 {
            return vec![self.whole()];
        }
        match self.scenario {
            Scenario::Still => Vec::new(),
            Scenario::Caret => {
                if n.is_multiple_of(CARET_PERIOD) {
                    vec![self.caret]
                } else {
                    Vec::new()
                }
            }
            Scenario::Edit => vec![self.edit],
            Scenario::Scroll => vec![self.window],
            // Dirty exactly while the offset is moving, and the two are worked
            // out from the same rule so they cannot drift apart: a frame
            // reported as changed that rendered identically would be handed to
            // the encoder as work it did not have to do, and the settle curve
            // would start one frame late.
            Scenario::Settle => {
                if settle_offset_steps(n) != settle_offset_steps(n - 1) {
                    vec![self.window]
                } else {
                    Vec::new()
                }
            }
            // Where it was and where it is. Desktop duplication reports a move
            // this way too, and the two overlap heavily at eight pixels a frame.
            Scenario::Drag => vec![self.dialog_at(n - 1), self.dialog_at(n)],
        }
    }

    /// Draw the current frame's state into the given regions.
    ///
    /// Everything the scenario does is expressed here as copies out of the
    /// still image, so every pixel the encoder sees came off a real screen.
    fn render(&mut self, rects: &[Rect]) {
        let (w, h) = (self.shot.width as usize, self.shot.height as usize);
        let row = w * 4;
        // Everything derived from `self` is worked out before the buffer is
        // borrowed mutably, so the overlays below can still see the geometry.
        let scenario = self.scenario;
        let frame_no = self.frame_no;
        let dialog_from = self.dialog;
        let dialog_to = self.dialog_at(frame_no);
        let edit_to = self.edit;
        let caret = self.caret;
        let win = self.window;

        let src = &self.shot.bgra;
        let dst = &mut self.buf;
        let win_h = (win.bottom - win.top).max(1);
        let offset = match scenario {
            Scenario::Scroll => (frame_no.wrapping_mul(u64::from(self.step)) % win_h as u64) as i32,
            Scenario::Settle => {
                let steps = settle_offset_steps(frame_no);
                (steps.wrapping_mul(u64::from(self.step)) % win_h as u64) as i32
            }
            _ => 0,
        };

        for r in rects {
            let x0 = r.left.clamp(0, w as i32) as usize;
            let x1 = r.right.clamp(0, w as i32) as usize;
            let y0 = r.top.clamp(0, h as i32) as usize;
            let y1 = r.bottom.clamp(0, h as i32) as usize;
            if x0 >= x1 {
                continue;
            }
            let wl = (win.left.clamp(0, w as i32) as usize).clamp(x0, x1);
            let wr = (win.right.clamp(0, w as i32) as usize).clamp(x0, x1);
            for y in y0..y1 {
                let scrolls = matches!(scenario, Scenario::Scroll | Scenario::Settle)
                    && (y as i32) >= win.top
                    && (y as i32) < win.bottom;
                if !scrolls {
                    span(src, dst, row, y, y, x0, x1);
                    continue;
                }
                // The row is split at the window edges: only the part inside the
                // window scrolls. Splitting rather than testing the whole span
                // matters because a full-frame redraw asks for x0..width in one
                // piece, and a test on the whole span would quietly leave the
                // window unscrolled — a still picture reported as a scroll.
                //
                // Inside the window the row is taken from further down the
                // image, wrapping within the window so the content is always
                // real pixels rather than one edge repeated.
                let local = (y as i32 - win.top + offset).rem_euclid(win_h);
                let sy = (win.top + local) as usize;
                span(src, dst, row, y, y, x0, wl);
                span(src, dst, row, sy, y, wl, wr);
                span(src, dst, row, y, y, wr, x1);
            }
        }

        // Overlays go on top of whatever the base composition put down.
        match scenario {
            Scenario::Drag => blit(src, dst, w, h, dialog_from, dialog_to, rects),
            Scenario::Edit => {
                // The strip is filled from a slowly travelling place in the
                // image, so its content genuinely differs frame to frame instead
                // of being the same pixels rewritten.
                let travel = (frame_no as i32 * 7).rem_euclid((h as i32 - 64).max(1));
                let from = Rect {
                    left: edit_to.left,
                    top: travel,
                    right: edit_to.right,
                    bottom: travel + (edit_to.bottom - edit_to.top),
                };
                blit(src, dst, w, h, from, edit_to, rects);
            }
            Scenario::Caret => {
                if (frame_no / CARET_PERIOD) % 2 == 1 {
                    fill(dst, w, h, caret, [32, 32, 32, 0xFF], rects);
                }
            }
            Scenario::Still | Scenario::Scroll | Scenario::Settle => {}
        }
    }
}

/// How many scroll steps have been taken by frame `n` in [`Scenario::Settle`].
///
/// Advances through the first [`SETTLE_SCROLL`] frames of each cycle and then
/// stands still for the rest of it. A free function rather than a method because
/// both the dirty-rect decision and the render need it, and they must agree.
fn gcd(a: u32, b: u32) -> u32 {
    if a == 0 {
        return b.max(1);
    }
    gcd(b % a, a)
}

fn settle_offset_steps(n: u64) -> u64 {
    let cycles = n / SETTLE_CYCLE;
    let within = n % SETTLE_CYCLE;
    cycles * SETTLE_SCROLL + within.min(SETTLE_SCROLL)
}

/// Copy one horizontal run of pixels from source row `sy` to destination row
/// `dy`. A no-op when the run is empty, so callers can split a row at the window
/// edges without checking each piece.
fn span(src: &[u8], dst: &mut [u8], row: usize, sy: usize, dy: usize, x0: usize, x1: usize) {
    if x0 >= x1 {
        return;
    }
    let s = sy * row + x0 * 4;
    let d = dy * row + x0 * 4;
    let len = (x1 - x0) * 4;
    dst[d..d + len].copy_from_slice(&src[s..s + len]);
}

/// A rectangle of the given fractions of the screen, centred horizontally and
/// sitting a little above centre, where a document window usually is.
fn centred(w: i32, h: i32, fw: f64, fh: f64) -> Rect {
    let rw = ((w as f64 * fw) as i32).max(2) & !1;
    let rh = ((h as f64 * fh) as i32).max(2) & !1;
    let left = ((w - rw) / 2).max(0);
    let top = ((h - rh) * 2 / 5).max(0);
    Rect { left, top, right: left + rw, bottom: top + rh }
}

/// Triangle wave: 0, 1, … span, span-1, … 0. Keeps a moving object on screen
/// without ever teleporting it.
fn bounce(value: i32, span: i32) -> i32 {
    let period = span * 2;
    let phase = value.rem_euclid(period);
    if phase <= span { phase } else { period - phase }
}

/// Copy a block of the image to another position, clipped to the regions being
/// redrawn so an overlay never writes outside the rectangles the report claims.
fn blit(src: &[u8], dst: &mut [u8], w: usize, h: usize, from: Rect, to: Rect, within: &[Rect]) {
    let row = w * 4;
    let height = (to.bottom - to.top).min(from.bottom - from.top).max(0);
    let width = (to.right - to.left).min(from.right - from.left).max(0);
    for dy in 0..height {
        let (sy, ty) = (from.top + dy, to.top + dy);
        if sy < 0 || sy >= h as i32 || ty < 0 || ty >= h as i32 {
            continue;
        }
        for dx in 0..width {
            let (sx, tx) = (from.left + dx, to.left + dx);
            if sx < 0 || sx >= w as i32 || tx < 0 || tx >= w as i32 {
                continue;
            }
            if !within.iter().any(|r| contains(r, tx, ty)) {
                continue;
            }
            let s = sy as usize * row + sx as usize * 4;
            let t = ty as usize * row + tx as usize * 4;
            dst[t..t + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
}

fn fill(dst: &mut [u8], w: usize, h: usize, r: Rect, colour: [u8; 4], within: &[Rect]) {
    let row = w * 4;
    for y in r.top.max(0)..r.bottom.min(h as i32) {
        for x in r.left.max(0)..r.right.min(w as i32) {
            if !within.iter().any(|rr| contains(rr, x, y)) {
                continue;
            }
            let i = y as usize * row + x as usize * 4;
            dst[i..i + 4].copy_from_slice(&colour);
        }
    }
}

fn contains(r: &Rect, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

impl FrameSource for ImageSource {
    fn describe(&self) -> String {
        // The fingerprint identifies the screenshot file and nothing else — it
        // is FNV over the still image, so it is the same whatever the scenario
        // does with it. The step and the period have to be stated separately or
        // two runs over different content look identical on the page.
        let mut s = format!(
            "снимок «{}» {}×{}, сценарий {}, отпечаток {}",
            self.name,
            self.shot.width,
            self.shot.height,
            self.scenario.name(),
            self.fingerprint.short()
        );
        if let Some(period) = self.period() {
            s.push_str(&format!(
                ", шаг {} пикс/кадр, период {period} кадров",
                self.step
            ));
        }
        s
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.shot.width, self.shot.height)
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
        readback: Readback,
    ) -> Result<Option<Frame<'_>>, CaptureError> {
        let rects = self.dirty_for(self.frame_no);
        if rects.is_empty() {
            // A still screen has to cost the caller the same wait a real timeout
            // would, or the runner spins and the processor figure becomes a
            // measurement of this loop rather than of the work.
            std::thread::sleep(self.interval.map_or(Duration::from_micros(200), |_| timeout));
            self.frame_no += 1;
            return Ok(None);
        }

        let mut waited = Duration::ZERO;
        if let Some(interval) = self.interval {
            let now = Instant::now();
            let due = self.next_due.unwrap_or(now);
            if let Some(wait) = due.checked_duration_since(now) {
                if wait > timeout {
                    std::thread::sleep(timeout);
                    return Ok(None);
                }
                std::thread::sleep(wait);
                waited = wait;
            }
            self.next_due = Some(due + interval);
        }

        // Rendering is this source's stand-in for a GPU readback: it is the
        // stage that moves a frame's worth of bytes through memory. Named as
        // such in the report so nobody reads it as a measurement of a GPU.
        let whole = self.whole();
        let painted: Vec<Rect> = match readback {
            Readback::Off => Vec::new(),
            Readback::Full => vec![whole],
            Readback::Dirty | Readback::Compare | Readback::Buffered => rects.clone(),
        };
        let started = Instant::now();
        self.render(&painted);
        let readback_us = started.elapsed().as_micros() as u64;
        let copied_px = painted.iter().map(Rect::area).sum();

        let compare_us = (readback == Readback::Compare).then(|| {
            let started = Instant::now();
            self.render(&[whole]);
            started.elapsed().as_micros() as u64
        });

        self.frame_no += 1;
        Ok(Some(Frame {
            width: self.shot.width,
            height: self.shot.height,
            stride: self.shot.width as usize * 4,
            bgra: readback.wants_pixels().then_some(self.buf.as_slice()),
            dirty: Dirty::Rects(rects),
            wait_us: waited.as_micros() as u64,
            work_us: 0,
            readback_us,
            copied_px,
            compare_us,
        }))
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn stand_in(&self) -> Option<String> {
        Some(
            concat!(
                "Копирование — memcpy из памяти, а не чтение через шину GPU.\n",
                "Заданное движение дешевле живой прокрутки: на том же содержимом\n",
                "живой экран дал p95 кодирования на треть больше."
            )
            .to_owned(),
        )
    }

    fn caveats(&self) -> Vec<String> {
        vec![concat!(
            "Это снимок, а не живой экран: движение задано скриптом.\n",
            "Числа отсюда сравнимы между машинами и между прогонами, но вердикт\n",
            "«проходит или нет» выносится по живому столу: настоящая прокрутка\n",
            "дороже этой — ползунок едет, заголовки стоят, текст пересглаживается."
        )
        .to_owned()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a screenshot: flat panels with a band of alternating
    /// columns where the "text" is, so a translation is visible in the bytes.
    fn shot(w: u32, h: u32) -> Shot {
        let mut bgra = vec![0u8; w as usize * h as usize * 4];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                let v = if (y / 3) % 2 == 0 { 240 } else { (x * 7 + y * 13) as u8 };
                bgra[i] = v;
                bgra[i + 1] = v;
                bgra[i + 2] = v;
                bgra[i + 3] = 0xFF;
            }
        }
        Shot { width: w, height: h, bgra }
    }

    fn source(scenario: Scenario) -> ImageSource {
        ImageSource::from_shot(shot(320, 240), "тест".to_owned(), scenario, 6, None)
    }

    /// Drive a source and collect what it handed out, so two runs can be
    /// compared byte for byte.
    fn frames(scenario: Scenario, count: usize) -> Vec<Option<(Vec<Rect>, Vec<u8>)>> {
        let mut src = source(scenario);
        let mut out = Vec::new();
        for _ in 0..count {
            let got = src
                .next_frame(Duration::from_millis(1), Readback::Dirty)
                .expect("источник из памяти не падает");
            out.push(got.map(|f| {
                let rects = match &f.dirty {
                    Dirty::Rects(r) => r.clone(),
                    Dirty::Unknown => Vec::new(),
                };
                (rects, f.bgra.expect("пиксели запрошены").to_vec())
            }));
        }
        out
    }

    /// The whole reason this source exists. If two runs ever diverge, a number
    /// from the VM cannot be divided by a number from the Celeron, and the trip
    /// answers nothing.
    #[test]
    fn two_runs_produce_identical_frames() {
        for scenario in [
            Scenario::Still,
            Scenario::Caret,
            Scenario::Edit,
            Scenario::Scroll,
            Scenario::Drag,
        ] {
            let a = frames(scenario, 40);
            let b = frames(scenario, 40);
            assert_eq!(a.len(), b.len());
            for (i, (x, y)) in a.iter().zip(&b).enumerate() {
                assert_eq!(x.is_some(), y.is_some(), "{scenario:?}, кадр {i}");
                if let (Some((ra, pa)), Some((rb, pb))) = (x, y) {
                    assert_eq!(ra, rb, "{scenario:?}, кадр {i}: разные прямоугольники");
                    assert!(pa == pb, "{scenario:?}, кадр {i}: разные пиксели");
                }
            }
        }
    }

    #[test]
    fn a_still_screen_reports_nothing_after_the_first_frame() {
        let got = frames(Scenario::Still, 10);
        assert!(got[0].is_some(), "первый кадр — это сама картинка");
        assert!(got[1..].iter().all(Option::is_none), "дальше меняться нечему");
    }

    #[test]
    fn a_caret_blinks_and_is_quiet_in_between() {
        let got = frames(Scenario::Caret, 46);
        let changed: Vec<usize> =
            got.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| i).collect();
        // Frame 0 is the picture itself; then one frame every blink.
        assert_eq!(changed, vec![0, 15, 30, 45]);
        let (rects, _) = got[15].as_ref().unwrap();
        assert_eq!(rects.len(), 1);
        assert!(rects[0].area() < 100, "мигающий курсор — это десятки пикселей");
    }

    #[test]
    fn scrolling_changes_about_a_third_of_the_screen() {
        let got = frames(Scenario::Scroll, 5);
        let (rects, _) = got[3].as_ref().unwrap();
        assert_eq!(rects.len(), 1);
        let share = rects[0].area() as f64 / (320.0 * 240.0);
        // The live VM runs reported 32–38% changing per frame while a document
        // was scrolled. A stand-in that moved the whole screen would be a
        // different workload wearing the same name.
        assert!((0.30..0.42).contains(&share), "доля {share}");
    }

    #[test]
    fn scrolling_actually_moves_the_picture() {
        let got = frames(Scenario::Scroll, 4);
        let (_, a) = got[1].as_ref().unwrap();
        let (_, b) = got[2].as_ref().unwrap();
        assert!(a != b, "между кадрами прокрутки картинка обязана измениться");
    }

    /// A full-frame readback has to produce the same picture as a dirty-region
    /// one. It very nearly did not: testing the whole row span against the
    /// window edges left the window unscrolled whenever the caller asked for the
    /// entire width, which is exactly what `--readback full` does.
    #[test]
    fn a_full_redraw_scrolls_the_window_too() {
        let mut dirty = source(Scenario::Scroll);
        let mut full = source(Scenario::Scroll);
        for _ in 0..4 {
            let a = dirty
                .next_frame(Duration::from_millis(1), Readback::Dirty)
                .unwrap()
                .map(|f| f.bgra.unwrap().to_vec());
            let b = full
                .next_frame(Duration::from_millis(1), Readback::Full)
                .unwrap()
                .map(|f| f.bgra.unwrap().to_vec());
            assert_eq!(a.is_some(), b.is_some());
            if let (Some(a), Some(b)) = (a, b) {
                assert!(a == b, "полная перерисовка разошлась с частичной");
            }
        }
    }

    #[test]
    fn dragging_reports_where_it_was_and_where_it_is() {
        let got = frames(Scenario::Drag, 6);
        let (rects, _) = got[4].as_ref().unwrap();
        assert_eq!(rects.len(), 2, "уехало и приехало — два прямоугольника");
        assert_ne!(rects[0], rects[1], "за кадр окно обязано сдвинуться");
    }

    /// The dialog must never jump: a teleport across the desktop is not a drag,
    /// and the encoder would price it as one enormous change.
    #[test]
    fn the_dragged_dialog_never_teleports() {
        let src = source(Scenario::Drag);
        let mut prev = src.dialog_at(0);
        for n in 1..500u64 {
            let now = src.dialog_at(n);
            let jump = (now.left - prev.left).abs().max((now.top - prev.top).abs());
            assert!(jump <= 8, "кадр {n}: скачок на {jump} пикселей");
            prev = now;
        }
    }

    /// The whole point of `settle` is that the picture stops. If the pixels
    /// keep changing through the hold, there is nothing to settle to and the
    /// measurement is of scrolling under another name.
    #[test]
    fn settle_moves_then_actually_stops() {
        let mut src = source(Scenario::Settle);
        let mut prev: Option<Vec<u8>> = None;
        let mut moved_in_scroll = 0;
        let mut moved_in_hold = 0;

        for n in 0..(SETTLE_CYCLE * 2) {
            let frame = src
                .next_frame(Duration::from_millis(1), Readback::Full)
                .unwrap()
                .and_then(|f| f.bgra.map(<[u8]>::to_vec));
            let Some(frame) = frame else { continue };
            if let Some(before) = &prev {
                if *before != frame {
                    // Frame zero is the whole image arriving, not motion.
                    if n % SETTLE_CYCLE >= 1 && n % SETTLE_CYCLE <= SETTLE_SCROLL {
                        moved_in_scroll += 1;
                    } else {
                        moved_in_hold += 1;
                    }
                }
            }
            prev = Some(frame);
        }

        assert!(moved_in_scroll > 0, "во время прокрутки картинка обязана меняться");
        assert_eq!(moved_in_hold, 0, "после остановки картинка обязана замереть");
    }

    /// A frame announced as dirty that rendered identically would be charged to
    /// the encoder as work it never had to do, and would push the settle curve
    /// one frame late.
    #[test]
    fn settle_calls_a_frame_dirty_exactly_when_it_changed() {
        let src = source(Scenario::Settle);
        for n in 1..(SETTLE_CYCLE * 3) {
            let announced = !src.dirty_for(n).is_empty();
            let moved = settle_offset_steps(n) != settle_offset_steps(n - 1);
            assert_eq!(announced, moved, "кадр {n}");
        }
    }

    /// The trap this scenario fell into on the day it was written, stated as a
    /// test so it cannot be fallen into again.
    ///
    /// A quiet frame is reported as `None`, and a consumer that reads that as
    /// "nothing yet, ask again" gets back exactly the moving frames — which are
    /// `scroll`, frame for frame and byte for byte. The export path did this,
    /// and `settle` produced a file with the same fingerprint as `scroll` while
    /// every settling number computed from it looked plausible.
    ///
    /// Anything building a timeline out of this source has to count the quiet
    /// frames rather than wait through them.
    #[test]
    fn dropping_the_quiet_frames_turns_settle_back_into_scroll() {
        let moving: Vec<Vec<u8>> = frames(Scenario::Settle, (SETTLE_CYCLE * 2) as usize)
            .into_iter()
            .flatten()
            .map(|(_, px)| px)
            .collect();
        assert!(!moving.is_empty(), "хоть что-то должно было двигаться");

        let scrolled: Vec<Vec<u8>> = frames(Scenario::Scroll, moving.len())
            .into_iter()
            .flatten()
            .map(|(_, px)| px)
            .collect();

        assert_eq!(
            moving.len(),
            scrolled.len(),
            "движущихся кадров у settle столько же, сколько всех у scroll"
        );
        assert_eq!(
            moving, scrolled,
            "выброси тихие кадры — и settle это ровно scroll, до байта"
        );
    }

    /// The number that explained the settle measurement's warm cache, and which
    /// nothing printed. A step sharing a factor with the window height visits
    /// only a fraction of the possible pictures.
    #[test]
    fn the_period_says_how_few_distinct_pictures_a_scroll_has() {
        // 320×240 stand-in: the window is 60% of the height, so 144 rows.
        let sixty = ImageSource::from_shot(shot(320, 240), "т".to_owned(), Scenario::Scroll, 60, None);
        assert_eq!(sixty.period(), Some(144 / 12), "gcd(60,144)=12");

        // Coprime to the window height: every offset appears.
        let coprime = ImageSource::from_shot(shot(320, 240), "т".to_owned(), Scenario::Scroll, 61, None);
        assert_eq!(coprime.period(), Some(144));

        // A still screen has no period at all.
        let still = ImageSource::from_shot(shot(320, 240), "т".to_owned(), Scenario::Still, 60, None);
        assert_eq!(still.period(), None);
    }

    /// Two runs differing only in `--step` used to print character-identical
    /// descriptions while measuring different content.
    #[test]
    fn the_description_distinguishes_two_steps() {
        let a = ImageSource::from_shot(shot(320, 240), "т".to_owned(), Scenario::Scroll, 40, None);
        let b = ImageSource::from_shot(shot(320, 240), "т".to_owned(), Scenario::Scroll, 80, None);
        assert_ne!(a.describe(), b.describe(), "{}", a.describe());
    }

    #[test]
    fn scenarios_parse_and_unknown_ones_do_not() {
        assert_eq!(Scenario::parse("scroll"), Some(Scenario::Scroll));
        assert_eq!(Scenario::parse("drag"), Some(Scenario::Drag));
        assert_eq!(Scenario::parse("settle"), Some(Scenario::Settle));
        assert_eq!(Scenario::parse("прокрутка"), None);
        assert_eq!(Scenario::parse("full"), None);
    }
}
