//! The bitstream our own encoder produced, written where another tool can read it.
//!
//! [`crate::yuv`] settles one half of the codec question by handing our frames
//! to encoders we do not have. This settles the other half, and the two are not
//! interchangeable. Exporting frames puts *their* encoders on our content;
//! exporting the bitstream puts *our* encoder on their measuring stick. Until
//! both exist, every comparison is one of these two things pretending to be the
//! other.
//!
//! Concretely, the gap this closes: on a 300-frame run of the same screenshot,
//! ffmpeg's libvpx would not go below about 2,5 Mbit/s however it was asked,
//! while this harness holds 1,85 Mbit/s on that content at scale 1. So the VP9
//! row of the codec table was measured on a configuration worse than the one
//! the project actually runs, and the gap to the hardware encoders is smaller
//! than that table shows by an amount nobody could name. With the bitstream on
//! disk, ffmpeg decodes it and scores it against the exported source, and our
//! number lands on the same axis as the other three instead of beside it.
//!
//! IVF is the container, for the same reason Y4M was: it is a 32-byte file
//! header and a 12-byte header per frame, and both libvpx and ffmpeg read it.
//! Anything richer would mean a muxer, and a muxer is a dependency with
//! opinions about timestamps — the one thing a comparison must not have.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

/// Where the frame count sits in the file header, for the patch at the end.
const FRAME_COUNT_OFFSET: u64 = 24;

const HEADER_BYTES: u16 = 32;

#[derive(Debug)]
pub struct Writer {
    out: BufWriter<File>,
    frames: u64,
    /// Compressed bytes only — the container's own headers are not counted,
    /// because the figure this is compared against is a bitrate.
    payload: u64,
    hash: u64,
}

impl Writer {
    /// Open a file and write the IVF header.
    ///
    /// `fourcc` is the codec's four bytes: `VP80` or `VP90`. The caller passes
    /// it rather than this module mapping a codec enum, so that nothing here
    /// has to exist behind the `vpx` feature gate.
    pub fn create(
        path: &Path,
        fourcc: [u8; 4],
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, String> {
        let (w16, h16) = (u16::try_from(width), u16::try_from(height));
        let (Ok(w16), Ok(h16)) = (w16, h16) else {
            // IVF stores geometry in sixteen bits. Worth a refusal rather than
            // a truncation: a wrapped width produces a file that decodes into
            // convincing-looking rubbish.
            return Err(format!("{width}×{height} не влезает в заголовок IVF"));
        };

        let file =
            File::create(path).map_err(|e| format!("не создать {}: {e}", path.display()))?;
        let mut header = [0u8; HEADER_BYTES as usize];
        header[0..4].copy_from_slice(b"DKIF");
        header[4..6].copy_from_slice(&0u16.to_le_bytes()); // version
        header[6..8].copy_from_slice(&HEADER_BYTES.to_le_bytes());
        header[8..12].copy_from_slice(&fourcc);
        header[12..14].copy_from_slice(&w16.to_le_bytes());
        header[14..16].copy_from_slice(&h16.to_le_bytes());
        // The timebase, denominator first — that is the order IVF uses, and
        // getting it backwards makes a 30 fps file play at one frame every
        // thirty seconds. It is the encoder's own `g_timebase` of 1/fps, so a
        // timestamp below is a frame number and nothing has to be scaled.
        header[16..20].copy_from_slice(&fps.to_le_bytes());
        header[20..24].copy_from_slice(&1u32.to_le_bytes());
        // Frame count, patched by `finish`. Zero until then, so a file left
        // behind by a run that died mid-way is recognisably unfinished.
        header[24..28].copy_from_slice(&0u32.to_le_bytes());

        let mut out = BufWriter::new(file);
        out.write_all(&header).map_err(|e| format!("запись заголовка не удалась: {e}"))?;
        Ok(Self { out, frames: 0, payload: 0, hash: 0xcbf2_9ce4_8422_2325 })
    }

    /// Append one encoded frame.
    ///
    /// The timestamp is the frame's own index, which is what the encoder was
    /// handed as its presentation time. Keeping the two the same is what lets a
    /// decoder's frame count be compared with ours at all.
    pub fn frame(&mut self, payload: &[u8]) -> Result<(), String> {
        let size = u32::try_from(payload.len())
            .map_err(|_| format!("кадр в {} Б не влезает в заголовок IVF", payload.len()))?;

        let mut head = [0u8; 12];
        head[0..4].copy_from_slice(&size.to_le_bytes());
        head[4..12].copy_from_slice(&self.frames.to_le_bytes());
        self.out.write_all(&head).map_err(|e| format!("запись не удалась: {e}"))?;
        self.out.write_all(payload).map_err(|e| format!("запись не удалась: {e}"))?;

        // FNV-1a over the compressed bytes alone, with the constants the
        // screenshots and the Y4M export already use. Deliberately not over the
        // container: two runs that produced the same bitstream should be
        // recognised as the same encode even if the framing around it changed.
        for &b in payload {
            self.hash ^= u64::from(b);
            self.hash = self.hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.frames += 1;
        self.payload += payload.len() as u64;
        Ok(())
    }

    /// Patch the frame count into the header and close.
    ///
    /// Returns frames, compressed bytes and the fingerprint.
    pub fn finish(mut self) -> Result<(u64, u64, String), String> {
        let count = u32::try_from(self.frames)
            .map_err(|_| format!("{} кадров не влезает в заголовок IVF", self.frames))?;
        self.out
            .seek(SeekFrom::Start(FRAME_COUNT_OFFSET))
            .map_err(|e| format!("не перемотать к счётчику кадров: {e}"))?;
        self.out
            .write_all(&count.to_le_bytes())
            .map_err(|e| format!("не записать счётчик кадров: {e}"))?;
        self.out.flush().map_err(|e| format!("файл не дописан: {e}"))?;
        Ok((self.frames, self.payload, format!("{:016x}", self.hash)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join("spike-ivf-test");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_header_says_what_a_reader_needs_to_know() {
        let path = dir().join("head.ivf");
        let mut w = Writer::create(&path, *b"VP90", 1920, 1080, 30).unwrap();
        w.frame(&[1, 2, 3]).unwrap();
        w.finish().unwrap();

        let got = read(&path);
        assert_eq!(&got[0..4], b"DKIF");
        assert_eq!(u16::from_le_bytes([got[6], got[7]]), 32, "длина заголовка");
        assert_eq!(&got[8..12], b"VP90");
        assert_eq!(u16::from_le_bytes([got[12], got[13]]), 1920);
        assert_eq!(u16::from_le_bytes([got[14], got[15]]), 1080);
        // Denominator first: thirty, then one. Backwards here is a file that
        // claims one frame every thirty seconds, and every tool believes it.
        assert_eq!(u32::from_le_bytes(got[16..20].try_into().unwrap()), 30);
        assert_eq!(u32::from_le_bytes(got[20..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(got[24..28].try_into().unwrap()), 1, "кадров");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn each_frame_carries_its_length_and_its_own_number() {
        let path = dir().join("frames.ivf");
        let mut w = Writer::create(&path, *b"VP80", 64, 32, 25).unwrap();
        w.frame(&[9; 5]).unwrap();
        w.frame(&[7; 2]).unwrap();
        let (frames, payload, _) = w.finish().unwrap();
        assert_eq!((frames, payload), (2, 7));

        let got = read(&path);
        assert_eq!(u32::from_le_bytes(got[32..36].try_into().unwrap()), 5);
        assert_eq!(u64::from_le_bytes(got[36..44].try_into().unwrap()), 0, "первый кадр — pts 0");
        assert_eq!(&got[44..49], &[9; 5]);

        let second = 44 + 5;
        assert_eq!(u32::from_le_bytes(got[second..second + 4].try_into().unwrap()), 2);
        assert_eq!(
            u64::from_le_bytes(got[second + 4..second + 12].try_into().unwrap()),
            1,
            "второй кадр — pts 1"
        );
        assert_eq!(&got[second + 12..second + 14], &[7; 2]);
        assert_eq!(got.len(), second + 14, "ничего лишнего после последнего кадра");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_fingerprint_follows_the_compressed_bytes_and_not_the_framing() {
        let a = dir().join("a.ivf");
        let b = dir().join("b.ivf");

        let mut w = Writer::create(&a, *b"VP90", 64, 32, 30).unwrap();
        w.frame(&[1, 2, 3, 4]).unwrap();
        let (_, _, fa) = w.finish().unwrap();

        // Same payload, different geometry in the header: the same encode as
        // far as this fingerprint is concerned, which is the point of hashing
        // the bitstream rather than the file.
        let mut w = Writer::create(&b, *b"VP90", 32, 16, 15).unwrap();
        w.frame(&[1, 2, 3, 4]).unwrap();
        let (_, _, fb) = w.finish().unwrap();
        assert_eq!(fa, fb);

        // One bit of the bitstream, on the other hand, must move it.
        let mut w = Writer::create(&b, *b"VP90", 64, 32, 30).unwrap();
        w.frame(&[1, 2, 3, 5]).unwrap();
        let (_, _, fc) = w.finish().unwrap();
        assert_ne!(fa, fc);

        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn geometry_that_does_not_fit_the_header_is_refused_rather_than_wrapped() {
        let path = dir().join("huge.ivf");
        let err = Writer::create(&path, *b"VP90", 70_000, 1080, 30).unwrap_err();
        assert!(err.contains("не влезает"), "{err}");
        std::fs::remove_file(&path).ok();
    }
}
