//! `BitBlt` fallback.
//!
//! Slow, blind to change, and included anyway — because it works in places
//! desktop duplication does not, and a VM is very often one of them. Its job in
//! phase 0 is to keep the harness producing numbers when DDA refuses to open,
//! and to put a price on the fallback path we would ship for the same machines.
//!
//! It cannot report which pixels changed, so every frame is accounted as a full
//! repaint ([`Dirty::Unknown`]). That is not pessimism: an encoder fed by this
//! backend genuinely has no better information.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::{CaptureError, Dirty, Frame, FrameSource, Readback};

pub struct GdiSource {
    screen: HDC,
    mem: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    width: i32,
    height: i32,
    buf: Vec<u8>,
    next_due: Option<Instant>,
}

impl GdiSource {
    pub fn open() -> Result<Self, CaptureError> {
        // SAFETY: every handle created here is stored in the struct and released
        // in `Drop`. The screen DC is obtained for the whole desktop (HWND::None)
        // and released with ReleaseDC, not DeleteDC, as the API requires.
        unsafe {
            let width = GetSystemMetrics(SM_CXSCREEN);
            let height = GetSystemMetrics(SM_CYSCREEN);
            if width <= 0 || height <= 0 {
                return Err(CaptureError::Unavailable(
                    "система сообщает нулевой размер экрана — вероятно, нет сеанса с рабочим столом"
                        .to_owned(),
                ));
            }

            let screen = GetDC(None::<HWND>);
            if screen.is_invalid() {
                return Err(CaptureError::Unavailable("не выдан контекст экрана".to_owned()));
            }

            let mem = CreateCompatibleDC(Some(screen));
            if mem.is_invalid() {
                ReleaseDC(None::<HWND>, screen);
                return Err(CaptureError::Unavailable("не создан контекст в памяти".to_owned()));
            }

            let bitmap = CreateCompatibleBitmap(screen, width, height);
            if bitmap.is_invalid() {
                let _ = DeleteDC(mem);
                ReleaseDC(None::<HWND>, screen);
                return Err(CaptureError::Unavailable("не создан растр".to_owned()));
            }

            let previous = SelectObject(mem, HGDIOBJ(bitmap.0));

            Ok(Self {
                screen,
                mem,
                bitmap,
                previous,
                width,
                height,
                buf: vec![0u8; width as usize * height as usize * 4],
                next_due: None,
            })
        }
    }
}

impl Drop for GdiSource {
    fn drop(&mut self) {
        // SAFETY: each handle is released exactly once, in the reverse order of
        // creation, and the originally selected object is restored first.
        unsafe {
            SelectObject(self.mem, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.mem);
            ReleaseDC(None::<HWND>, self.screen);
        }
    }
}

impl FrameSource for GdiSource {
    fn describe(&self) -> String {
        format!(
            "GDI BitBlt, {}×{} — изменившиеся области недоступны, каждый кадр считается полным",
            self.width, self.height
        )
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
        readback: Readback,
    ) -> Result<Option<Frame<'_>>, CaptureError> {
        // GDI has no notion of "wait until something changes", so the pacing is
        // ours to do. Without it the runner spins at whatever rate BitBlt manages
        // and the CPU figure stops meaning anything.
        let now = Instant::now();
        let due = self.next_due.unwrap_or(now);
        let waited = due.checked_duration_since(now).unwrap_or(Duration::ZERO);
        if waited > Duration::ZERO {
            std::thread::sleep(waited.min(timeout));
        }
        self.next_due = Some(due + timeout);

        let work_start = Instant::now();
        // SAFETY: both DCs are live for the lifetime of `self`.
        unsafe {
            BitBlt(
                self.mem,
                0,
                0,
                self.width,
                self.height,
                Some(self.screen),
                0,
                0,
                SRCCOPY,
            )
            .map_err(|e| CaptureError::Failed(format!("BitBlt: {e}")))?;
        }
        let work_us = work_start.elapsed().as_micros() as u64;

        let readback_us = if readback.wants_pixels() {
            let started = Instant::now();
            let mut info = BITMAPINFO::default();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width,
                // Negative height asks for a top-down image. Without it GDI hands
                // back the rows bottom-up and every consumer sees the screen
                // upside down.
                biHeight: -self.height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };

            // SAFETY: `buf` holds width*height*4 bytes, which is exactly what a
            // 32-bit BI_RGB image of these dimensions occupies; `info` is live.
            let lines = unsafe {
                GetDIBits(
                    self.mem,
                    self.bitmap,
                    0,
                    self.height as u32,
                    Some(self.buf.as_mut_ptr().cast()),
                    &mut info,
                    DIB_RGB_COLORS,
                )
            };
            if lines == 0 {
                return Err(CaptureError::Failed("GetDIBits не вернул ни строки".into()));
            }
            started.elapsed().as_micros() as u64
        } else {
            0
        };

        Ok(Some(Frame {
            width: self.width as u32,
            height: self.height as u32,
            stride: self.width as usize * 4,
            bgra: readback.wants_pixels().then_some(self.buf.as_slice()),
            dirty: Dirty::Unknown,
            wait_us: waited.as_micros() as u64,
            work_us,
            readback_us,
            // GDI reports no change information, so there is nothing partial to
            // do: `Readback::Dirty` gets the same full copy as `Full`, and the
            // count says so rather than flattering the backend.
            copied_px: if readback.wants_pixels() {
                self.width as u64 * self.height as u64
            } else {
                0
            },
            // Nothing to compare against: without change information there is no
            // second path to time.
            compare_us: None,
        }))
    }

    fn caveats(&self) -> Vec<String> {
        vec![
            "GDI не сообщает изменившиеся области — частичное копирование здесь              невозможно, каждый кадр копируется целиком"
                .to_owned(),
        ]
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        // GDI never reports ACCESS_LOST, so this is only reached if the runner
        // is asked to reinitialise for another reason. Rebuilding from scratch is
        // the honest response to a resolution change.
        *self = Self::open()?;
        Ok(())
    }
}
