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
    /// The middle threshold's share, one entry per frame, **in frame order**.
    ///
    /// Percentiles answer "how bad overall" and cannot answer "how long until
    /// it comes back", which is the question the `settle` scenario exists to
    /// ask. A curve is needed for that, and a sorted one is not a curve.
    pub series: Vec<f64>,
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

    loop {
        // Both sides are pulled before either is used: `next_luma` borrows its
        // reader mutably, so the two slices cannot be alive at once. Copying the
        // source frame out is the price of streaming both.
        let Some(luma_a) = a.next_luma()? else { break };
        let src_frame = luma_a.to_vec();
        let Some(dst_frame) = b.next_luma()? else {
            return Err(format!("во втором файле кадров меньше: кончились на {frames}"));
        };

        let mut edges = 0u64;
        let mut damaged = [0u64; DAMAGE.len()];
        let mut damaged_all = 0u64;

        for row in 0..h - 1 {
            let base = row * w;
            for col in 0..w - 1 {
                let i = base + col;
                let here = i32::from(src_frame[i]);
                let gx = (i32::from(src_frame[i + 1]) - here).unsigned_abs();
                let gy = (i32::from(src_frame[i + w]) - here).unsigned_abs();
                let err = (i32::from(dst_frame[i]) - here).unsigned_abs();
                error_total += u64::from(err);

                if err > u32::from(DAMAGE[1]) {
                    damaged_all += 1;
                }
                if gx + gy >= u32::from(EDGE_MIN) {
                    edges += 1;
                    for (k, &t) in DAMAGE.iter().enumerate() {
                        if err > u32::from(t) {
                            damaged[k] += 1;
                        }
                    }
                }
            }
        }

        let counted = ((h - 1) * (w - 1)) as u64;
        pixel_total += counted;
        edge_total += edges;
        for k in 0..DAMAGE.len() {
            // A frame with no edges at all — a blank screen — contributes
            // nothing rather than a zero, which would be read as "undamaged".
            if edges > 0 {
                edge_series[k].push(damaged[k] as f64 / edges as f64);
            }
        }
        all_series.push(damaged_all as f64 / counted as f64);
        frames += 1;
    }

    if frames == 0 {
        return Err("в исходнике нет ни одного кадра".to_owned());
    }
    if b.next_luma()?.is_some() {
        return Err("во втором файле кадров больше, чем в исходнике".to_owned());
    }

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
            "  краевых пикселей в исходнике {:.1}% — только по ним и считается\n\n",
            self.edge_share * 100.0
        ));
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
