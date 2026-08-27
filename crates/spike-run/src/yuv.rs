//! Frame sequences, written out for an encoder that is not ours.
//!
//! The point is to settle a question this harness cannot answer alone: how the
//! hardware encoder on this machine compares to the software VP9 the project
//! inherited. Writing our own FFI to the hardware path is days of work before
//! the first number; handing the exact same frames to a tool that already
//! speaks to it is an afternoon.
//!
//! **Exact** is the whole load-bearing word. A comparison of our codec on our
//! frames against another codec on frames that merely look similar is not a
//! comparison of codecs. So the frames come out of the same [`I420`] the
//! encoder is handed, written from the same loop, and the writer accumulates a
//! fingerprint over every byte it emits — the same FNV-1a the screenshots use,
//! so the two can be recognised as the same kind of claim.
//!
//! Two layouts, because the receiving tool costs something either way:
//!
//! * **Y4M** carries width, height and rate in a text header, so an encoder
//!   cannot be pointed at it with the wrong geometry and quietly produce mush.
//!   That is worth the eleven bytes a frame it costs.
//! * **NV12** is raw and carries nothing, and exists for one reason: feeding
//!   Quick Sync through ffmpeg from Y4M makes ffmpeg repack the planes to NV12
//!   first, and a product would not pay that. Left in, that repacking lands in
//!   the very first hardware number as if the encoder had spent it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use spike_encode::convert::I420;

/// How the planes are laid out on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Header, then `FRAME\n` and three planes per frame.
    Y4m,
    /// Y plane, then U and V interleaved. No header, no framing.
    Nv12,
}

impl Layout {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "y4m" => Some(Self::Y4m),
            "nv12" => Some(Self::Nv12),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Y4m => "y4m",
            Self::Nv12 => "nv12",
        }
    }
}

/// Hash and write in one pass.
///
/// A free function taking the three fields it touches rather than a method on
/// the writer: the chroma buffer being interleaved lives on the writer too, and
/// a `&mut self` method would make emitting it require moving it out first.
fn emit(
    out: &mut BufWriter<File>,
    hash: &mut u64,
    total: &mut u64,
    bytes: &[u8],
) -> Result<(), String> {
    // FNV-1a, the same constants the screenshots are fingerprinted with, so the
    // two figures are recognisably the same kind of claim.
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    *total += bytes.len() as u64;
    out.write_all(bytes).map_err(|e| format!("запись не удалась: {e}"))
}

pub struct Writer {
    out: BufWriter<File>,
    layout: Layout,
    width: u32,
    height: u32,
    frames: u64,
    bytes: u64,
    hash: u64,
    /// Reused between frames so the interleave does not allocate per frame.
    chroma: Vec<u8>,
}

impl Writer {
    /// `provenance` goes into the header as an `XSPIKE` parameter.
    ///
    /// Y4M lets a writer add its own `X`-prefixed parameters and every reader
    /// is required to ignore the ones it does not know, so this survives being
    /// handed to ffmpeg. It exists because neither exported format recorded
    /// which run produced it: two files that differ only in `--step` or
    /// `--motion` were indistinguishable once written, and the whole comparison
    /// method rests on knowing that two files hold the same frames.
    ///
    /// Must contain no whitespace — the header is space-separated.
    pub fn create(
        path: &Path,
        layout: Layout,
        width: u32,
        height: u32,
        fps: u32,
        provenance: &str,
    ) -> Result<Self, String> {
        let file = File::create(path)
            .map_err(|e| format!("не создать {}: {e}", path.display()))?;
        let mut w = Self {
            out: BufWriter::new(file),
            layout,
            width,
            height,
            frames: 0,
            bytes: 0,
            hash: 0xcbf2_9ce4_8422_2325,
            chroma: Vec::new(),
        };

        if layout == Layout::Y4m {
            // `Ip` is progressive, `A1:1` square pixels.
            //
            // `C420jpeg` rather than `C420mpeg2`, and the difference is not
            // cosmetic: the colour kernel averages a 2×2 block, so the chroma
            // sample sits at the centre of that block, which is JPEG siting.
            // It moves almost no bytes, but there is nothing to be gained by
            // stating it wrongly.
            //
            // Nothing here declares a colour range, and that is deliberate:
            // the conversion writes studio-swing BT.601, and yuv420p without a
            // range tag is read as limited by every tool that matters. Saying
            // `pc` would shift everything by sixteen and the shift would be
            // charged to whichever encoder was on trial.
            let tag: String = provenance
                .chars()
                .map(|c| if c.is_whitespace() { '_' } else { c })
                .collect();
            let header =
                format!("YUV4MPEG2 W{width} H{height} F{fps}:1 Ip A1:1 C420jpeg XSPIKE{tag}\n");
            w.put(header.as_bytes())?;
        }
        Ok(w)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), String> {
        emit(&mut self.out, &mut self.hash, &mut self.bytes, bytes)
    }

    /// Append one frame.
    ///
    /// Rejects a frame whose size does not match the header, because a Y4M
    /// stream that changes geometry mid-file is not a thing any reader expects
    /// — and silently letting it through would produce a file that decodes to
    /// garbage a long way from here.
    pub fn frame(&mut self, p: &I420) -> Result<(), String> {
        if p.width != self.width || p.height != self.height {
            return Err(format!(
                "кадр {}×{} не совпадает с заголовком {}×{}",
                p.width, p.height, self.width, self.height
            ));
        }

        // Written field by field rather than through `self`, so the chroma
        // scratch can be filled and emitted in the same breath without either
        // cloning a two-megabyte plane per frame or moving it out and back.
        let Self { out, hash, bytes, chroma, .. } = self;

        match self.layout {
            Layout::Y4m => {
                emit(out, hash, bytes, b"FRAME\n")?;
                // Three writes and no arithmetic: I420 here is already tightly
                // packed planar 4:2:0, which is exactly the Y4M frame body.
                emit(out, hash, bytes, &p.y)?;
                emit(out, hash, bytes, &p.u)?;
                emit(out, hash, bytes, &p.v)?;
            }
            Layout::Nv12 => {
                emit(out, hash, bytes, &p.y)?;
                let n = p.u.len().min(p.v.len());
                chroma.clear();
                chroma.reserve(n * 2);
                for i in 0..n {
                    chroma.push(p.u[i]);
                    chroma.push(p.v[i]);
                }
                emit(out, hash, bytes, chroma)?;
            }
        }
        self.frames += 1;
        Ok(())
    }

    /// Flush and report what was written: frames, bytes, and the fingerprint
    /// over every byte of them.
    pub fn finish(mut self) -> Result<(u64, u64, String), String> {
        self.out.flush().map_err(|e| format!("не сбросить буфер: {e}"))?;
        // One syscall against the count this writer has been keeping. A short
        // write, a full disk or a truncated flush otherwise produces a file that
        // decodes to fewer frames than the report says it wrote — and the whole
        // comparison method rests on two files holding the same frames.
        let on_disk = self
            .out
            .get_ref()
            .metadata()
            .map_err(|e| format!("не проверить длину файла: {e}"))?
            .len();
        if on_disk != self.bytes {
            return Err(format!(
                "записано {} Б, а на диске {on_disk} Б — файл неполон",
                self.bytes
            ));
        }
        let short = format!("{:016x}", self.hash)[..8].to_owned();
        Ok((self.frames, self.bytes, short))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane(width: u32, height: u32) -> I420 {
        let mut p = I420::new(width, height, 1);
        // Something other than the initialised constant, so a writer that
        // emitted the wrong plane would still be caught by length alone but
        // also by content.
        for (i, b) in p.y.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        for (i, b) in p.u.iter_mut().enumerate() {
            *b = (i % 97) as u8;
        }
        for (i, b) in p.v.iter_mut().enumerate() {
            *b = (i % 89) as u8;
        }
        p
    }

    #[test]
    fn a_y4m_frame_is_the_header_then_the_planes_untouched() {
        let dir = std::env::temp_dir().join("spike-y4m-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.y4m");

        let p = plane(64, 32);
        let mut w = Writer::create(&path, Layout::Y4m, p.width, p.height, 30, "тест").unwrap();
        w.frame(&p).unwrap();
        let (frames, _, _) = w.finish().unwrap();
        assert_eq!(frames, 1);

        let got = std::fs::read(&path).unwrap();
        let head = b"YUV4MPEG2 W64 H32 F30:1 Ip A1:1 C420jpeg XSPIKE\xd1\x82\xd0\xb5\xd1\x81\xd1\x82\n";
        assert!(
            got.starts_with(head),
            "заголовок не тот: {}",
            String::from_utf8_lossy(&got[..head.len().min(got.len())])
        );

        let body = &got[head.len()..];
        assert!(body.starts_with(b"FRAME\n"));
        let payload = &body[b"FRAME\n".len()..];
        // Y, then U, then V, byte for byte and in that order.
        assert_eq!(&payload[..p.y.len()], &p.y[..]);
        assert_eq!(&payload[p.y.len()..p.y.len() + p.u.len()], &p.u[..]);
        assert_eq!(&payload[p.y.len() + p.u.len()..], &p.v[..]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn nv12_interleaves_the_chroma_and_writes_no_header() {
        let dir = std::env::temp_dir().join("spike-y4m-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.nv12");

        let p = plane(64, 32);
        let mut w = Writer::create(&path, Layout::Nv12, p.width, p.height, 30, "тест").unwrap();
        w.frame(&p).unwrap();
        w.finish().unwrap();

        let got = std::fs::read(&path).unwrap();
        assert_eq!(got.len(), p.y.len() + p.u.len() + p.v.len(), "лишние байты");
        assert_eq!(&got[..p.y.len()], &p.y[..]);
        // U and V alternate, starting with U — that is what NV12 means, and
        // getting it backwards swaps the colour channels rather than failing.
        for i in 0..p.u.len() {
            assert_eq!(got[p.y.len() + i * 2], p.u[i], "U на месте {i}");
            assert_eq!(got[p.y.len() + i * 2 + 1], p.v[i], "V на месте {i}");
        }
        std::fs::remove_file(&path).ok();
    }

    /// Whitespace in the tag would end the parameter early and ffmpeg would read
    /// the rest as parameters of its own.
    #[test]
    fn provenance_with_spaces_cannot_break_the_header() {
        let dir = std::env::temp_dir().join("spike-y4m-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prov.y4m");

        let p = plane(64, 32);
        let mut w =
            Writer::create(&path, Layout::Y4m, p.width, p.height, 30, "image:a b\tc").unwrap();
        w.frame(&p).unwrap();
        w.finish().unwrap();

        let got = std::fs::read(&path).unwrap();
        let line: Vec<u8> = got.iter().copied().take_while(|&b| b != b'\n').collect();
        let text = String::from_utf8(line).unwrap();
        assert!(text.ends_with("XSPIKEimage:a_b_c"), "{text}");
        // Nine parameters and no more: a stray space would make ten.
        assert_eq!(text.split(' ').count(), 8, "{text}");
        std::fs::remove_file(&path).ok();
    }

    /// Two runs differing only in the key that decides the content must not
    /// produce files that look the same.
    #[test]
    fn two_steps_leave_different_headers() {
        let dir = std::env::temp_dir().join("spike-y4m-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = plane(64, 32);

        let mut heads = Vec::new();
        for step in ["step40", "step80"] {
            let path = dir.join(format!("{step}.y4m"));
            let mut w = Writer::create(&path, Layout::Y4m, p.width, p.height, 30, step).unwrap();
            w.frame(&p).unwrap();
            w.finish().unwrap();
            let got = std::fs::read(&path).unwrap();
            heads.push(got.iter().copied().take_while(|&b| b != b'\n').collect::<Vec<u8>>());
            std::fs::remove_file(&path).ok();
        }
        assert_ne!(heads[0], heads[1]);
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_refused_rather_than_appended() {
        let dir = std::env::temp_dir().join("spike-y4m-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mismatch.y4m");

        let mut w = Writer::create(&path, Layout::Y4m, 64, 32, 30, "тест").unwrap();
        let other = plane(32, 16);
        let err = w.frame(&other).unwrap_err();
        assert!(err.contains("не совпадает"), "{err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_fingerprint_follows_the_bytes() {
        let dir = std::env::temp_dir().join("spike-y4m-test");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.y4m");
        let b = dir.join("b.y4m");

        let p = plane(64, 32);
        let mut w = Writer::create(&a, Layout::Y4m, p.width, p.height, 30, "тест").unwrap();
        w.frame(&p).unwrap();
        let (_, _, fa) = w.finish().unwrap();

        let mut w = Writer::create(&b, Layout::Y4m, p.width, p.height, 30, "тест").unwrap();
        w.frame(&p).unwrap();
        let (_, _, fb) = w.finish().unwrap();
        assert_eq!(fa, fb, "одни и те же кадры — один и тот же отпечаток");

        // One byte different anywhere must move it, or it cannot be used to
        // claim two files hold the same frames.
        let mut q = plane(64, 32);
        q.y[0] ^= 1;
        let mut w = Writer::create(&b, Layout::Y4m, q.width, q.height, 30, "тест").unwrap();
        w.frame(&q).unwrap();
        let (_, _, fq) = w.finish().unwrap();
        assert_ne!(fa, fq);

        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }
}
