//! A 24-bit BMP writer, for looking at frames instead of timing them.
//!
//! BMP because it is the only lossless format that costs no dependency: a
//! 54-byte header and rows of BGR, bottom-up, each padded to four bytes. PNG
//! would need deflate, and this harness is not taking a dependency to write a
//! diagnostic picture that nobody ships. Windows opens BMP without being asked.

use std::io::Write;
use std::path::Path;

/// Drop the alpha and the stride padding: BGRA rows as the capture hands them
/// over, BGR rows packed as a BMP wants them.
pub fn from_bgra(width: u32, height: u32, stride: usize, bgra: &[u8]) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let row = y * stride;
        for x in 0..w {
            let i = row + x * 4;
            out.extend_from_slice(&bgra[i..i + 3]);
        }
    }
    out
}

/// I420 back to BGR, inverting exactly the studio-swing BT.601 that
/// `spike_encode::convert` applied on the way in.
///
/// Inverting the same matrix matters: a picture converted back through the full
/// range would look washed out, and the washing out would be blamed on the
/// encoder.
pub fn from_i420(width: u32, height: u32, y: &[u8], u: &[u8], v: &[u8]) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let cw = w.div_ceil(2);
    let mut out = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        for col in 0..w {
            let ci = (row / 2) * cw + col / 2;
            let c = y[row * w + col] as i32 - 16;
            let d = *u.get(ci).unwrap_or(&128) as i32 - 128;
            let e = *v.get(ci).unwrap_or(&128) as i32 - 128;
            let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255) as u8;
            let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255) as u8;
            let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255) as u8;
            out.extend_from_slice(&[b, g, r]);
        }
    }
    out
}

/// Write packed top-down BGR rows as a bottom-up 24-bit BMP.
pub fn write(path: &Path, width: u32, height: u32, bgr: &[u8]) -> Result<(), String> {
    let (w, h) = (width as usize, height as usize);
    let expect = w * h * 3;
    if bgr.len() != expect {
        return Err(format!("для {width}×{height} нужно {expect} байт, а дано {}", bgr.len()));
    }
    // Every BMP row is padded to a multiple of four bytes. Not optional, and
    // the reason a picture written without it comes out sheared.
    let stride = (w * 3).div_ceil(4) * 4;
    let pad = vec![0u8; stride - w * 3];
    let size = 54 + stride * h;

    let mut f = std::fs::File::create(path)
        .map_err(|e| format!("не создать {}: {e}", path.display()))?;
    let mut head = Vec::with_capacity(54);
    head.extend_from_slice(b"BM");
    head.extend_from_slice(&(size as u32).to_le_bytes());
    head.extend_from_slice(&0u32.to_le_bytes());
    head.extend_from_slice(&54u32.to_le_bytes());
    head.extend_from_slice(&40u32.to_le_bytes());
    head.extend_from_slice(&(width as i32).to_le_bytes());
    head.extend_from_slice(&(height as i32).to_le_bytes());
    head.extend_from_slice(&1u16.to_le_bytes());
    head.extend_from_slice(&24u16.to_le_bytes());
    head.extend_from_slice(&0u32.to_le_bytes());
    head.extend_from_slice(&((stride * h) as u32).to_le_bytes());
    head.extend_from_slice(&2835i32.to_le_bytes());
    head.extend_from_slice(&2835i32.to_le_bytes());
    head.extend_from_slice(&0u32.to_le_bytes());
    head.extend_from_slice(&0u32.to_le_bytes());
    f.write_all(&head).map_err(|e| format!("не записать заголовок: {e}"))?;

    // Bottom-up: a positive height in the header means the last row comes first.
    for row in (0..h).rev() {
        let start = row * w * 3;
        f.write_all(&bgr[start..start + w * 3])
            .map_err(|e| format!("не записать строку: {e}"))?;
        if !pad.is_empty() {
            f.write_all(&pad).map_err(|e| format!("не записать выравнивание: {e}"))?;
        }
    }
    Ok(())
}
