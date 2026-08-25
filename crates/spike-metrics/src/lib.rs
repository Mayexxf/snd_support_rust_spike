//! Measurement core for the phase-0 harness.
//!
//! Everything here is platform-independent on purpose: the numbers this crate
//! produces are the entire deliverable of phase 0, so they are the part that has
//! to be under test on a machine we can actually run tests on. The Win32 calls
//! live in `spike-capture` behind `cfg(windows)`.
//!
//! **Reporting posture.** A measurement that quietly reports a mean is worse than
//! no measurement. A remote session is judged on its worst moments, not its
//! average one, so every latency is reported as p50/p95/p99/max and never as a
//! bare average.

pub mod cpu;
pub mod env;

use std::fmt::Write as _;
use std::time::Duration;

/// Latency samples in microseconds, kept raw so percentiles stay exact.
///
/// Raw samples rather than a bucketed histogram: a 60-second run at 30 fps is
/// 1800 samples, and sorting that is free. Buckets would only start paying off
/// three orders of magnitude later.
#[derive(Debug, Default, Clone)]
pub struct Latencies {
    samples: Vec<u64>,
}

impl Latencies {
    pub fn push(&mut self, micros: u64) {
        self.samples.push(micros);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Nearest-rank percentile. `q` is 0.0..=1.0.
    ///
    /// Nearest-rank rather than interpolated: the result is always a value that
    /// actually occurred, which is what you want when the next question is
    /// "which frame was that?".
    pub fn percentile(&self, q: f64) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        let rank = (q * sorted.len() as f64).ceil() as usize;
        let idx = rank.saturating_sub(1).min(sorted.len() - 1);
        Some(sorted[idx])
    }

    pub fn max(&self) -> Option<u64> {
        self.samples.iter().copied().max()
    }

    pub fn mean(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: u128 = self.samples.iter().map(|&v| v as u128).sum();
        Some((sum / self.samples.len() as u128) as u64)
    }

    /// `p50 / p95 / p99 / max`, in milliseconds with one decimal.
    fn summary_ms(&self) -> String {
        match (
            self.percentile(0.50),
            self.percentile(0.95),
            self.percentile(0.99),
            self.max(),
        ) {
            (Some(p50), Some(p95), Some(p99), Some(max)) => format!(
                "{:>6.1} {:>6.1} {:>6.1} {:>6.1}",
                p50 as f64 / 1000.0,
                p95 as f64 / 1000.0,
                p99 as f64 / 1000.0,
                max as f64 / 1000.0
            ),
            _ => "     —      —      —      —".to_owned(),
        }
    }
}

/// One pass through the pipeline.
#[derive(Debug, Default, Clone)]
pub struct FrameStat {
    /// Time blocked waiting for the screen to change. **Not a cost.**
    ///
    /// A source capped at 30 fps spends most of its wall clock here by design.
    /// Folded into the working time it would make an idle desktop look like an
    /// overloaded one, which is the opposite of the truth.
    pub wait_us: u64,
    /// Time doing capture work once a frame was available. A cost.
    pub work_us: u64,
    /// Time copying pixels from GPU memory into system memory. A cost.
    ///
    /// Separate from `work_us` on purpose. On a Braswell iGPU the readback of a
    /// 1080p BGRA frame is about 8 MB over the bus thirty times a second, and if
    /// that is the bottleneck then no choice of encoder fixes it. Rolling the two
    /// together would hide which one to attack.
    pub readback_us: u64,
    /// `false` when the capture API reported no new frame — the screen was still.
    pub is_new: bool,
    /// Pixels covered by the reported dirty rectangles, before overlap removal.
    pub changed_px: u64,
    /// Number of dirty rectangles the capture API reported.
    pub dirty_rects: u32,
    /// Encode time, when an encoder is wired in.
    pub encode_us: Option<u64>,
    /// Size of the encoded frame in bytes.
    pub encoded_bytes: Option<usize>,
    /// Whether the encoder emitted a keyframe.
    pub is_keyframe: bool,
}

/// Accumulates per-frame statistics over one run.
#[derive(Debug)]
pub struct Recorder {
    label: String,
    width: u32,
    height: u32,
    target_fps: u32,
    wait: Latencies,
    work: Latencies,
    readback: Latencies,
    encode: Latencies,
    budget: Latencies,
    frames_seen: u64,
    frames_new: u64,
    keyframes: u64,
    changed_px_total: u128,
    dirty_rects_total: u64,
    encoded_bytes_total: u128,
    keyframe_bytes_total: u128,
    delta_bytes_total: u128,
    delta_frames: u64,
    access_lost: u32,
    reinits: u32,
    cpu_start: cpu::CpuSnapshot,
}

impl Recorder {
    pub fn new(label: impl Into<String>, width: u32, height: u32, target_fps: u32) -> Self {
        Self {
            label: label.into(),
            width,
            height,
            target_fps,
            wait: Latencies::default(),
            work: Latencies::default(),
            readback: Latencies::default(),
            encode: Latencies::default(),
            budget: Latencies::default(),
            frames_seen: 0,
            frames_new: 0,
            keyframes: 0,
            changed_px_total: 0,
            dirty_rects_total: 0,
            encoded_bytes_total: 0,
            keyframe_bytes_total: 0,
            delta_bytes_total: 0,
            delta_frames: 0,
            access_lost: 0,
            reinits: 0,
            cpu_start: cpu::CpuSnapshot::now(),
        }
    }

    pub fn record(&mut self, stat: &FrameStat) {
        self.frames_seen += 1;
        self.wait.push(stat.wait_us);
        if !stat.is_new {
            return;
        }
        self.frames_new += 1;
        self.work.push(stat.work_us);
        if stat.readback_us > 0 {
            self.readback.push(stat.readback_us);
        }
        // Everything the machine actually had to do for this frame. This is the
        // figure that decides whether the target keeps up, so it is accumulated
        // per frame rather than reconstructed from three separate percentiles —
        // the p95 of a sum is not the sum of the p95s.
        self.budget.push(
            stat.work_us
                + stat.readback_us
                + stat.encode_us.unwrap_or(0),
        );
        self.changed_px_total += stat.changed_px as u128;
        self.dirty_rects_total += stat.dirty_rects as u64;
        if let Some(us) = stat.encode_us {
            self.encode.push(us);
        }
        if let Some(bytes) = stat.encoded_bytes {
            self.encoded_bytes_total += bytes as u128;
            if stat.is_keyframe {
                self.keyframes += 1;
                self.keyframe_bytes_total += bytes as u128;
            } else {
                self.delta_frames += 1;
                self.delta_bytes_total += bytes as u128;
            }
        }
    }

    /// A capture backend lost its access to the desktop and had to reinitialise.
    ///
    /// Counted rather than treated as fatal: on a real machine this fires every
    /// time the user locks the screen, a UAC prompt appears or the resolution
    /// changes, and how often it happens *is* one of the phase-0 questions.
    pub fn note_access_lost(&mut self) {
        self.access_lost += 1;
    }

    pub fn note_reinit(&mut self) {
        self.reinits += 1;
    }

    pub fn finish(self, elapsed: Duration) -> Report {
        let cpu = self.cpu_start.elapsed_since();
        Report {
            label: self.label,
            width: self.width,
            height: self.height,
            target_fps: self.target_fps,
            elapsed,
            wait: self.wait,
            work: self.work,
            readback: self.readback,
            encode: self.encode,
            budget: self.budget,
            frames_seen: self.frames_seen,
            frames_new: self.frames_new,
            keyframes: self.keyframes,
            changed_px_total: self.changed_px_total,
            dirty_rects_total: self.dirty_rects_total,
            encoded_bytes_total: self.encoded_bytes_total,
            keyframe_bytes_total: self.keyframe_bytes_total,
            delta_bytes_total: self.delta_bytes_total,
            delta_frames: self.delta_frames,
            access_lost: self.access_lost,
            reinits: self.reinits,
            cpu,
        }
    }
}

/// The finished measurement. `Display` renders the operator-facing report.
#[derive(Debug)]
pub struct Report {
    pub label: String,
    pub width: u32,
    pub height: u32,
    pub target_fps: u32,
    pub elapsed: Duration,
    pub wait: Latencies,
    pub work: Latencies,
    pub readback: Latencies,
    pub encode: Latencies,
    pub budget: Latencies,
    pub frames_seen: u64,
    pub frames_new: u64,
    pub keyframes: u64,
    pub changed_px_total: u128,
    pub dirty_rects_total: u64,
    pub encoded_bytes_total: u128,
    pub keyframe_bytes_total: u128,
    pub delta_bytes_total: u128,
    pub delta_frames: u64,
    pub access_lost: u32,
    pub reinits: u32,
    pub cpu: cpu::CpuUsage,
}

impl Report {
    /// Frames per second that actually carried new content.
    pub fn effective_fps(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        self.frames_new as f64 / secs
    }

    /// Share of polls that found the screen unchanged, 0.0..=1.0.
    ///
    /// This is the number the whole encoding budget rests on: a support session
    /// spends most of its time on a still desktop, and a still frame costs
    /// nothing to encode because it is never encoded.
    pub fn still_share(&self) -> f64 {
        if self.frames_seen == 0 {
            return 0.0;
        }
        (self.frames_seen - self.frames_new) as f64 / self.frames_seen as f64
    }

    /// Mean share of the screen that changed, per frame that carried content.
    pub fn mean_changed_share(&self) -> f64 {
        let px = u128::from(self.width) * u128::from(self.height);
        if self.frames_new == 0 || px == 0 {
            return 0.0;
        }
        (self.changed_px_total as f64 / self.frames_new as f64) / px as f64
    }

    /// Average bitrate over the whole run, in megabits per second.
    pub fn mbps(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.encoded_bytes_total as f64 * 8.0) / secs / 1_000_000.0
    }

    fn mean_bytes(total: u128, count: u64) -> Option<u128> {
        (count > 0).then(|| total / count as u128)
    }

    /// Microseconds available per frame at the target rate.
    pub fn interval_us(&self) -> u64 {
        if self.target_fps == 0 {
            return 0;
        }
        1_000_000 / u64::from(self.target_fps)
    }

    /// Share of the frame interval consumed by work at the given percentile.
    ///
    /// This is the whole point of the exercise. Frame rate alone hides the
    /// answer: a machine can hold 30 fps while leaving nothing for the user's
    /// own work, and a machine that misses 30 fps by a hair is a different
    /// conversation from one that misses it by four times.
    pub fn budget_share(&self, q: f64) -> Option<f64> {
        let interval = self.interval_us();
        if interval == 0 {
            return None;
        }
        self.budget.percentile(q).map(|us| us as f64 / interval as f64)
    }

    /// Plain-language verdict on whether the machine keeps up.
    ///
    /// Judged on p95, not the median: a session is judged on its worst moments.
    /// The thresholds are deliberately conservative — the client machine also has
    /// to run the wizard and whatever the user was actually doing.
    pub fn verdict(&self) -> Option<Verdict> {
        if self.budget.len() < MIN_FRAMES_FOR_VERDICT {
            return None;
        }
        let share = self.budget_share(0.95)?;
        Some(if share < 0.5 {
            Verdict::Comfortable(share)
        } else if share < 0.8 {
            Verdict::Tight(share)
        } else if share < 1.0 {
            Verdict::Marginal(share)
        } else {
            Verdict::Fails(share)
        })
    }
}

/// Frames of actual content below which no verdict is offered.
///
/// Nearest-rank p95 over `n` samples lands on index `ceil(0.95n)`, which for
/// `n < 21` is simply the worst sample. A run of five content frames therefore
/// reports p95 = p99 = max and would let the harness pronounce on a machine from
/// a single bad frame. Thirty leaves the percentile something to choose from.
///
/// The practical consequence: an idle desktop measures how *often* the screen
/// changes, and a scrolling one measures what a frame *costs*. One run cannot do
/// both.
pub const MIN_FRAMES_FOR_VERDICT: usize = 30;

/// How the machine coped, at p95 of the per-frame working time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    Comfortable(f64),
    Tight(f64),
    Marginal(f64),
    Fails(f64),
}

impl Verdict {
    pub fn share(&self) -> f64 {
        match *self {
            Verdict::Comfortable(s) | Verdict::Tight(s) | Verdict::Marginal(s) | Verdict::Fails(s) => s,
        }
    }

    pub fn explain(&self) -> &'static str {
        match self {
            Verdict::Comfortable(_) => {
                "запас есть. Машина успевает и оставляет процессор пользователю"
            }
            Verdict::Tight(_) => {
                "впритык. Успевает, но на загруженной машине начнёт ронять кадры. Снижайте разрешение или частоту"
            }
            Verdict::Marginal(_) => {
                "почти не успевает. При любой посторонней нагрузке кадры поедут. Нужен другой режим: меньше разрешение, меньше частота, дешевле кодек"
            }
            Verdict::Fails(_) => {
                "НЕ УСПЕВАЕТ. Целевая частота недостижима в этой конфигурации"
            }
        }
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();

        writeln!(s, "\n=== {} ===", self.label)?;
        writeln!(
            s,
            "Разрешение {}×{}, цель {} к/с, длительность {:.1} с",
            self.width,
            self.height,
            self.target_fps,
            self.elapsed.as_secs_f64()
        )?;

        writeln!(s, "\n-- Кадры --")?;
        writeln!(s, "  опрошено                {}", self.frames_seen)?;
        writeln!(
            s,
            "  с новым содержимым      {} ({:.1} к/с фактически)",
            self.frames_new,
            self.effective_fps()
        )?;
        writeln!(
            s,
            "  экран не менялся        {:.1}% опросов",
            self.still_share() * 100.0
        )?;
        if self.frames_new > 0 {
            writeln!(
                s,
                "  менялось за кадр        {:.2}% площади, прямоугольников в среднем {:.1}",
                self.mean_changed_share() * 100.0,
                self.dirty_rects_total as f64 / self.frames_new as f64
            )?;
        }
        if self.access_lost > 0 || self.reinits > 0 {
            writeln!(
                s,
                "  потеря доступа к экрану {} (переинициализаций {})",
                self.access_lost, self.reinits
            )?;
        }

        writeln!(s, "\n-- Стоимость кадра, мс --")?;
        writeln!(s, "                            p50    p95    p99    max")?;
        writeln!(s, "  работа захвата       {}", self.work.summary_ms())?;
        if !self.readback.is_empty() {
            writeln!(s, "  копирование в память {}", self.readback.summary_ms())?;
        }
        if !self.encode.is_empty() {
            writeln!(s, "  кодирование          {}", self.encode.summary_ms())?;
        }
        writeln!(s, "  ИТОГО на кадр        {}", self.budget.summary_ms())?;
        writeln!(s, "\n  ожидание кадра       {}", self.wait.summary_ms())?;
        writeln!(s, "  (ожидание — не расход: при цели {} к/с так и должно быть)", self.target_fps)?;

        if let Some(v) = self.verdict() {
            let interval_ms = self.interval_us() as f64 / 1000.0;
            writeln!(s, "\n-- Бюджет кадра --")?;
            writeln!(s, "  интервал при {} к/с      {:.1} мс", self.target_fps, interval_ms)?;
            if let (Some(p50), Some(p95)) = (self.budget_share(0.50), self.budget_share(0.95)) {
                writeln!(s, "  занято p50 / p95        {:.0}% / {:.0}%", p50 * 100.0, p95 * 100.0)?;
            }
            let mark = match v {
                Verdict::Comfortable(_) => "✓",
                Verdict::Tight(_) => "·",
                Verdict::Marginal(_) => "⚠",
                Verdict::Fails(_) => "✗",
            };
            writeln!(s, "  {mark} {}", v.explain())?;
        } else if !self.budget.is_empty() {
            writeln!(s, "\n-- Бюджет кадра --")?;
            writeln!(
                s,
                "  Вердикта не будет: кадров с содержимым {}, нужно хотя бы {}.",
                self.budget.len(),
                MIN_FRAMES_FOR_VERDICT
            )?;
            writeln!(
                s,
                "  При таком числе p95 — это просто худший кадр из {}, и цифры выше",
                self.budget.len()
            )?;
            writeln!(s, "  описывают один неудачный кадр, а не поведение машины.")?;
            writeln!(s, "  Стоимость кадра меряется прогоном С ДВИЖЕНИЕМ на экране.")?;
        }

        if self.encoded_bytes_total > 0 {
            writeln!(s, "\n-- Поток --")?;
            writeln!(s, "  средний битрейт         {:.2} Мбит/с", self.mbps())?;
            if let Some(b) = Report::mean_bytes(self.keyframe_bytes_total, self.keyframes) {
                writeln!(s, "  ключевой кадр           {} КБ ({} шт.)", b / 1024, self.keyframes)?;
            }
            if let Some(b) = Report::mean_bytes(self.delta_bytes_total, self.delta_frames) {
                writeln!(s, "  разностный кадр         {} Б ({} шт.)", b, self.delta_frames)?;
            }
        }

        writeln!(s, "\n-- Процессор --")?;
        write!(s, "{}", self.cpu.render(self.elapsed))?;

        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latencies(values: &[u64]) -> Latencies {
        let mut l = Latencies::default();
        for &v in values {
            l.push(v);
        }
        l
    }

    #[test]
    fn empty_latencies_have_no_percentiles() {
        let l = Latencies::default();
        assert_eq!(l.percentile(0.5), None);
        assert_eq!(l.max(), None);
        assert_eq!(l.mean(), None);
        // The report must survive a run that captured nothing at all — that is
        // exactly what a failed DDA init on a VM produces.
        assert!(l.summary_ms().contains('—'));
    }

    #[test]
    fn nearest_rank_returns_values_that_occurred() {
        let l = latencies(&[10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
        assert_eq!(l.percentile(0.50), Some(50));
        assert_eq!(l.percentile(0.95), Some(100));
        assert_eq!(l.percentile(0.99), Some(100));
        assert_eq!(l.max(), Some(100));
        assert_eq!(l.mean(), Some(55));
    }

    #[test]
    fn percentile_edges_stay_in_range() {
        let l = latencies(&[7]);
        assert_eq!(l.percentile(0.0), Some(7));
        assert_eq!(l.percentile(1.0), Some(7));
        // A q above 1.0 must clamp rather than panic: it can only arrive from a
        // typo in a caller, and panicking would throw away a finished run.
        assert_eq!(l.percentile(2.0), Some(7));
    }

    #[test]
    fn still_frames_are_counted_but_not_averaged_into_content() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        r.record(&FrameStat { wait_us: 33_000, is_new: false, ..Default::default() });
        r.record(&FrameStat { wait_us: 33_000, is_new: false, ..Default::default() });
        r.record(&FrameStat {
            wait_us: 1_000,
            work_us: 2_000,
            readback_us: 500,
            is_new: true,
            changed_px: 5_000,
            dirty_rects: 3,
            ..Default::default()
        });
        let rep = r.finish(Duration::from_secs(1));

        assert_eq!(rep.frames_seen, 3);
        assert_eq!(rep.frames_new, 1);
        // Still polls must not dilute the "how much changed" figure, or a quiet
        // desktop would report a comfortingly small number for the wrong reason.
        assert!((rep.mean_changed_share() - 0.5).abs() < 1e-9);
        assert!((rep.still_share() - 2.0 / 3.0).abs() < 1e-9);
        // Waiting is recorded for every poll, working only for frames that
        // carried content — a still poll costs nothing but wall clock.
        assert_eq!(rep.wait.len(), 3);
        assert_eq!(rep.work.len(), 1);
        assert_eq!(rep.readback.len(), 1);
        // Budget is work + readback + encode, never the wait.
        assert_eq!(rep.budget.percentile(0.5), Some(2_500));
    }

    #[test]
    fn budget_is_summed_per_frame_not_across_percentiles() {
        // Two frames whose costs are staggered: frame A is slow to read back,
        // frame B is slow to encode. Summing p95s would report 20 ms; the truth
        // is that no single frame ever cost more than 12 ms.
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        r.record(&FrameStat {
            is_new: true,
            work_us: 2_000,
            readback_us: 10_000,
            encode_us: Some(0),
            ..Default::default()
        });
        r.record(&FrameStat {
            is_new: true,
            work_us: 2_000,
            readback_us: 0,
            encode_us: Some(10_000),
            ..Default::default()
        });
        let rep = r.finish(Duration::from_secs(1));
        assert_eq!(rep.budget.max(), Some(12_000));
    }

    #[test]
    fn verdict_reads_the_p95_against_the_frame_interval() {
        // 30 fps leaves 33.3 ms per frame.
        let cases = [
            (10_000u64, "запас"),   // 30%
            (20_000, "впритык"),    // 60%
            (30_000, "почти"),      // 90%
            (50_000, "НЕ УСПЕВАЕТ"), // 150%
        ];
        for (cost_us, expected) in cases {
            let mut r = Recorder::new("тест", 1920, 1080, 30);
            for _ in 0..MIN_FRAMES_FOR_VERDICT {
                r.record(&FrameStat { is_new: true, work_us: cost_us, ..Default::default() });
            }
            let rep = r.finish(Duration::from_secs(1));
            let v = rep.verdict().expect("вердикт");
            assert!(
                v.explain().contains(expected),
                "при {cost_us} мкс ожидали «{expected}», получили «{}»",
                v.explain()
            );
        }
    }

    #[test]
    fn a_handful_of_frames_earns_no_verdict() {
        // The first real Windows run captured five content frames on an idle
        // desktop and reported p95 = p99 = max, because nearest-rank p95 over
        // five samples *is* the worst sample. A verdict from that describes one
        // unlucky frame, not a machine.
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        for _ in 0..(MIN_FRAMES_FOR_VERDICT - 1) {
            r.record(&FrameStat { is_new: true, work_us: 1_000, ..Default::default() });
        }
        let rep = r.finish(Duration::from_secs(30));
        assert!(rep.verdict().is_none());
        // The section must still explain itself rather than silently vanish.
        let text = rep.to_string();
        assert!(text.contains("Вердикта не будет"), "{text}");
        assert!(text.contains("С ДВИЖЕНИЕМ"), "{text}");
    }

    #[test]
    fn verdict_strings_carry_no_runaway_whitespace() {
        // These literals were once written through a script that ate their line
        // continuations, leaving eighteen spaces mid-sentence in the report.
        for v in [
            Verdict::Comfortable(0.1),
            Verdict::Tight(0.6),
            Verdict::Marginal(0.9),
            Verdict::Fails(1.5),
        ] {
            let text = v.explain();
            assert!(!text.contains("  "), "двойной пробел в «{text}»");
            assert!(!text.contains('\n'), "перенос строки в «{text}»");
        }
    }

    #[test]
    fn a_run_with_no_content_has_no_verdict() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        r.record(&FrameStat { wait_us: 33_000, is_new: false, ..Default::default() });
        let rep = r.finish(Duration::from_secs(1));
        // Nothing was measured, so nothing is claimed. A verdict here would be
        // an invented answer to the question phase 0 exists to ask.
        assert!(rep.verdict().is_none());
        let _ = rep.to_string();
    }

    #[test]
    fn keyframes_and_deltas_are_averaged_apart() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        r.record(&FrameStat {
            is_new: true,
            encode_us: Some(30_000),
            encoded_bytes: Some(100_000),
            is_keyframe: true,
            ..Default::default()
        });
        for _ in 0..3 {
            r.record(&FrameStat {
                is_new: true,
                encode_us: Some(5_000),
                encoded_bytes: Some(2_000),
                is_keyframe: false,
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(2));

        assert_eq!(rep.keyframes, 1);
        assert_eq!(rep.delta_frames, 3);
        assert_eq!(Report::mean_bytes(rep.keyframe_bytes_total, rep.keyframes), Some(100_000));
        assert_eq!(Report::mean_bytes(rep.delta_bytes_total, rep.delta_frames), Some(2_000));
        // 106000 bytes over 2 s = 424 kbit/s.
        assert!((rep.mbps() - 0.424).abs() < 1e-6);
    }

    #[test]
    fn zero_duration_does_not_divide_by_zero() {
        let r = Recorder::new("тест", 1920, 1080, 30);
        let rep = r.finish(Duration::ZERO);
        assert_eq!(rep.effective_fps(), 0.0);
        assert_eq!(rep.mbps(), 0.0);
        // Rendering must work on an empty run: the operator needs to see *that*
        // nothing was captured, not an empty terminal.
        let _ = rep.to_string();
    }
}
