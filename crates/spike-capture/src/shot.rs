//! A single real screenshot, stored in a file.
//!
//! Why a real frame rather than something drawn from a formula: the cost of
//! encoding a desktop is decided by the things a formula gets wrong. Real text
//! is a field of one-pixel edges with antialiasing, which is the expensive part;
//! real window chrome is large flat areas, which is the cheap part. The previous
//! synthetic source painted smooth gradients — neither of those — and was
//! therefore useless as a stand-in for the encoder's workload.
//!
//! Why a file rather than capturing afresh each run: a measurement is only
//! comparable to another measurement that saw the same pixels. Two machines
//! cannot scroll the same document the same way, and the same machine cannot do
//! it twice. One file carried between them removes content from the list of
//! variables.
//!
//! **The file is not committed.** It is a frame of somebody's real desktop and
//! this repository is public. See `.gitignore`.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

/// `SPIKEIMG` then version, width, height, all little-endian u32, then tightly
/// packed BGRA. No compression: 8 MB fits on anything, and a format nobody has
/// to install a library to read is worth more here than a small one.
const MAGIC: [u8; 8] = *b"SPIKEIMG";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 8 + 4 * 3;

/// Refuse anything absurd rather than trying to allocate it. 8K is past any
/// screen this is aimed at.
const MAX_SIDE: u32 = 8192;

pub struct Shot {
    pub width: u32,
    pub height: u32,
    /// BGRA, stride exactly `width * 4`.
    pub bgra: Vec<u8>,
}

impl Shot {
    /// Write a captured frame out, repacking away any padding the capture API
    /// left at the end of each row.
    pub fn save(
        path: &Path,
        width: u32,
        height: u32,
        stride: usize,
        bgra: &[u8],
    ) -> Result<(), String> {
        let row = width as usize * 4;
        if stride < row {
            return Err(format!("шаг строки {stride} меньше ширины кадра {row}"));
        }
        let needed = stride * height as usize;
        if bgra.len() < needed {
            return Err(format!("в буфере {} байт, нужно {needed}", bgra.len()));
        }

        let file = File::create(path).map_err(|e| format!("не создать {}: {e}", path.display()))?;
        let mut out = BufWriter::new(file);
        let write = |out: &mut BufWriter<File>, bytes: &[u8]| -> Result<(), String> {
            out.write_all(bytes).map_err(|e| format!("не записать {}: {e}", path.display()))
        };
        write(&mut out, &MAGIC)?;
        for value in [VERSION, width, height] {
            write(&mut out, &value.to_le_bytes())?;
        }
        for y in 0..height as usize {
            write(&mut out, &bgra[y * stride..y * stride + row])?;
        }
        out.flush().map_err(|e| format!("не дописать {}: {e}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let mut file =
            File::open(path).map_err(|e| format!("не открыть {}: {e}", path.display()))?;
        let mut header = [0u8; HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|_| format!("{} короче заголовка — это не снимок", path.display()))?;
        if header[..8] != MAGIC {
            return Err(format!(
                "{} не начинается с SPIKEIMG. Снимок делается ключом --grab, \
                 а не переименованием чего попало",
                path.display()
            ));
        }
        let word = |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
        let (version, width, height) = (word(8), word(12), word(16));
        if version != VERSION {
            return Err(format!("{}: версия формата {version}, ожидалась {VERSION}", path.display()));
        }
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err(format!("{}: размер {width}×{height} неправдоподобен", path.display()));
        }

        let expect = width as usize * 4 * height as usize;
        let mut bgra = Vec::with_capacity(expect);
        file.read_to_end(&mut bgra)
            .map_err(|e| format!("не дочитать {}: {e}", path.display()))?;
        if bgra.len() != expect {
            return Err(format!(
                "{}: в файле {} байт пикселей, для {width}×{height} нужно {expect}",
                path.display(),
                bgra.len()
            ));
        }
        Ok(Shot { width, height, bgra })
    }

    /// What this image is and how hard it is to encode.
    ///
    /// The hash exists so two runs cannot be compared by accident: different
    /// images produce different encoding costs, and the report has to say which
    /// one it saw rather than trusting whoever is on duty to remember.
    ///
    /// The other two are cheap proxies for the same thing, readable by a human:
    /// a desktop of flat panels and a desktop of dense small text are the two
    /// ends of the range, and they differ several-fold in what the encoder pays.
    pub fn fingerprint(&self) -> Fingerprint {
        // FNV-1a. Not a cryptographic hash and does not need to be: it guards
        // against mixing two files up, not against someone forging one.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &byte in &self.bgra {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }

        let (mut flat, mut edge_sum, mut counted) = (0u64, 0u64, 0u64);
        let row = self.width as usize * 4;
        for y in 0..self.height as usize {
            let base = y * row;
            for x in 1..self.width as usize {
                let i = base + x * 4;
                let j = i - 4;
                let px = &self.bgra[i..i + 4];
                let left = &self.bgra[j..j + 4];
                if px == left {
                    flat += 1;
                } else {
                    let d = (0..3)
                        .map(|c| u64::from(px[c].abs_diff(left[c])))
                        .sum::<u64>();
                    edge_sum += d / 3;
                }
                counted += 1;
            }
        }
        let counted = counted.max(1) as f64;
        Fingerprint {
            hash,
            flat_share: flat as f64 / counted,
            edge_mean: edge_sum as f64 / counted,
        }
    }
}

/// Hand-written rather than derived: a derived one would print eight megabytes
/// of pixels into a test failure message.
impl std::fmt::Debug for Shot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Shot({}×{}, {} байт)", self.width, self.height, self.bgra.len())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Fingerprint {
    pub hash: u64,
    /// Share of pixels identical to the one on their left. Flat panels and
    /// wallpaper push this up; dense text pushes it down.
    pub flat_share: f64,
    /// Mean brightness step between horizontal neighbours, over the pixels that
    /// are not flat. Text edges push this up.
    pub edge_mean: f64,
}

impl Fingerprint {
    /// Short form for the report header, where the point is only "was this the
    /// same file".
    pub fn short(&self) -> String {
        format!("{:016x}", self.hash)[..8].to_owned()
    }

    pub fn describe(&self) -> String {
        format!(
            "плоских пикселей {:.0}%, средний перепад на границе {:.1}",
            self.flat_share * 100.0,
            self.edge_mean
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("spike-shot-test-{name}.shot"))
    }

    /// Two rows of four pixels, written with a stride that has padding on the
    /// end — which is what desktop duplication actually hands over.
    fn padded() -> (u32, u32, usize, Vec<u8>) {
        let (w, h, stride) = (4u32, 2u32, 24usize);
        let mut buf = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = y * stride + x * 4;
                buf[i] = (x * 10) as u8;
                buf[i + 1] = (y * 20) as u8;
                buf[i + 2] = 7;
                buf[i + 3] = 0xFF;
            }
            // Padding, which must not survive into the file.
            for b in &mut buf[y * stride + w as usize * 4..(y + 1) * stride] {
                *b = 0xAB;
            }
        }
        (w, h, stride, buf)
    }

    #[test]
    fn a_saved_shot_loads_back_without_the_row_padding() {
        let path = scratch("roundtrip");
        let (w, h, stride, buf) = padded();
        Shot::save(&path, w, h, stride, &buf).expect("сохраняется");
        let shot = Shot::load(&path).expect("читается");
        assert_eq!((shot.width, shot.height), (w, h));
        assert_eq!(shot.bgra.len(), w as usize * 4 * h as usize);
        assert_eq!(&shot.bgra[0..4], &[0, 0, 7, 0xFF]);
        assert_eq!(&shot.bgra[4..8], &[10, 0, 7, 0xFF]);
        // Second row starts right after the first, padding gone.
        assert_eq!(&shot.bgra[16..20], &[0, 20, 7, 0xFF]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn something_that_is_not_a_shot_is_refused_by_name() {
        let path = scratch("garbage");
        std::fs::write(&path, "это не снимок, а просто файл подходящей длины".as_bytes()).unwrap();
        let err = Shot::load(&path).expect_err("должно быть отвергнуто");
        assert!(err.contains("SPIKEIMG"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_truncated_shot_is_refused_rather_than_padded() {
        let path = scratch("short");
        let (w, h, stride, buf) = padded();
        Shot::save(&path, w, h, stride, &buf).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&path, &bytes).unwrap();
        let err = Shot::load(&path).expect_err("обрезанный файл — не снимок");
        assert!(err.contains("нужно"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_flat_image_and_a_busy_one_fingerprint_differently() {
        let flat = Shot { width: 64, height: 64, bgra: vec![200; 64 * 64 * 4] };
        let mut busy = Shot { width: 64, height: 64, bgra: vec![0; 64 * 64 * 4] };
        for (i, b) in busy.bgra.iter_mut().enumerate() {
            // Alternating columns: every pixel differs from its left neighbour,
            // which is what a field of text looks like to this measure.
            *b = if (i / 4) % 2 == 0 { 20 } else { 235 };
        }
        let (f, b) = (flat.fingerprint(), busy.fingerprint());
        assert!(f.flat_share > 0.99, "{:?}", f);
        assert!(b.flat_share < 0.01, "{:?}", b);
        assert!(b.edge_mean > f.edge_mean);
        assert_ne!(f.hash, b.hash);
        assert_eq!(f.short().len(), 8);
    }
}
