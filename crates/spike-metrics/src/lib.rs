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

    /// `p50 / p95 / max` as whole numbers, for samples that are not durations.
    ///
    /// The quantizer is the only one so far, and printing it through
    /// [`Self::summary_ms`] would divide it by a thousand and call it
    /// milliseconds.
    fn summary_plain(&self) -> String {
        match (self.percentile(0.50), self.percentile(0.95), self.max()) {
            (Some(p50), Some(p95), Some(max)) => {
                format!("{p50:>6} {p95:>6} {max:>6}")
            }
            _ => "     —      —      —".to_owned(),
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
    /// Time converting BGRA to the planar YUV the encoder wants. A cost.
    ///
    /// Its own stage rather than part of encoding, because it is separately
    /// optimisable — it walks only the changed regions, and a production version
    /// would use SIMD. Hidden inside the encoder figure, a slow conversion would
    /// look like a slow codec and send the work in the wrong direction.
    pub convert_us: u64,
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
    /// Pixels actually moved out of GPU memory this frame.
    ///
    /// Not the same as `changed_px`: the point of tracking both is to see how
    /// much of the reported change the copy path managed to skip.
    pub copied_px: u64,
    /// Encode time, when an encoder is wired in.
    pub encode_us: Option<u64>,
    /// Size of the encoded frame in bytes.
    pub encoded_bytes: Option<usize>,
    /// In a comparison run, what the rejected copy path cost on this same frame.
    pub compare_us: Option<u64>,
    /// Whether the encoder emitted a keyframe.
    pub is_keyframe: bool,
    /// The encoder took this frame and emitted nothing for it.
    ///
    /// Counted separately from a zero-byte frame, because the two are the
    /// difference between "the screen was quiet" and "we could not keep up" —
    /// and every other number in this report reads the same either way.
    pub encode_dropped: bool,
    /// The quantizer the encoder chose for this frame, 0..=63.
    pub quantizer: Option<u8>,
    /// The whole iteration, wait included — everything between one poll and the
    /// next.
    ///
    /// `ИТОГО` is the sum of the stage timers, so it cannot see the work between
    /// them. Subtracting the wait from this gives the same span measured from
    /// outside, and the difference is what the stages missed.
    pub iter_us: u64,
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
    convert: Latencies,
    compare: Latencies,
    encode: Latencies,
    budget: Latencies,
    outside: Latencies,
    frames_seen: u64,
    frames_new: u64,
    keyframes: u64,
    encode_drops: u64,
    pointer_only: u64,
    moved_px_total: u128,
    moved_rects_total: u64,
    quantizer: Latencies,
    changed_px_total: u128,
    copied_px_total: u128,
    dirty_rects_total: u64,
    encoded_bytes_total: u128,
    keyframe_bytes_total: u128,
    delta_bytes_total: u128,
    delta_frames: u64,
    access_lost: u32,
    reinits: u32,
    cpu_start: cpu::CpuSnapshot,
    tracks: Vec<Track>,
    comparing: bool,
    stand_in: Option<String>,
    unpaced: bool,
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
            convert: Latencies::default(),
            compare: Latencies::default(),
            encode: Latencies::default(),
            budget: Latencies::default(),
            outside: Latencies::default(),
            frames_seen: 0,
            frames_new: 0,
            keyframes: 0,
            encode_drops: 0,
            pointer_only: 0,
            moved_px_total: 0,
            moved_rects_total: 0,
            quantizer: Latencies::default(),
            changed_px_total: 0,
            copied_px_total: 0,
            dirty_rects_total: 0,
            encoded_bytes_total: 0,
            keyframe_bytes_total: 0,
            delta_bytes_total: 0,
            delta_frames: 0,
            access_lost: 0,
            reinits: 0,
            cpu_start: cpu::CpuSnapshot::now(),
            tracks: Vec::new(),
            comparing: false,
            stand_in: None,
            unpaced: false,
        }
    }

    /// Declare that this run was fed frames as fast as it could take them.
    ///
    /// `--frames` over a screenshot deliberately drops the pacing, so that a
    /// fast machine does not spend the difference waiting and read as slow. The
    /// wall clock is then not the timeline the stream would have occupied, and
    /// the bitrate has to be rebuilt from the target rate instead — see
    /// [`Report::mbps`].
    pub fn note_unpaced(&mut self) {
        self.unpaced = true;
    }

    /// How many polls found the desktop unchanged but the pointer moved.
    ///
    /// Taken from the source at the end of the run rather than per frame,
    /// because these polls never produce a `FrameStat` — they are exactly the
    /// ones the loop discards, which is how they came to be counted as a still
    /// screen in the first place.
    pub fn note_pointer_only(&mut self, polls: u64) {
        self.pointer_only = polls;
    }

    /// How much of the change the driver described as a blit rather than a
    /// repaint. Taken from the source at the end for the same reason as above.
    pub fn note_moved(&mut self, pixels: u128, rects: u64) {
        self.moved_px_total = pixels;
        self.moved_rects_total = rects;
    }

    /// Declare that this run's costs are not what the product would pay, and why.
    ///
    /// Set for any source that is not a live screen. The budget column then
    /// stops carrying a verdict: measured against a screenshot it read 69% and
    /// "nearly keeps up" for a configuration the live desktop had priced at 116%
    /// and "does not". Two reasons, both structural — copying is a memcpy rather
    /// than a read across the bus, and a scripted translation has none of the
    /// irregular moments a real scroll puts in the tail.
    ///
    /// A number that flatters is worse than no number, and a tick mark next to
    /// it is worse still.
    pub fn note_stand_in(&mut self, why: impl Into<String>) {
        self.stand_in = Some(why.into());
    }

    /// Add one encoder configuration to a comparison run.
    ///
    /// Adding any track switches the recorder into comparison mode: the shared
    /// per-frame budget stops being meaningful, because the loop is now running
    /// several encoders over every frame, and each track carries its own.
    pub fn add_track(&mut self, label: impl Into<String>, width: u32, height: u32) -> usize {
        self.comparing = true;
        self.tracks.push(Track::new(label, width, height));
        self.tracks.len() - 1
    }

    pub fn record_track(&mut self, index: usize, stat: &TrackStat) {
        if let Some(track) = self.tracks.get_mut(index) {
            track.record(stat);
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
        // Pushed unconditionally, including zeros.
        //
        // These two used to be filtered on `> 0`, which looks like "skip the
        // stage that did not run" and is in fact a censored sample: the timers
        // truncate to whole microseconds, so a cheap frame reads 0 and was
        // dropped while a dear one was kept. Every percentile on these two rows
        // came out biased upward, and the bias grew as the stage got cheaper —
        // which is precisely backwards, since the cheap case is the one a change
        // is trying to produce. `budget` counted the same frame as zero all
        // along (below), so the two rows were printed side by side in one table
        // having been computed over different sets of frames.
        //
        // A stage that genuinely did not run contributes a zero, which is the
        // truth about that frame.
        self.readback.push(stat.readback_us);
        // Conversion is the one stage a comparison run does not do once. Each
        // scale is converted separately and charged to the tracks that read it
        // (`TrackStat::convert_us`), so the figure arriving here is a zero that
        // means "not measured", not a zero that means "free" — and pushing it
        // printed a row of `0.0 0.0 0.0 0.0` under a stage that had in fact
        // cost 1.6 ms a frame, in every comparison report this harness has
        // produced. Readback above is not gated: that one really is paid once.
        if !self.comparing {
            self.convert.push(stat.convert_us);
        }
        if let Some(us) = stat.compare_us {
            self.compare.push(us);
        }
        // Everything the machine actually had to do for this frame. This is the
        // figure that decides whether the target keeps up, so it is accumulated
        // per frame rather than reconstructed from three separate percentiles —
        // the p95 of a sum is not the sum of the p95s.
        if !self.comparing {
            let staged = stat.work_us
                + stat.readback_us
                + stat.convert_us
                + stat.encode_us.unwrap_or(0);
            self.budget.push(staged);
            // The same span measured from outside, minus the wait, minus what
            // the stages accounted for. Saturating because the two clocks are
            // read at different points and rounding can put them a microsecond
            // the wrong way round; that is noise, not negative work.
            // The rejected copy path is not overhead, it is a second copy the
            // operator asked for. Left in, it landed in «вне замеров стадий» —
            // a row whose whole purpose is to say how much work the stage
            // timers failed to see — and there charged the shipping pipeline
            // for a path deliberately run alongside it. `budget` is untouched
            // on purpose: the product pays for one copy, not two.
            let measured = stat.iter_us.saturating_sub(stat.wait_us);
            let accounted = staged + stat.compare_us.unwrap_or(0);
            self.outside.push(measured.saturating_sub(accounted));
        }
        self.changed_px_total += stat.changed_px as u128;
        self.copied_px_total += stat.copied_px as u128;
        self.dirty_rects_total += stat.dirty_rects as u64;
        if let Some(us) = stat.encode_us {
            self.encode.push(us);
        }
        if let Some(q) = stat.quantizer {
            self.quantizer.push(q as u64);
        }
        if stat.encode_dropped {
            // Counted, and kept out of the delta average: a dropped frame is
            // zero bytes, and averaging it in would quietly pull the delta size
            // down while looking like better compression.
            self.encode_drops += 1;
        } else if let Some(bytes) = stat.encoded_bytes {
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
            convert: self.convert,
            compare: self.compare,
            encode: self.encode,
            budget: self.budget,
            outside: self.outside,
            frames_seen: self.frames_seen,
            frames_new: self.frames_new,
            keyframes: self.keyframes,
            encode_drops: self.encode_drops,
            pointer_only: self.pointer_only,
            moved_px_total: self.moved_px_total,
            moved_rects_total: self.moved_rects_total,
            quantizer: self.quantizer,
            changed_px_total: self.changed_px_total,
            copied_px_total: self.copied_px_total,
            dirty_rects_total: self.dirty_rects_total,
            encoded_bytes_total: self.encoded_bytes_total,
            keyframe_bytes_total: self.keyframe_bytes_total,
            delta_bytes_total: self.delta_bytes_total,
            delta_frames: self.delta_frames,
            access_lost: self.access_lost,
            reinits: self.reinits,
            cpu,
            tracks: self.tracks,
            stand_in: self.stand_in,
            unpaced: self.unpaced,
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
    pub convert: Latencies,
    pub compare: Latencies,
    pub encode: Latencies,
    pub budget: Latencies,
    /// Per frame, the part of the iteration the stage timers did not see.
    /// See [`FrameStat::iter_us`].
    pub outside: Latencies,
    pub frames_seen: u64,
    pub frames_new: u64,
    pub keyframes: u64,
    /// Frames the encoder took and emitted nothing for. See [`FrameStat::encode_dropped`].
    pub encode_drops: u64,
    /// Polls where only the mouse pointer moved. See
    /// [`spike_capture::FrameSource::pointer_only_polls`] — counted apart from
    /// still polls because a moving cursor is something the product must send.
    pub pointer_only: u64,
    /// Pixels blitted rather than repainted. See
    /// [`spike_capture::FrameSource::moved_pixels`].
    pub moved_px_total: u128,
    pub moved_rects_total: u64,
    /// Quantizer chosen per frame, 0..=63.
    pub quantizer: Latencies,
    pub changed_px_total: u128,
    pub copied_px_total: u128,
    pub dirty_rects_total: u64,
    pub encoded_bytes_total: u128,
    pub keyframe_bytes_total: u128,
    pub delta_bytes_total: u128,
    pub delta_frames: u64,
    pub access_lost: u32,
    pub reinits: u32,
    pub cpu: cpu::CpuUsage,
    /// Encoder configurations measured side by side on identical frames.
    /// Empty outside a comparison run.
    pub tracks: Vec<Track>,
    /// Why this run's per-frame budget is not what the product would pay.
    /// `None` for a live screen, which is the only source that earns a verdict.
    pub stand_in: Option<String>,
    /// True when the run was not held to the target frame rate — `--frames`
    /// over a screenshot, where frames are fed as fast as the machine manages.
    ///
    /// Only the bitrate cares, and it cares a lot: see [`Report::mbps`].
    pub unpaced: bool,
}

impl Report {
    /// Frames per second the machine managed on the wall clock.
    ///
    /// Deliberately NOT on the notional timeline the bitrate and the processor
    /// share use. This one answers "how fast did this machine go", which is a
    /// question about the wall clock by definition — dividing by a notional
    /// length would make an unpaced run report exactly its target rate and say
    /// nothing at all.
    ///
    /// Named `wall_fps` rather than `effective_fps` for that reason: the old
    /// name invited reading it as "the rate a session would see", which is what
    /// it is only when the run kept pace.
    pub fn wall_fps(&self) -> f64 {
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

    /// Mean share of the screen actually copied, per frame that carried content.
    ///
    /// Read against [`Report::mean_changed_share`]: if copying is doing its job
    /// the two are close, and if it is copying everything regardless this one
    /// sits at 100% while the other does not.
    pub fn mean_copied_share(&self) -> f64 {
        let px = u128::from(self.width) * u128::from(self.height);
        if self.frames_new == 0 || px == 0 {
            return 0.0;
        }
        (self.copied_px_total as f64 / self.frames_new as f64) / px as f64
    }

    /// How many times cheaper the shipping copy path was, at the median.
    ///
    /// `None` outside a comparison run, and `None` if either side has no
    /// samples. Reported at the median rather than p95 because the tails on a
    /// shared machine are contention, not copying.
    pub fn copy_speedup(&self) -> Option<f64> {
        let ours = self.readback.percentile(0.50)?;
        let other = self.compare.percentile(0.50)?;
        (ours > 0).then(|| other as f64 / ours as f64)
    }

    /// Split the copy cost into the part partial copying can remove and the
    /// part it cannot.
    ///
    /// Two points are known from a comparison run: the whole frame costs
    /// `F + V`, and a copy of share `s` costs `F + sV`. Solving gives the fixed
    /// overhead `F` — the map, the unmap, the wait on the GPU — which no amount
    /// of copying less will ever remove.
    ///
    /// This is the number that decides whether the optimisation is worth
    /// anything on a given machine. If `F` dominates, copying less buys nothing
    /// and the effort belongs elsewhere.
    ///
    /// `None` when there is no comparison, when the shares are degenerate, or
    /// when noise puts the arithmetic outside the physically sensible range —
    /// a negative fixed cost means the run was too noisy to decompose, and
    /// saying nothing beats reporting a tidy impossibility.
    pub fn copy_cost_split(&self) -> Option<(f64, f64)> {
        let full = self.compare.percentile(0.50)? as f64;
        let partial = self.readback.percentile(0.50)? as f64;
        let share = self.mean_copied_share();
        if !(0.0..0.95).contains(&share) || full <= 0.0 {
            return None;
        }
        let variable = (full - partial) / (1.0 - share);
        let fixed = full - variable;
        (fixed >= 0.0 && variable >= 0.0).then_some((fixed, variable))
    }

    /// Average bitrate over the whole run, in megabits per second.
    ///
    /// The denominator is the timeline the stream would have occupied, which is
    /// the wall clock only while the run is paced. Under `--frames` it is not:
    /// pacing is dropped on purpose so a fast machine does not read as slow, the
    /// frames go by several times faster than real time, and dividing bytes by
    /// the wall clock printed 11.93 Mbit/s for a stream that carries 1.99. The
    /// error was exactly the speed-up, and it went into the reports unmarked.
    ///
    /// Rebuilding it from the poll count and the target rate is right for both
    /// cases and stays right for a still screen: a poll that found nothing
    /// occupies its slot in the timeline and contributes no bytes, which is what
    /// makes an idle session cheap. Only the paced case is left on the clock,
    /// because there the polls are the clock, and a backend that spins — DDA
    /// returns immediately on a cursor-only frame — would otherwise inflate the
    /// count and understate the rate.
    ///
    /// The mirror image of this mistake was caught long ago: `--compare` refuses
    /// to print a bitrate at all, because several encoders per frame depress the
    /// rate the other way. One sign of the error was guarded, the other was not.
    pub fn mbps(&self) -> f64 {
        let secs = self.timeline_secs();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.encoded_bytes_total as f64 * 8.0) / secs / 1_000_000.0
    }

    /// How long this run represents, in seconds.
    ///
    /// The wall clock when the run kept pace, and the notional length of the
    /// frames otherwise. A fixed-frame run deliberately goes flat out, so its
    /// wall clock is "how fast is this machine", not "how long was the session"
    /// — billing a bitrate or a processor share to it answers a question nobody
    /// asked.
    ///
    /// One function rather than the same conditional in three places. `mbps` was
    /// taught this and the processor share was not, so the two sat on the same
    /// page describing different amounts of time.
    pub fn timeline_secs(&self) -> f64 {
        if self.unpaced {
            self.frames_seen as f64 / self.target_fps.max(1) as f64
        } else {
            self.elapsed.as_secs_f64()
        }
    }

    /// Cheapest and dearest configuration by median encode time.
    ///
    /// `None` when there is nothing to compare — fewer than two tracks, or no
    /// samples yet.
    pub fn encode_spread(&self) -> Option<(&Track, &Track)> {
        if self.tracks.len() < 2 {
            return None;
        }
        let key = |t: &&Track| t.encode.percentile(0.50);
        let cheap = self.tracks.iter().filter(|t| key(t).is_some()).min_by_key(key)?;
        let dear = self.tracks.iter().filter(|t| key(t).is_some()).max_by_key(key)?;
        Some((cheap, dear))
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
        // A verdict is a claim about the whole pipeline, and the budget is a sum
        // over whichever stages happened to run. With `--encode none` the sum
        // omits the single most expensive one and still earned a tick — the
        // machine was declared to keep up with work it had not been asked to do.
        if !self.missing_stages().is_empty() {
            return None;
        }
        self.budget_share(0.95).map(Verdict::classify)
    }

    /// Stages of the shipping pipeline that produced no samples in this run.
    ///
    /// Named rather than counted, because the report has to be able to say
    /// which one is absent: "no verdict" and "no verdict because nothing was
    /// encoded" are different messages to the person reading it.
    pub fn missing_stages(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.readback.is_empty() {
            out.push("копирование в память");
        }
        if self.convert.is_empty() {
            out.push("конвертация в YUV");
        }
        if self.encode.is_empty() {
            out.push("кодирование");
        }
        out
    }

    /// What `ИТОГО` is the sum of, in this run.
    ///
    /// Printed next to it because the row is a sum over stages that ran, and a
    /// reader comparing two reports has no way to see that one of them is a sum
    /// over fewer terms than the other.
    pub fn budget_summands(&self) -> Vec<&'static str> {
        let mut out = vec!["работа захвата"];
        if !self.readback.is_empty() {
            out.push("копирование");
        }
        if !self.convert.is_empty() {
            out.push("конвертация");
        }
        if !self.encode.is_empty() {
            out.push("кодирование");
        }
        out
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
    /// Classify a p95 share of the frame interval.
    ///
    /// One place for the thresholds, so the per-track table of a comparison run
    /// and the verdict of a single run cannot quietly disagree about what
    /// "keeps up" means.
    /// Below this the machine has room to spare.
    pub const COMFORTABLE_BELOW: f64 = 0.5;
    /// Below this it keeps up, with nothing left over for anything else.
    pub const TIGHT_BELOW: f64 = 0.8;
    /// Below this it keeps up only on an otherwise idle machine.
    pub const MARGINAL_BELOW: f64 = 1.0;

    pub fn classify(share: f64) -> Self {
        if share < Self::COMFORTABLE_BELOW {
            Verdict::Comfortable(share)
        } else if share < Self::TIGHT_BELOW {
            Verdict::Tight(share)
        } else if share < Self::MARGINAL_BELOW {
            Verdict::Marginal(share)
        } else {
            Verdict::Fails(share)
        }
    }

    /// One-character marker for a table row.
    pub fn mark(&self) -> &'static str {
        match self {
            Verdict::Comfortable(_) => "✓",
            Verdict::Tight(_) => "·",
            Verdict::Marginal(_) => "⚠",
            Verdict::Fails(_) => "✗",
        }
    }

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

/// Russian plural for a count: 1 кодер, 2 кодера, 5 кодеров.
///
/// The report is read by whoever is on duty, not by a compiler. «5 кодера» in
/// the middle of a paragraph is the kind of thing that makes a reader distrust
/// the numbers around it.
pub fn plural(n: u64, one: &str, few: &str, many: &str) -> String {
    let (last, last_two) = (n % 10, n % 100);
    let word = if last == 1 && last_two != 11 {
        one
    } else if (2..=4).contains(&last) && !(12..=14).contains(&last_two) {
        few
    } else {
        many
    };
    format!("{n} {word}")
}

/// One frame's cost for one configuration in a comparison run.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackStat {
    /// Capture and copy. Paid once per frame and identical for every track,
    /// which is exactly why it is passed in rather than measured per track.
    pub shared_us: u64,
    /// Conversion into this track's resolution.
    pub convert_us: u64,
    pub encode_us: u64,
    pub bytes: usize,
    pub keyframe: bool,
    /// The encoder took this frame and emitted nothing for it.
    /// See [`FrameStat::encode_dropped`].
    pub dropped: bool,
    /// The quantizer this configuration chose for this frame, 0..=63.
    pub quantizer: Option<u8>,
}

/// One encoder configuration inside a comparison run.
///
/// Every track is handed the same captured frames, in the same order, within the
/// same run. That is the entire point. Four separate runs of the sweep differed
/// in how much of the screen the operator happened to move — between 23% and
/// 35% — and the apparent effect of doubling the encoder's threads turned out to
/// be that difference and nothing else: the ratio of the medians matched the
/// ratio of the changed areas to two decimal places.
///
/// With the content held fixed the medians below can be read against each other
/// directly. No normalising by changed area, no arithmetic by hand, no chance of
/// crediting the codec for what the operator's scrolling did.
#[derive(Debug)]
pub struct Track {
    /// The configuration as the operator typed it, e.g. `vp9:s2:t4`.
    pub label: String,
    /// Resolution handed to this encoder, after downscaling.
    pub width: u32,
    pub height: u32,
    /// Conversion into this track's resolution.
    ///
    /// Charged to the track, because a track running alone would pay it. Tracks
    /// asking for the same scale share one conversion in wall-clock time — it is
    /// timed once and charged to each of them, which is what each would cost
    /// alone rather than what they cost together.
    pub convert: Latencies,
    pub encode: Latencies,
    /// Capture, copy, conversion and encoding: what this configuration would
    /// cost per frame if it were the only one running.
    pub budget: Latencies,
    pub frames: u64,
    pub bytes_total: u128,
    pub keyframes: u64,
    /// Frames the encoder took and emitted nothing for. See [`FrameStat::encode_dropped`].
    pub encode_drops: u64,
    /// Quantizer chosen per frame, 0..=63.
    pub quantizer: Latencies,
    pub keyframe_bytes_total: u128,
    pub delta_frames: u64,
    pub delta_bytes_total: u128,
}

impl Track {
    pub fn new(label: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            label: label.into(),
            width,
            height,
            convert: Latencies::default(),
            encode: Latencies::default(),
            budget: Latencies::default(),
            frames: 0,
            bytes_total: 0,
            keyframes: 0,
            encode_drops: 0,
            quantizer: Latencies::default(),
            keyframe_bytes_total: 0,
            delta_frames: 0,
            delta_bytes_total: 0,
        }
    }

    pub fn record(&mut self, stat: &TrackStat) {
        self.frames += 1;
        self.convert.push(stat.convert_us);
        self.encode.push(stat.encode_us);
        self.budget
            .push(stat.shared_us + stat.convert_us + stat.encode_us);
        if let Some(q) = stat.quantizer {
            self.quantizer.push(q as u64);
        }
        if stat.dropped {
            self.encode_drops += 1;
        } else {
            self.bytes_total += stat.bytes as u128;
            if stat.keyframe {
                self.keyframes += 1;
                self.keyframe_bytes_total += stat.bytes as u128;
            } else {
                self.delta_frames += 1;
                self.delta_bytes_total += stat.bytes as u128;
            }
        }
    }

    pub fn budget_share(&self, q: f64, interval_us: u64) -> Option<f64> {
        if interval_us == 0 {
            return None;
        }
        self.budget
            .percentile(q)
            .map(|us| us as f64 / interval_us as f64)
    }

    /// Verdict for this configuration alone, on the same thresholds as a single
    /// run. `None` until there are enough frames for a percentile to mean
    /// anything.
    pub fn verdict(&self, interval_us: u64) -> Option<Verdict> {
        if self.budget.len() < MIN_FRAMES_FOR_VERDICT {
            return None;
        }
        self.budget_share(0.95, interval_us).map(Verdict::classify)
    }

    /// Mean size of a delta frame, in bytes.
    ///
    /// The comparable size figure in this mode. Bitrate is not: running several
    /// encoders over every frame drives the capture rate down, so every track's
    /// bytes-per-second is depressed by however many tracks are in the run.
    /// Bytes per frame does not care.
    pub fn mean_delta_bytes(&self) -> Option<u128> {
        Report::mean_bytes(self.delta_bytes_total, self.delta_frames)
    }

    pub fn mean_keyframe_bytes(&self) -> Option<u128> {
        Report::mean_bytes(self.keyframe_bytes_total, self.keyframes)
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
            "  с новым содержимым      {} ({:.1} к/с по стенным часам)",
            self.frames_new,
            self.wall_fps()
        )?;
        writeln!(
            s,
            "  экран не менялся        {:.1}% опросов",
            self.still_share() * 100.0
        )?;
        if self.moved_rects_total > 0 && self.changed_px_total > 0 {
            // A share of the changed area, not an addition to it. This is the
            // part a copy path could satisfy inside the CPU buffer, sending
            // nothing across the bus — the size of a saving nobody has taken.
            let share = self.moved_px_total as f64 / self.changed_px_total as f64;
            writeln!(
                s,
                "  из них перенесено       {:.1}% площади ({} прямоугольников) — \
                 не перерисовано, а сдвинуто",
                share * 100.0,
                self.moved_rects_total
            )?;
        }
        if self.pointer_only > 0 {
            // Part of the share above, and not idle: the desktop image was the
            // same but the cursor had moved, and a product has to send that.
            // Printed apart so the still share is read as what it is — a fact
            // about the desktop image, not about how much there was to send.
            let share = self.pointer_only as f64 / self.frames_seen.max(1) as f64;
            writeln!(
                s,
                "  из них двигался курсор  {:.1}% опросов ({}) — экран тот же, а слать есть что",
                share * 100.0,
                self.pointer_only
            )?;
        }
        if self.frames_new > 0 {
            writeln!(
                s,
                "  менялось за кадр        {:.2}% площади, прямоугольников в среднем {:.1}",
                self.mean_changed_share() * 100.0,
                self.dirty_rects_total as f64 / self.frames_new as f64
            )?;
        }
        if self.frames_new > 0 && self.copied_px_total > 0 {
            writeln!(
                s,
                "  копировалось за кадр    {:.2}% площади",
                self.mean_copied_share() * 100.0
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
        if !self.convert.is_empty() {
            writeln!(s, "  конвертация в YUV    {}", self.convert.summary_ms())?;
        }
        if !self.encode.is_empty() {
            writeln!(s, "  кодирование          {}", self.encode.summary_ms())?;
        }
        if !self.budget.is_empty() {
            writeln!(s, "  ИТОГО на кадр        {}", self.budget.summary_ms())?;
            writeln!(s, "    сумма: {}", self.budget_summands().join(" + "))?;
        }
        if !self.outside.is_empty() {
            // What the four stage timers did not see, measured from outside the
            // whole iteration. Printed because ИТОГО can only ever understate:
            // it is a sum of stages, and anything between them is invisible to
            // it. A small number here is the licence to keep reading ИТОГО as
            // the frame's cost; a large one is not.
            writeln!(s, "  вне замеров стадий   {}", self.outside.summary_ms())?;
        }
        writeln!(s, "\n  ожидание кадра       {}", self.wait.summary_ms())?;
        writeln!(s, "  (ожидание — не расход: при цели {} к/с так и должно быть)", self.target_fps)?;

        if let Some(v) = self.verdict() {
            let interval_ms = self.interval_us() as f64 / 1000.0;
            writeln!(s, "\n-- Бюджет кадра --")?;
            writeln!(s, "  интервал при {} к/с      {:.1} мс", self.target_fps, interval_ms)?;
            if let (Some(p50), Some(p95)) = (self.budget_share(0.50), self.budget_share(0.95)) {
                writeln!(s, "  занято p50 / p95        {:.0}% / {:.0}%", p50 * 100.0, p95 * 100.0)?;
                // The verdict below is a step function of the p95, and the
                // steps were never printed. A reader saw 47% and a tick and
                // could not tell whether that was comfortable by a mile or by
                // half a point.
                //
                // It matters here more than it looks. Six consecutive runs of
                // one configuration, driven so the content is identical to
                // within a tenth of a percent of changed area, still spread
                // across 46-49% of the budget. A configuration landing near a
                // boundary will print opposite verdicts on consecutive runs
                // with nothing wrong; naming the boundary is what lets the
                // reader see that coming.
                writeln!(
                    s,
                    "  пороги вердикта         {:.0}% запас · {:.0}% впритык · {:.0}% почти не успевает",
                    Verdict::COMFORTABLE_BELOW * 100.0,
                    Verdict::TIGHT_BELOW * 100.0,
                    Verdict::MARGINAL_BELOW * 100.0
                )?;
                writeln!(
                    s,
                    "  (разброс между прогонами одной конфигурации — около трёх пунктов p95)"
                )?;
            }
            match &self.stand_in {
                Some(why) => {
                    writeln!(s, "  ~ Вердикта не будет: бюджет здесь занижен.")?;
                    for line in why.lines() {
                        writeln!(s, "    {line}")?;
                    }
                }
                None => writeln!(s, "  {} {}", v.mark(), v.explain())?,
            }
        } else if !self.budget.is_empty() && !self.missing_stages().is_empty() {
            writeln!(s, "\n-- Бюджет кадра --")?;
            writeln!(
                s,
                "  Вердикта не будет: в прогоне не участвовали стадии — {}.",
                self.missing_stages().join(", ")
            )?;
            writeln!(
                s,
                "  ИТОГО здесь — сумма только оставшихся, и сравнивать её с полным\n  \
                 прогоном нельзя: вердикт по ней означал бы, что машина успевает\n  \
                 делать работу, которой её не просили делать."
            )?;
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

        if !self.compare.is_empty() {
            writeln!(s, "\n-- Сравнение путей копирования, мс --")?;
            writeln!(s, "                            p50    p95    p99    max")?;
            writeln!(s, "  весь кадр            {}", self.compare.summary_ms())?;
            writeln!(s, "  изменившиеся области {}", self.readback.summary_ms())?;
            // The two rows are only comparable if they are the same frames. A
            // source that reports no dirty metadata copies the whole frame into
            // the «изменившиеся области» row, and then the table prints a
            // speedup between the full path and the full path under a line
            // claiming both ran on one frame. The counts are the only thing
            // that can tell, so they are checked rather than assumed.
            if self.compare.len() != self.readback.len() {
                let orphans = self.readback.len().abs_diff(self.compare.len());
                writeln!(
                    s,
                    "\n  ⚠ ускорение не считается: строки сняты по разному числу кадров\n    \
                     ({} против {}, расхождение {orphans}). Обычно это кадры, по которым\n    \
                     источник не отдал списка изменившихся областей, и в верхнюю строку\n    \
                     попало полное копирование под видом частичного.",
                    self.compare.len(),
                    self.readback.len()
                )?;
            } else {
                if let Some(x) = self.copy_speedup() {
                    writeln!(
                        s,
                        "\n  на медиане частичное копирование быстрее в {x:.1} раза"
                    )?;
                }
                writeln!(
                    s,
                    "  оба пути отработали ОДИН И ТОТ ЖЕ кадр, порядок чередуется"
                )?;
            }
            if let Some((fixed, variable)) = self.copy_cost_split() {
                writeln!(s, "\n  из стоимости полного копирования:")?;
                writeln!(
                    s,
                    "    {:.1} мс не зависит от площади (map, unmap, ожидание GPU)",
                    fixed / 1000.0
                )?;
                writeln!(
                    s,
                    "    {:.1} мс пропорционально площади — только это и экономится",
                    variable / 1000.0
                )?;
            }
            writeln!(
                s,
                "\n  частота кадров в этом режиме занижена — на каждый кадр две копии,"
            )?;
            writeln!(
                s,
                "  и вторая копия нагружает GPU, так что бюджет здесь пессимистичен"
            )?;
        }

        if !self.tracks.is_empty() {
            let interval = self.interval_us();
            let w = self
                .tracks
                .iter()
                .map(|t| t.label.chars().count())
                .max()
                .unwrap_or(0)
                .max("конфигурация".chars().count());

            writeln!(s, "\n-- Сравнение на одних и тех же кадрах --")?;
            writeln!(
                s,
                "  {:<w$}  {:>9}  {:>8}  {:>19}  {:>11}  {:>9}  {:>9}  {:>4}",
                "конфигурация",
                "размер",
                "конверт.",
                "кодирование p50/p95",
                "бюджет p95",
                "разн.кадр",
                "ключ.кадр",
                "q50"
            )?;
            for t in &self.tracks {
                let size = format!("{}×{}", t.width, t.height);
                let enc = match (t.encode.percentile(0.50), t.encode.percentile(0.95)) {
                    (Some(a), Some(b)) => {
                        format!("{:.1} / {:.1} мс", a as f64 / 1000.0, b as f64 / 1000.0)
                    }
                    _ => "—".to_owned(),
                };
                let bud = match (t.budget_share(0.95, interval), t.verdict(interval)) {
                    // A stand-in source gets the number but never the mark: the
                    // number is comparable between rows, the mark would be a
                    // verdict this run is not entitled to give.
                    (Some(share), _) if self.stand_in.is_some() => {
                        format!("{:.0}% ~", share * 100.0)
                    }
                    (Some(share), Some(v)) => format!("{:.0}% {}", share * 100.0, v.mark()),
                    // Enough samples to divide, not enough for the division to
                    // mean anything. Saying so beats a confident tick.
                    (Some(share), None) => format!("{:.0}% ?", share * 100.0),
                    _ => "—".to_owned(),
                };
                let bytes = match t.mean_delta_bytes() {
                    Some(b) => format!("{b} Б"),
                    None => "—".to_owned(),
                };
                let conv = match t.convert.percentile(0.50) {
                    Some(us) => format!("{:.1} мс", us as f64 / 1000.0),
                    None => "—".to_owned(),
                };
                // The median quantizer, because the whole argument about the
                // ceiling was made from byte counts with this number unread. A
                // row sitting at 56 is a row rate control could not finish.
                let q = match t.quantizer.percentile(0.50) {
                    Some(q) => format!("{q}"),
                    None => "—".to_owned(),
                };
                // Count and mean size together, because either alone misleads.
                // A keyframe here is fifty times a delta frame, so a row with
                // one more of them carries a megabit per minute the delta
                // column cannot show — and the delta column is what the reader
                // has been comparing rows by. Collected since tracks existed
                // and printed by nothing until now.
                let key = match t.mean_keyframe_bytes() {
                    Some(b) => format!("{}×{} КБ", t.keyframes, b / 1024),
                    None => "—".to_owned(),
                };
                writeln!(
                    s,
                    "  {:<w$}  {:>9}  {:>8}  {:>19}  {:>11}  {:>9}  {:>9}  {:>4}",
                    t.label, size, conv, enc, bud, bytes, key, q
                )?;
            }

            // Its own line rather than a column, because it is zero in every
            // run so far and a column of zeros is a column nobody reads. When
            // it stops being zero it is the most important number on the page:
            // the frames in it never reached a receiver at all, and every other
            // figure in the row was computed as though the screen had been
            // quiet.
            let dropping: Vec<&Track> = self.tracks.iter().filter(|t| t.encode_drops > 0).collect();
            if !dropping.is_empty() {
                writeln!(s, "\n  ⚠ кодер принял кадры и не выдал ничего:")?;
                for t in dropping {
                    writeln!(
                        s,
                        "    {:<w$}  {} из {}",
                        t.label,
                        plural(t.encode_drops, "кадр", "кадра", "кадров"),
                        t.frames
                    )?;
                }
            }

            if let Some((cheap, dear)) = self.encode_spread() {
                let (lo, hi) = (
                    cheap.encode.percentile(0.50).unwrap_or(0) as f64 / 1000.0,
                    dear.encode.percentile(0.50).unwrap_or(0) as f64 / 1000.0,
                );
                if lo > 0.0 {
                    writeln!(
                        s,
                        "\n  разброс по кодированию: {:.1} мс ({}) … {:.1} мс ({}), то есть {:.2}×",
                        lo,
                        cheap.label,
                        hi,
                        dear.label,
                        hi / lo
                    )?;
                }
            }

            let frames = self.tracks.first().map_or(0, |t| t.frames);
            writeln!(
                s,
                "\n  Все конфигурации получили ОДНИ И ТЕ ЖЕ {}, порядок кодирования",
                plural(frames, "кадр", "кадра", "кадров")
            )?;
            writeln!(
                s,
                "  чередуется по кадрам. Содержимое больше не переменная: медианы"
            )?;
            writeln!(
                s,
                "  сравнимы напрямую, нормировать по изменившейся площади не нужно."
            )?;
            writeln!(
                s,
                "\n  Битрейт здесь не приводится: {} на каждый кадр занижают частоту,",
                plural(self.tracks.len() as u64, "кодер", "кодера", "кодеров")
            )?;
            writeln!(
                s,
                "  и байты в секунду просели бы у всех дорожек одинаково неверно."
            )?;
            writeln!(
                s,
                "  «Бюджет» — во что обошлась бы эта конфигурация, будь она одна."
            )?;
            if let Some(why) = &self.stand_in {
                writeln!(s, "\n  ~ Бюджет здесь ЗАНИЖЕН и вердиктом не является.")?;
                for line in why.lines() {
                    writeln!(s, "    {line}")?;
                }
                writeln!(
                    s,
                    "  Строки сравнимы между собой и с такой же таблицей на другой машине;"
                )?;
                writeln!(s, "  «проходит или нет» решается прогоном по живому столу.")?;
            }
            if frames < MIN_FRAMES_FOR_VERDICT as u64 {
                writeln!(
                    s,
                    "\n  ⚠ Кадров с содержимым {frames}, для вердикта нужно {MIN_FRAMES_FOR_VERDICT}."
                )?;
                writeln!(s, "  Столбец «бюджет» описывает один неудачный кадр, а не машину.")?;
            }
        }

        if self.encoded_bytes_total > 0 {
            writeln!(s, "\n-- Поток --")?;
            writeln!(s, "  средний битрейт         {:.2} Мбит/с", self.mbps())?;
            if self.unpaced {
                writeln!(
                    s,
                    "  (темп здесь не выдерживался, поэтому битрейт считан не по\n   \
                     настенным часам, а по {} опросам при цели {} к/с — то есть по\n   \
                     тому времени, которое поток занял бы на самом деле)",
                    self.frames_seen, self.target_fps
                )?;
            }
            if let Some(b) = Report::mean_bytes(self.keyframe_bytes_total, self.keyframes) {
                writeln!(s, "  ключевой кадр           {} КБ ({} шт.)", b / 1024, self.keyframes)?;
            }
            if let Some(b) = Report::mean_bytes(self.delta_bytes_total, self.delta_frames) {
                writeln!(s, "  разностный кадр         {} Б ({} шт.)", b, self.delta_frames)?;
            }
            if !self.quantizer.is_empty() {
                writeln!(s, "                          p50    p95    max")?;
                writeln!(s, "  квантователь          {}", self.quantizer.summary_plain())?;
            }
            if self.encode_drops > 0 {
                writeln!(
                    s,
                    "  кодер уронил кадров     {} — принял и не выдал ничего.\n  \
                     Это не тихий экран, а нехватка: такие кадры до получателя не дошли",
                    self.encode_drops
                )?;
            }
        }

        writeln!(s, "\n-- Процессор --")?;
        write!(s, "{}", self.cpu.render(Duration::from_secs_f64(self.timeline_secs())))?;

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

    /// Feed one frame to a comparison recorder. Shared costs are identical for
    /// every track by construction — that is the point of the mode.
    fn compare_frame(r: &mut Recorder, tracks: &[(usize, u64, u64, usize)]) {
        r.record(&FrameStat {
            wait_us: 1_000,
            work_us: 100,
            readback_us: 900,
            is_new: true,
            changed_px: 5_000,
            ..Default::default()
        });
        for &(index, convert_us, encode_us, bytes) in tracks {
            r.record_track(
                index,
                &TrackStat {
                    shared_us: 1_000,
                    convert_us,
                    encode_us,
                    bytes,
                    keyframe: false,
                    dropped: false,
                    quantizer: None,
                },
            );
        }
    }

    #[test]
    fn russian_counts_agree_with_their_nouns() {
        assert_eq!(plural(1, "кадр", "кадра", "кадров"), "1 кадр");
        assert_eq!(plural(2, "кадр", "кадра", "кадров"), "2 кадра");
        assert_eq!(plural(5, "кадр", "кадра", "кадров"), "5 кадров");
        // The teens are the trap: 11 takes the same form as 5, not as 1.
        assert_eq!(plural(11, "кадр", "кадра", "кадров"), "11 кадров");
        assert_eq!(plural(12, "кадр", "кадра", "кадров"), "12 кадров");
        assert_eq!(plural(14, "кадр", "кадра", "кадров"), "14 кадров");
        assert_eq!(plural(21, "кадр", "кадра", "кадров"), "21 кадр");
        assert_eq!(plural(102, "кадр", "кадра", "кадров"), "102 кадра");
        assert_eq!(plural(111, "кадр", "кадра", "кадров"), "111 кадров");
        assert_eq!(plural(0, "кадр", "кадра", "кадров"), "0 кадров");
    }

    #[test]
    fn verdict_thresholds_live_in_one_place() {
        assert!(matches!(Verdict::classify(0.49), Verdict::Comfortable(_)));
        assert!(matches!(Verdict::classify(0.50), Verdict::Tight(_)));
        assert!(matches!(Verdict::classify(0.79), Verdict::Tight(_)));
        assert!(matches!(Verdict::classify(0.80), Verdict::Marginal(_)));
        assert!(matches!(Verdict::classify(0.99), Verdict::Marginal(_)));
        assert!(matches!(Verdict::classify(1.00), Verdict::Fails(_)));
    }

    #[test]
    fn each_track_carries_its_own_budget() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let big = r.add_track("vp9:s1", 100, 100);
        let small = r.add_track("vp9:s2", 50, 50);
        for _ in 0..40 {
            compare_frame(&mut r, &[(big, 6_000, 40_000, 8_000), (small, 3_000, 10_000, 5_000)]);
        }
        let rep = r.finish(Duration::from_secs(1));

        // The shared budget is not collected in this mode, and must not be: with
        // several encoders per frame there is no single number it could mean.
        assert!(rep.budget.is_empty());
        assert!(rep.verdict().is_none());

        assert_eq!(rep.tracks.len(), 2);
        assert_eq!(rep.tracks[big].budget.percentile(0.50), Some(47_000));
        assert_eq!(rep.tracks[small].budget.percentile(0.50), Some(14_000));

        let interval = rep.interval_us();
        assert!(matches!(rep.tracks[big].verdict(interval), Some(Verdict::Fails(_))));
        assert!(matches!(rep.tracks[small].verdict(interval), Some(Verdict::Comfortable(_))));
    }

    #[test]
    fn spread_names_the_cheapest_and_the_dearest() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let a = r.add_track("vp9:s1", 100, 100);
        let b = r.add_track("vp9:s2", 50, 50);
        let c = r.add_track("vp8:s2", 50, 50);
        for _ in 0..40 {
            compare_frame(
                &mut r,
                &[(a, 6_000, 40_000, 8_000), (b, 3_000, 10_000, 5_000), (c, 3_000, 12_000, 6_000)],
            );
        }
        let rep = r.finish(Duration::from_secs(1));
        let (cheap, dear) = rep.encode_spread().expect("три дорожки есть");
        assert_eq!(cheap.label, "vp9:s2");
        assert_eq!(dear.label, "vp9:s1");
    }

    #[test]
    fn one_track_alone_has_nothing_to_compare() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let a = r.add_track("vp9", 100, 100);
        compare_frame(&mut r, &[(a, 1_000, 2_000, 100)]);
        let rep = r.finish(Duration::from_secs(1));
        assert!(rep.encode_spread().is_none());
    }

    #[test]
    fn a_thin_comparison_says_so_instead_of_judging() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let a = r.add_track("vp9:s2", 50, 50);
        // Five frames: nearest-rank p95 over five samples is just the worst one.
        for _ in 0..5 {
            compare_frame(&mut r, &[(a, 3_000, 10_000, 5_000)]);
        }
        let rep = r.finish(Duration::from_secs(1));
        assert!(rep.tracks[a].verdict(rep.interval_us()).is_none());
        let text = rep.to_string();
        assert!(text.contains("для вердикта нужно 30"), "{text}");
        // A question mark, not a tick: the share is computable, the judgement is
        // not.
        assert!(text.contains("% ?"), "{text}");
    }

    #[test]
    fn comparison_table_lists_every_configuration() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let a = r.add_track("vp9:s1", 100, 100);
        let b = r.add_track("vp8:s2:t4", 50, 50);
        for _ in 0..40 {
            compare_frame(&mut r, &[(a, 6_000, 40_000, 8_000), (b, 3_000, 10_000, 5_000)]);
        }
        let text = r.finish(Duration::from_secs(1)).to_string();
        assert!(text.contains("vp9:s1"), "{text}");
        assert!(text.contains("vp8:s2:t4"), "{text}");
        assert!(text.contains("100×100"), "{text}");
        assert!(text.contains("50×50"), "{text}");
        // Bitrate would be depressed by the number of encoders per frame, so it
        // is deliberately absent here.
        assert!(!text.contains("средний битрейт"), "{text}");
    }

    #[test]
    fn keyframes_do_not_contaminate_the_delta_size() {
        let mut track = Track::new("vp9", 64, 64);
        track.record(&TrackStat { encode_us: 1, bytes: 60_000, keyframe: true, ..Default::default() });
        for _ in 0..3 {
            track.record(&TrackStat { encode_us: 1, bytes: 1_000, keyframe: false, ..Default::default() });
        }
        assert_eq!(track.mean_delta_bytes(), Some(1_000));
        assert_eq!(track.mean_keyframe_bytes(), Some(60_000));
        assert_eq!(track.frames, 4);
    }

    #[test]
    fn the_stages_are_checked_against_the_whole_iteration() {
        // ИТОГО is a sum of stage timers and cannot see the work between them,
        // so it can only understate. The gap is measured rather than assumed.
        let mut r = Recorder::new("тест", 100, 100, 30);
        r.record(&FrameStat {
            is_new: true,
            wait_us: 20_000,
            work_us: 1_000,
            readback_us: 2_000,
            convert_us: 3_000,
            encode_us: Some(4_000),
            // Twenty spent waiting, ten inside the stages, and two the stages
            // never saw.
            iter_us: 32_000,
            ..Default::default()
        });
        let rep = r.finish(Duration::from_secs(1));

        assert_eq!(rep.budget.percentile(0.50), Some(10_000));
        assert_eq!(rep.outside.percentile(0.50), Some(2_000));
    }

    #[test]
    fn a_frame_whose_clocks_disagree_reports_no_negative_work() {
        // The two clocks are read at different points, so rounding can put the
        // iteration a shade under the stages. That is noise, and it must not
        // wrap a u64.
        let mut r = Recorder::new("тест", 100, 100, 30);
        r.record(&FrameStat {
            is_new: true,
            work_us: 5_000,
            iter_us: 4_999,
            ..Default::default()
        });
        assert_eq!(r.finish(Duration::from_secs(1)).outside.percentile(0.50), Some(0));
    }

    #[test]
    fn a_stage_that_cost_nothing_still_counts_as_a_sample() {
        // The timers truncate to whole microseconds, so a cheap frame reads 0.
        // Dropping those zeros censored the sample and biased every percentile
        // on these two rows upward — worst on the cheapest stage, which is the
        // one an optimisation is trying to produce.
        let mut r = Recorder::new("тест", 100, 100, 30);
        for _ in 0..9 {
            r.record(&FrameStat { readback_us: 0, convert_us: 0, is_new: true, ..Default::default() });
        }
        r.record(&FrameStat { readback_us: 8_000, convert_us: 8_000, is_new: true, ..Default::default() });
        let rep = r.finish(Duration::from_secs(1));

        assert_eq!(rep.readback.len(), 10, "нулевые кадры выброшены из выборки");
        assert_eq!(rep.convert.len(), 10);
        // Nine of ten frames cost nothing, so the median must say so. Censored,
        // it would have reported the single expensive frame as the median.
        assert_eq!(rep.readback.percentile(0.50), Some(0));
        assert_eq!(rep.readback.max(), Some(8_000));
    }

    #[test]
    fn an_unpaced_run_bills_the_bitrate_to_the_timeline_it_would_have_taken() {
        // `--frames` over a screenshot feeds frames as fast as the machine takes
        // them, so the wall clock is not the timeline the stream occupies.
        // 300 polls at 30 fps is ten seconds however fast they actually went.
        let mut paced = Recorder::new("тест", 100, 100, 30);
        let mut unpaced = Recorder::new("тест", 100, 100, 30);
        unpaced.note_unpaced();
        for r in [&mut paced, &mut unpaced] {
            for _ in 0..300 {
                r.record(&FrameStat {
                    is_new: true,
                    encode_us: Some(4_000),
                    encoded_bytes: Some(7_795),
                    ..Default::default()
                });
            }
        }
        // The run took 1.7 s of wall clock, six times faster than real time.
        let wall = Duration::from_millis(1_700);
        let on_the_clock = paced.finish(wall).mbps();
        let on_the_timeline = unpaced.finish(wall).mbps();

        // 7795 B x 300 x 8 / 10 s = 1.87 Mbit/s.
        assert!((on_the_timeline - 1.8708).abs() < 0.01, "{on_the_timeline}");
        // And the old behaviour, kept for the paced case, is the six-fold
        // overstatement that went into the committed reports unmarked.
        assert!(on_the_clock > on_the_timeline * 5.0, "{on_the_clock}");
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
                // Every stage contributes, because a verdict is now a claim
                // about the whole pipeline and is withheld when one is absent.
                // The split keeps the total at `cost_us` so this test still
                // measures the thresholds and nothing else.
                r.record(&FrameStat {
                    is_new: true,
                    work_us: cost_us - 3,
                    readback_us: 1,
                    convert_us: 1,
                    encode_us: Some(1),
                    ..Default::default()
                });
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
            r.record(&FrameStat {
                is_new: true,
                work_us: 1_000,
                readback_us: 1,
                convert_us: 1,
                encode_us: Some(1),
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(30));
        assert!(rep.verdict().is_none());
        // The section must still explain itself rather than silently vanish.
        let text = rep.to_string();
        assert!(text.contains("Вердикта не будет"), "{text}");
        assert!(text.contains("С ДВИЖЕНИЕМ"), "{text}");
    }

    /// A run with `--encode none` used to collect a budget of capture plus copy
    /// plus conversion, compare it against the frame interval, and award a tick.
    /// The machine was declared to keep up with work nobody asked it to do.
    #[test]
    fn a_run_that_never_encoded_earns_no_verdict() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        for _ in 0..(MIN_FRAMES_FOR_VERDICT * 2) {
            r.record(&FrameStat {
                is_new: true,
                work_us: 1_000,
                readback_us: 500,
                convert_us: 500,
                // No encoder in this run.
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(30));

        assert!(rep.budget.len() >= MIN_FRAMES_FOR_VERDICT, "кадров достаточно");
        assert!(rep.verdict().is_none(), "но вердикта быть не должно");
        assert_eq!(rep.missing_stages(), vec!["кодирование"]);

        let text = rep.to_string();
        assert!(text.contains("не участвовали стадии"), "{text}");
        assert!(text.contains("кодирование"), "{text}");
        // And ИТОГО has to say what it is a sum of, so two reports summing
        // different stages cannot be read as the same figure.
        assert!(text.contains("сумма: работа захвата + копирование + конвертация"), "{text}");
        assert!(!text.contains("сумма: работа захвата + копирование + конвертация + кодирование"));
    }

    /// The rejected copy path is work the operator asked for, not overhead the
    /// stage timers missed. Charged to «вне замеров стадий» it made the shipping
    /// pipeline look like it had unexplained cost in it.
    #[test]
    fn the_rejected_copy_path_is_not_charged_to_unmeasured_overhead() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        for _ in 0..40 {
            r.record(&FrameStat {
                is_new: true,
                work_us: 1_000,
                readback_us: 1_000,
                convert_us: 1_000,
                encode_us: Some(1_000),
                // The second copy, run deliberately alongside.
                compare_us: Some(5_000),
                // Everything above, plus the second copy, plus 100 µs of real
                // unmeasured overhead.
                iter_us: 9_100,
                wait_us: 0,
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(2));
        assert_eq!(
            rep.outside.percentile(0.50),
            Some(100),
            "вне стадий должно остаться только настоящее"
        );
    }

    /// Two rows over different frames are not a comparison, whatever the ratio
    /// between them says.
    #[test]
    fn a_copy_table_over_unequal_frame_counts_refuses_to_state_a_speedup() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        for i in 0..40 {
            r.record(&FrameStat {
                is_new: true,
                work_us: 1_000,
                readback_us: 2_000,
                // Only some frames carried the second path.
                compare_us: if i % 2 == 0 { Some(4_000) } else { None },
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(2));
        let text = rep.to_string();
        assert!(text.contains("ускорение не считается"), "{text}");
        assert!(!text.contains("быстрее в"), "{text}");
    }

    /// The bitrate was taught the notional timeline and the processor share was
    /// not, so one page carried two different amounts of elapsed time.
    #[test]
    fn the_bitrate_and_the_processor_share_bill_to_the_same_timeline() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        for _ in 0..60 {
            r.record(&FrameStat { is_new: true, work_us: 1_000, ..Default::default() });
        }
        let mut rep = r.finish(Duration::from_secs(1));
        rep.unpaced = true;

        // Sixty frames at a target of thirty is two seconds of session,
        // whatever the one second of wall clock it took to produce them.
        assert!((rep.timeline_secs() - 2.0).abs() < 1e-9, "{}", rep.timeline_secs());
        // wall_fps deliberately stays on the wall clock: it answers how fast
        // the machine went, which is not a question about the session.
        assert!((rep.wall_fps() - 60.0).abs() < 1e-9, "{}", rep.wall_fps());
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
    fn comparison_reports_the_speedup_at_the_median() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        for _ in 0..MIN_FRAMES_FOR_VERDICT {
            r.record(&FrameStat {
                is_new: true,
                readback_us: 4_000,
                compare_us: Some(20_000),
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(10));
        assert!((rep.copy_speedup().unwrap() - 5.0).abs() < 1e-9);
        let text = rep.to_string();
        assert!(text.contains("Сравнение путей копирования"), "{text}");
        assert!(text.contains("быстрее в 5.0 раза"), "{text}");
    }

    #[test]
    fn comparison_separates_fixed_cost_from_the_part_that_scales() {
        // The shape measured on the VM: 43% of the screen copied, whole frame
        // 7.0 ms, partial 4.5 ms. Linear scaling alone would have predicted
        // 3.0 ms, so something does not scale — and that residue is the point.
        let mut r = Recorder::new("тест", 100, 100, 30);
        for _ in 0..MIN_FRAMES_FOR_VERDICT {
            r.record(&FrameStat {
                is_new: true,
                copied_px: 4_345,
                readback_us: 4_500,
                compare_us: Some(7_000),
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(10));
        let (fixed, variable) = rep.copy_cost_split().expect("разложение");
        assert!((fixed - 2_583.0).abs() < 5.0, "фикс {fixed}");
        assert!((variable - 4_417.0).abs() < 5.0, "перем {variable}");
        assert!(rep.to_string().contains("не зависит от площади"));
    }

    #[test]
    fn a_noisy_run_is_not_decomposed_into_impossibilities() {
        // Partial slower than full happens under contention. Solving anyway
        // yields a negative fixed cost, which is tidy and false.
        let mut r = Recorder::new("тест", 100, 100, 30);
        for _ in 0..MIN_FRAMES_FOR_VERDICT {
            r.record(&FrameStat {
                is_new: true,
                copied_px: 5_000,
                readback_us: 9_000,
                compare_us: Some(7_000),
                ..Default::default()
            });
        }
        let rep = r.finish(Duration::from_secs(10));
        assert!(rep.copy_cost_split().is_none());
    }

    #[test]
    fn no_comparison_means_no_speedup_claimed() {
        let mut r = Recorder::new("тест", 1920, 1080, 30);
        r.record(&FrameStat { is_new: true, readback_us: 4_000, ..Default::default() });
        let rep = r.finish(Duration::from_secs(1));
        assert!(rep.copy_speedup().is_none());
        assert!(!rep.to_string().contains("Сравнение путей"));
    }

    #[test]
    fn copied_and_changed_are_tracked_apart() {
        // A backend asked for partial copies but unable to do them reports the
        // full screen copied against a fraction changed. That gap is the whole
        // point of the measurement, so it must not be averaged away.
        let mut r = Recorder::new("тест", 100, 100, 30);
        r.record(&FrameStat {
            is_new: true,
            changed_px: 2_500,
            copied_px: 10_000,
            ..Default::default()
        });
        let rep = r.finish(Duration::from_secs(1));
        assert!((rep.mean_changed_share() - 0.25).abs() < 1e-9);
        assert!((rep.mean_copied_share() - 1.0).abs() < 1e-9);
        assert!(rep.to_string().contains("копировалось за кадр"));
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
        assert_eq!(rep.wall_fps(), 0.0);
        assert_eq!(rep.mbps(), 0.0);
        // Rendering must work on an empty run: the operator needs to see *that*
        // nothing was captured, not an empty terminal.
        let _ = rep.to_string();
    }

    /// A comparison run converts once per *scale* and charges each track for
    /// its own, so the shared conversion timer is never written. Printing it
    /// anyway put `конвертация в YUV 0.0 0.0 0.0 0.0` into every comparison
    /// report on disk, under a stage the per-track column shows costing 1.6 ms.
    #[test]
    fn a_comparison_does_not_print_a_conversion_it_never_timed() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let a = r.add_track("vp9:s1", 100, 100);
        let b = r.add_track("vp9:s2", 50, 50);
        for _ in 0..40 {
            compare_frame(&mut r, &[(a, 6_000, 40_000, 8_000), (b, 3_000, 10_000, 5_000)]);
        }
        let text = r.finish(Duration::from_secs(1)).to_string();
        assert!(!text.contains("конвертация в YUV"), "{text}");
        // The per-track column is where it really is, and it must stay.
        assert!(text.contains("конверт."), "{text}");
        // Readback is not gated with it: that one is genuinely paid once per
        // frame and shared, and dropping it would lose a real measurement.
        assert!(text.contains("копирование в память"), "{text}");
    }

    /// The same stage in a single-encoder run is measured and must still print,
    /// or the fix above would have removed the row rather than the falsehood.
    #[test]
    fn a_single_run_still_prints_its_conversion() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        for _ in 0..40 {
            r.record(&FrameStat {
                is_new: true,
                work_us: 100,
                readback_us: 900,
                convert_us: 1_600,
                encode_us: Some(10_000),
                ..Default::default()
            });
        }
        let text = r.finish(Duration::from_secs(1)).to_string();
        assert!(text.contains("конвертация в YUV"), "{text}");
        assert!(text.contains("1.6"), "{text}");
    }

    /// Feed one frame where each track may emit a keyframe or emit nothing.
    fn compare_frame_kinds(r: &mut Recorder, tracks: &[(usize, usize, bool, bool)]) {
        r.record(&FrameStat { wait_us: 1_000, work_us: 100, readback_us: 900, is_new: true, ..Default::default() });
        for &(index, bytes, keyframe, dropped) in tracks {
            r.record_track(
                index,
                &TrackStat {
                    shared_us: 1_000,
                    convert_us: 1_000,
                    encode_us: 10_000,
                    bytes,
                    keyframe,
                    dropped,
                    quantizer: Some(56),
                },
            );
        }
    }

    /// Both figures were collected from the day tracks existed and printed by
    /// nothing. They are the first thing to check when two rows disagree about
    /// bitrate: one extra keyframe is fifty delta frames, and a dropped frame
    /// is a frame the receiver never saw at all.
    #[test]
    fn the_comparison_table_names_keyframes_and_dropped_frames() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let a = r.add_track("vp9:s1:m56", 100, 100);
        let b = r.add_track("vp9:s1:m63", 100, 100);
        // Track `a` opens with a 100 KB keyframe and later drops two frames;
        // track `b` does neither.
        compare_frame_kinds(&mut r, &[(a, 102_400, true, false), (b, 51_200, true, false)]);
        for i in 0..39 {
            let a_drops = i < 2;
            compare_frame_kinds(
                &mut r,
                &[(a, if a_drops { 0 } else { 8_000 }, false, a_drops), (b, 5_000, false, false)],
            );
        }
        let text = r.finish(Duration::from_secs(1)).to_string();

        assert!(text.contains("ключ.кадр"), "нет заголовка столбца:\n{text}");
        assert!(text.contains("1×100 КБ"), "нет ключевого кадра дорожки a:\n{text}");
        assert!(text.contains("1×50 КБ"), "нет ключевого кадра дорожки b:\n{text}");
        assert!(text.contains("кодер принял кадры и не выдал ничего"), "{text}");
        assert!(text.contains("2 кадра из 40"), "не названы потери дорожки a:\n{text}");
        // The track that dropped nothing must not appear in that list at all.
        // Only the list itself is examined — it ends at the blank line, and the
        // rest of the report names every track for other reasons.
        let list = text
            .split("не выдал ничего:\n")
            .nth(1)
            .expect("список есть")
            .split("\n\n")
            .next()
            .expect("список кончается пустой строкой");
        assert!(list.contains("vp9:s1:m56"), "дорожка с потерями пропала:\n{list}");
        assert!(!list.contains("vp9:s1:m63"), "дорожка без потерь попала в список:\n{list}");
    }

    /// No drops is the case in every run so far, and it must stay silent: a
    /// warning that fires always is a warning nobody reads.
    #[test]
    fn a_comparison_without_drops_says_nothing_about_them() {
        let mut r = Recorder::new("тест", 100, 100, 30);
        let a = r.add_track("vp9:s1", 100, 100);
        for _ in 0..40 {
            compare_frame_kinds(&mut r, &[(a, 8_000, false, false)]);
        }
        let text = r.finish(Duration::from_secs(1)).to_string();
        assert!(!text.contains("не выдал ничего"), "{text}");
        // And with no keyframe at all the column says so rather than lying.
        assert!(text.contains("ключ.кадр"), "{text}");
    }
}
