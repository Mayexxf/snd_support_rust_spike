//! How much of the text survived, measured where text actually lives.
//!
//! The codec comparison so far asked one question: fix a bitrate, see what the
//! picture looks like. That is the wrong way round for this product. Nobody
//! chooses two megabits and then finds out whether the client can read the
//! screen; they need the screen readable and want to know what it costs. To ask
//! it that way round there has to be a bar, and a bar needs a measure that can
//! tell readable from unreadable.
//!
//! SSIM cannot. That is not a guess — it was watched happening: at about two
//! megabits, VP9 scored 0.696 against hardware H.264's 0.761, a gap that reads
//! as a modest difference in grade, while the crops showed one of them readable
//! in whole phrases and the other mush outside a single static column. A mean
//! over the whole frame is dominated by the flat background, which every codec
//! reproduces perfectly, and screen content is mostly flat background.
//!
//! So the measure here throws the background away. Two ideas, both narrow on
//! purpose:
//!
//! * **Only edges count.** A pixel is worth looking at if the *source* changes
//!   sharply there — that is what a glyph stroke is. Damage in the middle of a
//!   white page is invisible; damage on a stroke is the thing that stops a word
//!   being a word.
//! * **Damage is counted, not averaged.** The figure is the *share* of those
//!   pixels the codec moved further than a threshold. Averaging is what let
//!   SSIM hide a smeared paragraph behind an intact desktop.
//!
//! Reported as a share per frame and then as percentiles across frames, because
//! a session where one frame in twenty is unreadable is not a readable session,
//! and a mean over frames would call it one.
//!
//! Luma only. Colour matters for coloured text, but a glyph that survives in Y
//! is legible and one that does not is not, whatever the chroma did.
//!
//! Several thresholds in a single pass, so that where the bar goes can be
//! settled by looking at cases already judged by eye rather than by re-running
//! everything for each candidate.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// How sharply the source must change at a pixel for it to count as an edge.
///
/// Sum of the forward differences right and down, on 0..=255 luma. Antialiased
/// text at this size clears it comfortably; JPEG-ish mottle in a photograph
/// does not, which is the intent — this is a screen-content measure and says
/// nothing useful about video of the outdoors.
const EDGE_MIN: u16 = 32;

/// How far a pixel must move to be called damaged.
///
/// Four of them, measured together, because the right one is a question about
/// human reading and not about arithmetic. It gets settled against crops whose
/// readability has already been judged by eye.
pub const DAMAGE: [u8; 4] = [8, 16, 24, 32];

/// Which of [`DAMAGE`] the per-frame curve and the readability bar are read at.
///
/// The third, meaning 24. Settled against crops: 28% of stroke pixels past it
/// is indistinguishable from the source, 33% still reads in whole phrases, 44%
/// has no legible word left. The cliff between 34 and 44 is what makes a bar
/// possible; the exact place inside that gap is not measured.
pub const SETTLE_THRESHOLD: usize = 2;

/// Share of damaged stroke pixels above which the text is taken as unreadable.
pub const BAR: f64 = 0.35;

/// A streaming Y4M reader.
///
/// Streaming rather than loading: a 300-frame 1080p sequence is 889 MB, and two
/// of them have to be open at once.
struct Reader {
    inner: BufReader<File>,
    width: usize,
    height: usize,
    /// Frames per second, from the `F` tag. Rounded to whole frames.
    ///
    /// Read because a settling time in milliseconds is a frame count divided by
    /// a rate, and the rate has to come from the frames themselves. It used to
    /// be a hard-coded thirty sitting next to a `--fps` the operator was free to
    /// change, which is a trap rather than a bug only because nothing on disk
    /// was ever exported at another rate.
    fps: f64,
    frame: Vec<u8>,
    /// Bytes of one frame body: luma plus both chroma planes.
    body: usize,
}

impl Reader {
    fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path)
            .map_err(|e| format!("не открыть {}: {e}", path.display()))?;
        let mut inner = BufReader::new(file);
        let header = read_line(&mut inner, path)?;
        if !header.starts_with("YUV4MPEG2") {
            return Err(format!("{} — это не Y4M", path.display()));
        }

        let mut width = 0usize;
        let mut height = 0usize;
        let mut fps = 0.0f64;
        for tag in header.split_ascii_whitespace().skip(1) {
            let (kind, value) = tag.split_at(1);
            match kind {
                "W" => width = value.parse().map_err(|_| format!("ширина «{value}» не число"))?,
                "H" => height = value.parse().map_err(|_| format!("высота «{value}» не число"))?,
                // Y4M states the rate as a ratio, `F30:1`. A denominator of one
                // is what this harness writes, but ffmpeg is free to hand back
                // `F30000:1001`, so it is divided rather than assumed.
                "F" => {
                    let (num, den) = value
                        .split_once(':')
                        .ok_or(format!("частота «{value}» не вида 30:1"))?;
                    let num: f64 = num.parse().map_err(|_| format!("числитель «{num}» не число"))?;
                    let den: f64 = den.parse().map_err(|_| format!("знаменатель «{den}» не число"))?;
                    if den <= 0.0 {
                        return Err(format!("частота «{value}» с нулевым знаменателем"));
                    }
                    fps = num / den;
                }
                _ => {}
            }
        }
        if width == 0 || height == 0 {
            return Err(format!("{} не сообщает размер кадра", path.display()));
        }
        if fps <= 0.0 {
            return Err(format!(
                "{} не сообщает частоту кадров (тег F).\n\
                 Без неё кадры не перевести в миллисекунды, а перевести их по\n\
                 умолчанию в тридцать — это напечатать неверное время как верное.",
                path.display()
            ));
        }

        // Chroma is read and thrown away rather than skipped by seeking: the
        // point is to advance the stream, and a seek would stop this working
        // the day the input arrives on a pipe.
        let body = width * height + 2 * (width / 2) * (height / 2);
        Ok(Self { inner, width, height, fps, frame: vec![0; body], body })
    }

    /// Read one frame and hand back its luma plane, or `None` at end of file.
    fn next_luma(&mut self) -> Result<Option<&[u8]>, String> {
        let mut first = [0u8; 1];
        match self.inner.read(&mut first) {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(e) => return Err(format!("чтение не удалось: {e}")),
        }
        if first[0] != b'F' {
            return Err("кадр не начинается с FRAME — файл повреждён".to_owned());
        }
        // The rest of "FRAME" plus any per-frame parameters, up to the newline.
        let mut byte = [0u8; 1];
        loop {
            self.inner.read_exact(&mut byte).map_err(|e| format!("обрыв в заголовке кадра: {e}"))?;
            if byte[0] == b'\n' {
                break;
            }
        }
        self.inner
            .read_exact(&mut self.frame[..self.body])
            .map_err(|e| format!("обрыв внутри кадра: {e}"))?;
        Ok(Some(&self.frame[..self.width * self.height]))
    }
}

fn read_line(inner: &mut BufReader<File>, path: &Path) -> Result<String, String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    while out.len() < 256 {
        inner
            .read_exact(&mut byte)
            .map_err(|e| format!("не прочитать заголовок {}: {e}", path.display()))?;
        if byte[0] == b'\n' {
            return String::from_utf8(out).map_err(|_| "заголовок не текст".to_owned());
        }
        out.push(byte[0]);
    }
    Err(format!("заголовок {} подозрительно длинный", path.display()))
}

/// What one comparison found.
#[derive(Debug)]
pub struct Report {
    pub frames: u64,
    pub width: usize,
    pub height: usize,
    /// Share of source pixels that are edges. Constant-ish per content, printed
    /// because a measure over 2% of the frame deserves saying so out loud.
    pub edge_share: f64,
    /// Per threshold: the share of edge pixels damaged, at p50 and p95 across
    /// frames.
    pub edge_p50: [f64; DAMAGE.len()],
    pub edge_p95: [f64; DAMAGE.len()],
    /// The same at the middle threshold but over every pixel, edge or not. Kept
    /// as the control: if this moves and the edge figure does not, the damage
    /// is somewhere that does not carry text.
    pub all_p95: f64,
    /// Mean absolute luma error, for the same reason.
    pub mae: f64,
    /// The rate both files declared, carried so nothing downstream has to guess.
    pub fps: f64,
    /// Whether the pair was long enough for the alignment probe to run.
    ///
    /// A short file is not checked, and a number nobody checked should not read
    /// like one that was.
    pub aligned: bool,
    /// No pixel moved further than the finest threshold, in any frame.
    ///
    /// Not a codec result. A stream that damaged nothing at all is a copy of its
    /// own source — the same file passed twice, or a decode written over the
    /// input. Reported rather than refused here so the measurement stays a
    /// measurement; the caller that publishes numbers is the one that must
    /// refuse it.
    pub untouched: bool,
    /// The middle threshold's share, one entry per frame, **in frame order**.
    ///
    /// Percentiles answer "how bad overall" and cannot answer "how long until
    /// it comes back", which is the question the `settle` scenario exists to
    /// ask. A curve is needed for that, and a sorted one is not a curve.
    pub series: Vec<f64>,
}

/// How many source frames the alignment probe scores.
///
/// Twelve is enough for the answer to be unambiguous on real content and small
/// enough that buffering them costs about fifty megabytes at 1080p.
const ALIGN_PROBE: usize = 12;

/// How far either way the probe looks for a better fit.
///
/// Two frames. A comparison off by more than that is off by a keyframe interval
/// or a whole scenario, and the damage figure will be obviously wrong rather
/// than quietly flattering.
const ALIGN_RANGE: i32 = 2;

/// What one pair of frames contributed.
struct FrameScore {
    edges: u64,
    damaged: [u64; DAMAGE.len()],
    damaged_all: u64,
    error_sum: u64,
}

/// Score one source frame against one test frame.
///
/// Pulled out of the main loop because the alignment probe has to run exactly
/// the same arithmetic at four other offsets. Two implementations of "how
/// damaged is this frame" would let the probe bless an offset the measurement
/// then disagrees with.
fn score_frame(src: &[u8], dst: &[u8], w: usize, h: usize) -> FrameScore {
    let mut s = FrameScore { edges: 0, damaged: [0; DAMAGE.len()], damaged_all: 0, error_sum: 0 };
    for row in 0..h - 1 {
        let base = row * w;
        for col in 0..w - 1 {
            let i = base + col;
            let here = i32::from(src[i]);
            let gx = (i32::from(src[i + 1]) - here).unsigned_abs();
            let gy = (i32::from(src[i + w]) - here).unsigned_abs();
            let err = (i32::from(dst[i]) - here).unsigned_abs();
            s.error_sum += u64::from(err);

            if err > u32::from(DAMAGE[1]) {
                s.damaged_all += 1;
            }
            if gx + gy >= u32::from(EDGE_MIN) {
                s.edges += 1;
                for (k, &t) in DAMAGE.iter().enumerate() {
                    if err > u32::from(t) {
                        s.damaged[k] += 1;
                    }
                }
            }
        }
    }
    s
}

/// Which shift makes the two sequences agree best, and by how much.
///
/// The one defect in this harness whose symptom is a score BETTER than the
/// truth. A comparison pair off by a frame or two — a stale decode, an export
/// and an encode covering different stretches, a container that dropped one —
/// does not produce an absurd number. It produced 22.94% damage and «текст
/// собрался через 0 кадр(ов)», which is better than every real row in the
/// settle report. It would have won the ladder.
///
/// So the answer is not "is this plausible" but "does any other shift fit
/// better". If one does, the pair is not what it claims to be, whatever the
/// numbers look like.
///
/// Returns `(best_offset, scores)` where index `k` of `scores` is offset
/// `k as i32 - ALIGN_RANGE`.
fn alignment(src: &[Vec<u8>], test: &[Vec<u8>], w: usize, h: usize) -> (i32, Vec<f64>) {
    let span = (ALIGN_RANGE * 2 + 1) as usize;
    let mut scores = vec![f64::INFINITY; span];

    for (k, score) in scores.iter_mut().enumerate() {
        let d = k as i32 - ALIGN_RANGE;
        let mut edges = 0u64;
        let mut damaged = 0u64;
        let mut used = 0usize;

        for i in 0..ALIGN_PROBE {
            let si = i + ALIGN_RANGE as usize;
            let ti = si as i32 + d;
            if si >= src.len() || ti < 0 || ti as usize >= test.len() {
                continue;
            }
            let s = score_frame(&src[si], &test[ti as usize], w, h);
            edges += s.edges;
            damaged += s.damaged[SETTLE_THRESHOLD];
            used += 1;
        }
        if used > 0 && edges > 0 {
            *score = damaged as f64 / edges as f64;
        }
    }

    // Zero is the null hypothesis and it only loses to clear evidence.
    //
    // The first version simply took the minimum, and a tie made it accuse: on
    // content where every offset scored exactly 50% it named −2 and refused a
    // perfectly good pair. A probe that cannot tell the offsets apart has found
    // nothing, and "found nothing" must read as aligned. So an alternative has
    // to be better by a fifth AND by two whole points before it counts —
    // comfortably below what a real off-by-one does (44% against 65% on
    // scrolled text, 4% against 60% on a settled screen) and comfortably above
    // noise.
    let zero = scores[ALIGN_RANGE as usize];
    if !zero.is_finite() {
        return (0, scores);
    }
    let mut best = 0;
    let mut best_score = zero;
    for (k, &v) in scores.iter().enumerate() {
        let d = k as i32 - ALIGN_RANGE;
        if d == 0 || !v.is_finite() {
            continue;
        }
        if v < best_score && v < zero * 0.8 && v + 0.02 < zero {
            best = d;
            best_score = v;
        }
    }

    (best, scores)
}

/// Compare two Y4M sequences frame by frame.
pub fn compare(src: &Path, test: &Path) -> Result<Report, String> {
    let mut a = Reader::open(src)?;
    let mut b = Reader::open(test)?;
    if a.width != b.width || a.height != b.height {
        return Err(format!(
            "{}×{} против {}×{} — разные размеры не сравнить",
            a.width, a.height, b.width, b.height
        ));
    }
    // A rate mismatch means the two files are not the same recording, however
    // well their frame counts happen to line up. Refused rather than resolved in
    // favour of one of them: picking either would put a plausible millisecond
    // column under a comparison that is already meaningless.
    if (a.fps - b.fps).abs() > 1e-6 {
        return Err(format!(
            "{:.3} к/с против {:.3} к/с — это записи разной частоты,\n\
             сравнивать их покадрово нечего",
            a.fps, b.fps
        ));
    }
    let (w, h) = (a.width, a.height);
    let fps = a.fps;

    let mut edge_series: Vec<Vec<f64>> = vec![Vec::new(); DAMAGE.len()];
    let mut all_series: Vec<f64> = Vec::new();
    let mut edge_total = 0u64;
    let mut pixel_total = 0u64;
    let mut error_total = 0u64;
    let mut frames = 0u64;
    let mut untouched = true;

    // The head of both files, held in memory so the alignment probe can look at
    // the same frames from several offsets. Everything after this streams.
    let head = ALIGN_PROBE + 2 * ALIGN_RANGE as usize;
    let mut head_src: Vec<Vec<u8>> = Vec::with_capacity(head);
    let mut head_test: Vec<Vec<u8>> = Vec::with_capacity(head);
    while head_src.len() < head {
        let Some(luma) = a.next_luma()? else { break };
        let s = luma.to_vec();
        let Some(luma) = b.next_luma()? else {
            return Err(format!(
                "во втором файле кадров меньше: кончились на {}",
                head_src.len()
            ));
        };
        head_test.push(luma.to_vec());
        head_src.push(s);
    }

    if head_src.is_empty() {
        return Err("в исходнике нет ни одного кадра".to_owned());
    }

    // Only worth asking when there is enough material for the answer to mean
    // something. A three-frame file cannot tell a shift from a bad codec.
    let mut align_note = None;
    if head_src.len() >= ALIGN_PROBE + ALIGN_RANGE as usize {
        let (best, scores) = alignment(&head_src, &head_test, w, h);
        if best != 0 {
            let table: Vec<String> = scores
                .iter()
                .enumerate()
                .map(|(k, v)| {
                    let d = k as i32 - ALIGN_RANGE;
                    if v.is_finite() {
                        format!("{d:+}: {:.1}%", v * 100.0)
                    } else {
                        format!("{d:+}: —")
                    }
                })
                .collect();
            return Err(format!(
                "кадры не совпадают: при сдвиге {best:+} картинка сходится ЛУЧШЕ,\n\
                 чем при нулевом. Значит это не тот декодированный поток, не тот\n\
                 исходник, или один из них потерял кадр.\n\
                 Порча по первым {ALIGN_PROBE} кадрам: {}\n\
                 Считать по такой паре нельзя: смещённое сравнение даёт не абсурд,\n\
                 а правдоподобное число, и оно оказывается ЛУЧШЕ настоящих.",
                table.join(", ")
            ));
        }
        let _ = scores;
        align_note = Some(best);
    }

    let counted = ((h - 1) * (w - 1)) as u64;
    let mut take = |s: FrameScore,
                    edge_series: &mut Vec<Vec<f64>>,
                    all_series: &mut Vec<f64>| {
        pixel_total += counted;
        edge_total += s.edges;
        error_total += s.error_sum;
        if s.damaged[0] > 0 {
            untouched = false;
        }
        for k in 0..DAMAGE.len() {
            // A frame with no edges at all — a blank screen — contributes
            // nothing rather than a zero, which would be read as "undamaged".
            if s.edges > 0 {
                edge_series[k].push(s.damaged[k] as f64 / s.edges as f64);
            }
        }
        all_series.push(s.damaged_all as f64 / counted as f64);
        frames += 1;
    };

    for i in 0..head_src.len() {
        let s = score_frame(&head_src[i], &head_test[i], w, h);
        take(s, &mut edge_series, &mut all_series);
    }
    drop(head_src);
    drop(head_test);

    loop {
        // Both sides are pulled before either is used: `next_luma` borrows its
        // reader mutably, so the two slices cannot be alive at once. Copying the
        // source frame out is the price of streaming both.
        let Some(luma_a) = a.next_luma()? else { break };
        let src_frame = luma_a.to_vec();
        let Some(dst_frame) = b.next_luma()? else {
            return Err(format!("во втором файле кадров меньше: кончились на {frames}"));
        };
        let s = score_frame(&src_frame, dst_frame, w, h);
        take(s, &mut edge_series, &mut all_series);
    }

    if b.next_luma()?.is_some() {
        return Err("во втором файле кадров больше, чем в исходнике".to_owned());
    }
    let aligned = align_note.is_some();

    // Taken before the percentiles, which sort in place: the curve is only a
    // curve while it is still in frame order.
    let series = edge_series[SETTLE_THRESHOLD].clone();

    let mut edge_p50 = [0.0; DAMAGE.len()];
    let mut edge_p95 = [0.0; DAMAGE.len()];
    for k in 0..DAMAGE.len() {
        edge_p50[k] = percentile(&mut edge_series[k], 0.50);
        edge_p95[k] = percentile(&mut edge_series[k], 0.95);
    }

    Ok(Report {
        frames,
        width: w,
        height: h,
        edge_share: edge_total as f64 / pixel_total as f64,
        edge_p50,
        edge_p95,
        all_p95: percentile(&mut all_series, 0.95),
        mae: error_total as f64 / pixel_total as f64,
        fps,
        aligned,
        untouched,
        series,
    })
}

/// How long the text takes to come back after motion stops.
///
/// `cycle` and `scroll` describe the [`Scenario::Settle`] rhythm the sequence
/// was produced with: each cycle scrolls for `scroll` frames and then holds
/// still. For every hold this counts the frames from the moment motion ended
/// until the damage first drops below [`BAR`].
///
/// A hold that never gets there is reported as `None` rather than as the length
/// of the hold, because "did not settle in 40 frames" and "settled on frame 40"
/// are different findings and averaging them together would hide the first
/// inside the second.
///
/// [`Scenario::Settle`]: spike_capture::image::Scenario::Settle
pub fn settle_times(series: &[f64], cycle: u64, scroll: u64) -> Vec<Option<u64>> {
    let mut out = Vec::new();
    if cycle == 0 || scroll >= cycle {
        return out;
    }
    let mut start = scroll;
    while (start as usize) < series.len() {
        let end = (start + (cycle - scroll)).min(series.len() as u64);
        let mut found = None;
        for (waited, i) in (start..end).enumerate() {
            if series[i as usize] < BAR {
                found = Some(waited as u64);
                break;
            }
        }
        // Only complete holds are reported: a run that stopped in the middle of
        // one would otherwise contribute a "never settled" that is really just
        // the end of the file.
        if end - start == cycle - scroll {
            out.push(found);
        }
        start += cycle;
    }
    out
}

/// Nearest-rank, the same rule the rest of the harness reports percentiles by.
///
/// Sorts in place; the caller owns the vector and does not need it ordered.
fn percentile(values: &mut [f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (q * values.len() as f64).ceil().max(1.0) as usize;
    values[rank.min(values.len()) - 1]
}

impl Report {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "\nКадров сравнено: {}, {}×{}\n",
            self.frames, self.width, self.height
        ));
        out.push_str(&format!(
            "  краевых пикселей в исходнике {:.1}% — только по ним и считается\n",
            self.edge_share * 100.0
        ));
        out.push_str(if self.aligned {
            "  выравнивание проверено: сдвиг на кадр в обе стороны сходится хуже\n\n"
        } else {
            "  ⚠ выравнивание НЕ проверено: кадров слишком мало для пробы\n\n"
        });
        out.push_str("  порог   испорчено краевых, %\n");
        out.push_str("  luma      p50     p95\n");
        for (k, &t) in DAMAGE.iter().enumerate() {
            out.push_str(&format!(
                "  >{t:<3}    {:6.2}  {:6.2}\n",
                self.edge_p50[k] * 100.0,
                self.edge_p95[k] * 100.0
            ));
        }
        out.push_str(&format!(
            "\n  для контроля: по всем пикселям при >{} — p95 {:.2}%, средняя ошибка {:.2}\n",
            DAMAGE[1],
            self.all_p95 * 100.0,
            self.mae
        ));
        out
    }

    /// The settle report, for a sequence produced by the `settle` scenario.
    ///
    /// Refuses to guess: if the caller did not say what rhythm the frames were
    /// made with, there is no way to know where a hold begins, and a curve read
    /// against the wrong cycle would produce confident nonsense.
    pub fn render_settle(&self, cycle: u64, scroll: u64) -> String {
        // The rate comes off `self`, which got it from the files. There is
        // deliberately no parameter and no default: a caller able to supply the
        // rate is a caller able to supply the wrong one, and the wrong one here
        // prints a confident millisecond column nobody can tell is off.
        let fps = self.fps;
        let ms = |frames: u64| frames as f64 * 1000.0 / fps;

        let times = settle_times(&self.series, cycle, scroll);
        let mut out = format!(
            "\n  Оседание после остановки, порог {}% испорченных штрихов, {fps:.0} к/с:\n",
            (BAR * 100.0) as u32
        );
        if times.is_empty() {
            out.push_str("  ни одной полной остановки в этой записи\n");
            return out;
        }

        let settled: Vec<u64> = times.iter().filter_map(|t| *t).collect();
        let never = times.len() - settled.len();
        for (i, t) in times.iter().enumerate() {
            match t {
                Some(f) => out.push_str(&format!(
                    "    остановка {}: текст собрался через {f} кадр(ов), {:.0} мс\n",
                    i + 1,
                    ms(*f)
                )),
                None => out.push_str(&format!(
                    "    остановка {}: НЕ СОБРАЛСЯ за {} кадров\n",
                    i + 1,
                    cycle - scroll
                )),
            }
        }
        if !settled.is_empty() {
            let worst = settled.iter().copied().max().unwrap_or(0);
            out.push_str(&format!(
                "\n  худшее из собравшихся: {worst} кадр(ов), {:.0} мс\n",
                ms(worst)
            ));
        }
        if never > 0 {
            out.push_str(&format!("  не собралось вовсе: {never} из {}\n", times.len()));
        }
        out
    }

    /// Write the per-frame curve, one line per frame.
    pub fn write_series(&self, path: &Path) -> Result<(), String> {
        use std::io::Write;
        let mut f = File::create(path)
            .map_err(|e| format!("не создать {}: {e}", path.display()))?;
        writeln!(f, "frame,damaged_share").map_err(|e| format!("запись: {e}"))?;
        for (i, v) in self.series.iter().enumerate() {
            writeln!(f, "{i},{v:.6}").map_err(|e| format!("запись: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a Y4M holding one frame with the given luma; chroma is flat.
    fn write(path: &Path, w: usize, h: usize, luma: &[u8]) {
        write_at(path, w, h, "30:1", luma);
    }

    fn write_at(path: &Path, w: usize, h: usize, rate: &str, luma: &[u8]) {
        let mut f = File::create(path).unwrap();
        write!(f, "YUV4MPEG2 W{w} H{h} F{rate} Ip A1:1 C420jpeg\n").unwrap();
        f.write_all(b"FRAME\n").unwrap();
        f.write_all(luma).unwrap();
        f.write_all(&vec![128u8; 2 * (w / 2) * (h / 2)]).unwrap();
    }

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join("spike-quality-test");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A frame with a hard vertical line down the middle: the line is the only
    /// edge, everything else is flat.
    fn striped(w: usize, h: usize) -> Vec<u8> {
        let mut v = vec![235u8; w * h];
        for row in 0..h {
            v[row * w + w / 2] = 16;
        }
        v
    }

    #[test]
    fn an_identical_copy_is_undamaged() {
        let d = dir();
        let (a, b) = (d.join("id-a.y4m"), d.join("id-b.y4m"));
        let luma = striped(16, 8);
        write(&a, 16, 8, &luma);
        write(&b, 16, 8, &luma);

        let r = compare(&a, &b).unwrap();
        assert_eq!(r.frames, 1);
        assert!(r.edge_share > 0.0, "полоса должна дать края");
        for k in 0..DAMAGE.len() {
            assert_eq!(r.edge_p95[k], 0.0, "порог {}", DAMAGE[k]);
        }
        assert_eq!(r.mae, 0.0);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn damage_on_the_stroke_counts_and_damage_on_the_background_does_not() {
        let d = dir();
        let (a, on_edge, on_flat) = (d.join("q-a.y4m"), d.join("q-e.y4m"), d.join("q-f.y4m"));
        let (w, h) = (16usize, 8usize);
        let luma = striped(w, h);
        write(&a, w, h, &luma);

        // Smear the line itself.
        let mut hit = luma.clone();
        for row in 0..h {
            hit[row * w + w / 2] = 160;
        }
        write(&on_edge, w, h, &hit);

        // Move the same amount of luma by the same distance, but out in the
        // flat area where no glyph is.
        let mut miss = luma.clone();
        for row in 0..h {
            miss[row * w + 2] = 90;
        }
        write(&on_flat, w, h, &miss);

        let hurt = compare(&a, &on_edge).unwrap();
        let harmless = compare(&a, &on_flat).unwrap();

        // Exactly half, and the half is the point: a stroke has two edges, the
        // pixel where the luma drops and the one before it, and only one of the
        // two was moved here. Pinned rather than loosened, because "damaged
        // strokes read as half damaged" is a property of the measure that any
        // later threshold has to be chosen against.
        assert!(
            (hurt.edge_p95[3] - 0.5).abs() < 0.01,
            "смазанный штрих должен дать ровно половину краевых: {:?}",
            hurt.edge_p95
        );
        // The flat-area damage is what a mean would have charged equally. This
        // measure has to charge it at nearly nothing, or it is just SSIM again.
        assert!(
            harmless.edge_p95[3] < 0.2,
            "порча фона не должна считаться порчей текста: {:?}",
            harmless.edge_p95
        );
        // …and the control column must still see it, or the measure would be
        // blind rather than selective.
        assert!(harmless.all_p95 > 0.0, "контрольная колонка обязана её заметить");

        for p in [&a, &on_edge, &on_flat] {
            std::fs::remove_file(p).ok();
        }
    }

    /// The millisecond column has to follow the file, not a constant. At 15 fps
    /// a settling time is twice the milliseconds it is at 30, and the old
    /// hard-coded thirty would have printed the same number for both.
    #[test]
    fn milliseconds_come_from_the_rate_in_the_header() {
        let d = dir();
        let (w, h) = (16usize, 8usize);
        let luma = striped(w, h);

        let mut seen = Vec::new();
        for rate in ["30:1", "15:1"] {
            let (a, b) = (d.join("r-a.y4m"), d.join("r-b.y4m"));
            write_at(&a, w, h, rate, &luma);
            write_at(&b, w, h, rate, &luma);
            let r = compare(&a, &b).unwrap();
            // One undamaged frame settles immediately, so the interesting part
            // is the rate the report carries and prints.
            let text = r.render_settle(2, 1);
            seen.push((r.fps, text));
            std::fs::remove_file(&a).ok();
            std::fs::remove_file(&b).ok();
        }

        assert_eq!(seen[0].0, 30.0);
        assert_eq!(seen[1].0, 15.0);
        assert!(seen[0].1.contains("30 к/с"), "{}", seen[0].1);
        assert!(seen[1].1.contains("15 к/с"), "{}", seen[1].1);
    }

    /// The same settle, in frames, is twice as many milliseconds at half the
    /// rate. Built by hand rather than through `compare`, because a
    /// self-comparison settles in zero frames and zero milliseconds is the one
    /// value that cannot tell the two rates apart.
    #[test]
    fn the_same_frame_count_is_more_milliseconds_at_a_lower_rate() {
        let bad = 0.9;
        let good = 0.1;
        // Cycle of 10, four scrolling: the hold recovers on its third frame.
        let series = vec![bad, bad, bad, bad, bad, bad, good, good, good, good];

        let at = |fps: f64| Report {
            frames: series.len() as u64,
            width: 16,
            height: 8,
            edge_share: 0.5,
            edge_p50: [0.0; DAMAGE.len()],
            edge_p95: [0.0; DAMAGE.len()],
            all_p95: 0.0,
            mae: 0.0,
            fps,
            aligned: true,
            untouched: false,
            series: series.clone(),
        }
        .render_settle(10, 4);

        assert!(at(30.0).contains("через 2 кадр(ов), 67 мс"), "{}", at(30.0));
        assert!(at(15.0).contains("через 2 кадр(ов), 133 мс"), "{}", at(15.0));
    }

    #[test]
    fn a_file_with_no_rate_is_refused_rather_than_assumed_to_be_thirty() {
        let d = dir();
        let path = d.join("norate.y4m");
        let (w, h) = (16usize, 8usize);
        let mut f = File::create(&path).unwrap();
        write!(f, "YUV4MPEG2 W{w} H{h} Ip A1:1 C420jpeg\n").unwrap();
        f.write_all(b"FRAME\n").unwrap();
        f.write_all(&striped(w, h)).unwrap();
        f.write_all(&vec![128u8; 2 * (w / 2) * (h / 2)]).unwrap();
        drop(f);

        let err = compare(&path, &path).unwrap_err();
        assert!(err.contains("частоту кадров"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn two_recordings_at_different_rates_are_refused() {
        let d = dir();
        let (a, b) = (d.join("m-a.y4m"), d.join("m-b.y4m"));
        let (w, h) = (16usize, 8usize);
        let luma = striped(w, h);
        write_at(&a, w, h, "30:1", &luma);
        write_at(&b, w, h, "15:1", &luma);

        let err = compare(&a, &b).unwrap_err();
        assert!(err.contains("разной частоты"), "{err}");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    /// ffmpeg is free to hand back a ratio that is not over one.
    #[test]
    fn a_non_integer_rate_is_divided_rather_than_misread() {
        let d = dir();
        let (a, b) = (d.join("ntsc-a.y4m"), d.join("ntsc-b.y4m"));
        let (w, h) = (16usize, 8usize);
        let luma = striped(w, h);
        write_at(&a, w, h, "30000:1001", &luma);
        write_at(&b, w, h, "30000:1001", &luma);

        let r = compare(&a, &b).unwrap();
        assert!((r.fps - 29.97).abs() < 0.01, "{}", r.fps);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    /// Write a Y4M of several frames, luma supplied per frame.
    fn write_many(path: &Path, w: usize, h: usize, frames: &[Vec<u8>]) {
        let mut f = File::create(path).unwrap();
        write!(f, "YUV4MPEG2 W{w} H{h} F30:1 Ip A1:1 C420jpeg\n").unwrap();
        for luma in frames {
            f.write_all(b"FRAME\n").unwrap();
            f.write_all(luma).unwrap();
            f.write_all(&vec![128u8; 2 * (w / 2) * (h / 2)]).unwrap();
        }
    }

    /// A whole field of stripes that translates every frame — scrolled text, in
    /// miniature.
    ///
    /// Deliberately not a single moving line. That was the first version, and
    /// with it every offset scored exactly the same: one stripe damaged is one
    /// stripe damaged wherever it sits, so the probe had nothing to compare and
    /// the test was measuring a tie rather than a shift.
    fn moving(w: usize, h: usize, n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|k| {
                let mut v = vec![235u8; w * h];
                for row in 0..h {
                    for col in 0..w {
                        if (col + k * 7) % 5 == 0 {
                            v[row * w + col] = 16;
                        }
                    }
                }
                v
            })
            .collect()
    }

    /// The defect whose symptom is a score BETTER than the truth. A pair off by
    /// one frame does not look absurd; it looks like a good codec.
    #[test]
    fn a_pair_off_by_one_frame_is_refused_rather_than_scored() {
        let d = dir();
        let (a, b) = (d.join("sh-a.y4m"), d.join("sh-b.y4m"));
        let (w, h) = (64usize, 16usize);
        let seq = moving(w, h, 20);

        write_many(&a, w, h, &seq);
        // Same frames, shifted by one: frame i of the test is frame i+1 of the
        // source. Every picture is genuine; only the pairing is wrong.
        write_many(&b, w, h, &seq[1..].to_vec());

        let err = compare(&a, &b).unwrap_err();
        assert!(
            err.contains("не совпадают") || err.contains("кадров меньше"),
            "{err}"
        );
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    /// Same length, so nothing but the alignment probe can notice.
    #[test]
    fn a_shift_that_keeps_the_frame_count_is_still_caught() {
        let d = dir();
        let (a, b) = (d.join("sh2-a.y4m"), d.join("sh2-b.y4m"));
        let (w, h) = (64usize, 16usize);
        let seq = moving(w, h, 24);

        write_many(&a, w, h, &seq[0..20].to_vec());
        write_many(&b, w, h, &seq[1..21].to_vec());

        let err = compare(&a, &b).unwrap_err();
        assert!(err.contains("не совпадают"), "{err}");
        assert!(err.contains("сдвиге"), "должен назвать сдвиг: {err}");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    /// The guard must not fire on an honest comparison of a lossy stream.
    #[test]
    fn a_genuinely_damaged_but_aligned_pair_passes() {
        let d = dir();
        let (a, b) = (d.join("ok-a.y4m"), d.join("ok-b.y4m"));
        let (w, h) = (64usize, 16usize);
        let seq = moving(w, h, 20);
        // Blur every stripe: real damage, right pairing.
        let hurt: Vec<Vec<u8>> = seq
            .iter()
            .map(|f| f.iter().map(|&p| if p == 16 { 120 } else { p }).collect())
            .collect();

        write_many(&a, w, h, &seq);
        write_many(&b, w, h, &hurt);

        let r = compare(&a, &b).expect("выровненная пара обязана считаться");
        assert!(r.aligned, "проба должна была отработать");
        assert!(!r.untouched, "порча есть");
        assert!(r.edge_p50[SETTLE_THRESHOLD] > 0.0);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn an_untouched_pair_is_flagged_so_the_caller_can_refuse_it() {
        let d = dir();
        let (a, b) = (d.join("cp-a.y4m"), d.join("cp-b.y4m"));
        let (w, h) = (64usize, 16usize);
        let seq = moving(w, h, 20);
        write_many(&a, w, h, &seq);
        write_many(&b, w, h, &seq);

        let r = compare(&a, &b).unwrap();
        assert!(r.untouched, "копия обязана быть помечена");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn settle_time_is_counted_from_the_moment_motion_stopped() {
        // Cycle of 10: four frames scrolling, six holding.
        // Hold one recovers on the third held frame, hold two never does.
        let bad = 0.9;
        let good = 0.1;
        let series = vec![
            bad, bad, bad, bad, // scroll
            bad, bad, good, good, good, good, // hold: settles after 2
            bad, bad, bad, bad, // scroll
            bad, bad, bad, bad, bad, bad, // hold: never
        ];
        let t = settle_times(&series, 10, 4);
        assert_eq!(t, vec![Some(2), None]);
    }

    /// A hold cut short by the end of the file is not evidence of anything, and
    /// counting it as "never settled" would slander whatever produced it.
    #[test]
    fn an_incomplete_hold_is_dropped_rather_than_counted_as_a_failure() {
        let series = vec![0.9, 0.9, 0.9, 0.9, 0.9, 0.9, 0.9, 0.9];
        assert_eq!(settle_times(&series, 10, 4), Vec::<Option<u64>>::new());
    }

    #[test]
    fn a_shorter_second_file_is_an_error_rather_than_a_shorter_comparison() {
        let d = dir();
        let (a, b) = (d.join("len-a.y4m"), d.join("len-b.y4m"));
        let (w, h) = (16usize, 8usize);
        let luma = striped(w, h);

        let mut f = File::create(&a).unwrap();
        write!(f, "YUV4MPEG2 W{w} H{h} F30:1 Ip A1:1 C420jpeg\n").unwrap();
        for _ in 0..2 {
            f.write_all(b"FRAME\n").unwrap();
            f.write_all(&luma).unwrap();
            f.write_all(&vec![128u8; 2 * (w / 2) * (h / 2)]).unwrap();
        }
        drop(f);
        write(&b, w, h, &luma);

        let err = compare(&a, &b).unwrap_err();
        assert!(err.contains("меньше"), "{err}");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }
}
