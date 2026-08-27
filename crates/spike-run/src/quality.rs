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

/// A streaming Y4M reader.
///
/// Streaming rather than loading: a 300-frame 1080p sequence is 889 MB, and two
/// of them have to be open at once.
struct Reader {
    inner: BufReader<File>,
    width: usize,
    height: usize,
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
        for tag in header.split_ascii_whitespace().skip(1) {
            let (kind, value) = tag.split_at(1);
            match kind {
                "W" => width = value.parse().map_err(|_| format!("ширина «{value}» не число"))?,
                "H" => height = value.parse().map_err(|_| format!("высота «{value}» не число"))?,
                _ => {}
            }
        }
        if width == 0 || height == 0 {
            return Err(format!("{} не сообщает размер кадра", path.display()));
        }

        // Chroma is read and thrown away rather than skipped by seeking: the
        // point is to advance the stream, and a seek would stop this working
        // the day the input arrives on a pipe.
        let body = width * height + 2 * (width / 2) * (height / 2);
        Ok(Self { inner, width, height, frame: vec![0; body], body })
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
    let (w, h) = (a.width, a.height);

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
    })
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a Y4M holding one frame with the given luma; chroma is flat.
    fn write(path: &Path, w: usize, h: usize, luma: &[u8]) {
        let mut f = File::create(path).unwrap();
        write!(f, "YUV4MPEG2 W{w} H{h} F30:1 Ip A1:1 C420jpeg\n").unwrap();
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
