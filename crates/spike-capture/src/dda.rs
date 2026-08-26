//! DXGI Desktop Duplication.
//!
//! The real answer to phase 0's capture question, and the only backend that
//! reports *which* parts of the screen changed — which is the measurement the
//! whole encoding budget rests on.
//!
//! **Two behaviours here are load-bearing and must not be "fixed" into
//! failures.**
//!
//! `DXGI_ERROR_WAIT_TIMEOUT` means the desktop did not change within the
//! timeout. On an idle support session that is the overwhelming majority of
//! polls, and it is the good case, not an error.
//!
//! `DXGI_ERROR_ACCESS_LOST` means the desktop went away underneath us: a UAC
//! prompt or Ctrl+Alt+Del switched to the secure desktop, the screen locked, the
//! resolution changed, or the display driver reset. The duplication object is
//! dead and a new one has to be built. A session that dropped every time the
//! user saw a UAC prompt would be useless, so the runner counts these and
//! reinitialises.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use crate::{CaptureError, Dirty, Frame, FrameSource, Readback, Rect};

pub struct DdaSource {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// `None` between losing the desktop and rebuilding. Optional rather than
    /// always-present because the API allows only one duplication per output per
    /// device: the dead object must be dropped *before* a new one is requested,
    /// and there is nothing valid to hold in the meantime.
    dupl: Option<IDXGIOutputDuplication>,
    width: u32,
    height: u32,
    adapter_name: String,
    /// Staging texture for GPU→CPU copies, rebuilt when the desktop resizes.
    staging: Option<ID3D11Texture2D>,
    /// Second staging texture. Only [`Readback::Buffered`] uses it: it reads
    /// this one while the GPU is still filling the other.
    staging_alt: Option<ID3D11Texture2D>,
    /// Which of the two the next buffered copy goes into.
    write_alt: bool,
    /// Regions copied into the texture that is *not* the next write target,
    /// waiting to be mapped.
    ///
    /// `None` means nothing is in flight — the first frame after opening,
    /// resizing, or losing the desktop.
    pending: Option<Vec<Rect>>,
    /// Regions the last buffered read actually put into `buf`. What the caller
    /// has to be told changed, which is not what changed on screen this frame.
    applied: Option<Vec<Rect>>,
    /// Scratch for the dirty-rect metadata the API writes into.
    rect_scratch: Vec<RECT>,
    dirty: Vec<Rect>,
    buf: Vec<u8>,
    stride: usize,
    /// The CPU-side buffer holds nothing trustworthy yet.
    ///
    /// Partial copies only update what changed and rely on the rest of the
    /// buffer still holding the previous frame. Right after opening, resizing or
    /// reinitialising there is no previous frame, so the next copy has to be a
    /// full one however the caller asked. Without this the first frame after a
    /// UAC prompt would be three quarters uninitialised memory.
    force_full: bool,
    /// Frames copied so far. Only used to alternate path order in comparison
    /// runs, so wrapping is fine.
    frames_done: u64,
}

impl DdaSource {
    pub fn open() -> Result<Self, CaptureError> {
        // SAFETY: every call below is a plain COM creation with locally owned
        // out-parameters. Nothing outlives this function that is not moved into
        // the returned struct.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| CaptureError::Unavailable(format!("DXGI-фабрика: {e}")))?;

            let adapter = factory
                .EnumAdapters1(0)
                .map_err(|e| CaptureError::Unavailable(format!("видеоадаптер не найден: {e}")))?;

            let adapter_name = adapter
                .GetDesc()
                .map(|d| {
                    String::from_utf16_lossy(&d.Description)
                        .trim_end_matches('\0')
                        .trim()
                        .to_owned()
                })
                .unwrap_or_else(|_| "неизвестный".to_owned());

            let output = adapter.EnumOutputs(0).map_err(|e| {
                CaptureError::Unavailable(format!(
                    "к адаптеру не подключён монитор: {e}. \
                     На виртуальной машине и на сервере без дисплея это обычное дело"
                ))
            })?;

            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                // The adapter is passed explicitly, so the driver type must be
                // UNKNOWN — passing HARDWARE alongside an adapter is an error.
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None::<*mut D3D_FEATURE_LEVEL>,
                Some(&mut context),
            )
            .map_err(|e| CaptureError::Unavailable(format!("D3D11 не создалась: {e}")))?;

            let device = device
                .ok_or_else(|| CaptureError::Unavailable("D3D11 вернула пустое устройство".into()))?;
            let context = context
                .ok_or_else(|| CaptureError::Unavailable("D3D11 вернула пустой контекст".into()))?;

            let output1: IDXGIOutput1 = output
                .cast()
                .map_err(|e| CaptureError::Unavailable(format!("IDXGIOutput1 недоступен: {e}")))?;

            let dupl = output1.DuplicateOutput(&device).map_err(|e| {
                CaptureError::Unavailable(format!(
                    "дублирование рабочего стола не открылось: {e}. \
                     Частые причины: сеанс RDP, отсутствие монитора, \
                     уже запущенная программа захвата"
                ))
            })?;

            let desc = dupl.GetDesc();
            let (width, height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);

            Ok(Self {
                device,
                context,
                output: output1,
                dupl: Some(dupl),
                width,
                height,
                adapter_name,
                staging: None,
                staging_alt: None,
                write_alt: false,
                pending: None,
                applied: None,
                rect_scratch: Vec::new(),
                dirty: Vec::new(),
                buf: Vec::new(),
                stride: 0,
                force_full: true,
                frames_done: 0,
            })
        }
    }

    /// The live duplication object.
    ///
    /// Returns a clone rather than a borrow: the COM pointer is refcounted and
    /// cheap to clone, and holding a borrow of `self.dupl` would block the
    /// mutable access to `self.rect_scratch` that the very next line needs.
    fn dupl(&self) -> Result<IDXGIOutputDuplication, CaptureError> {
        self.dupl.clone().ok_or(CaptureError::AccessLost)
    }

    /// Collect the dirty rectangles the API reported for this frame.
    ///
    /// When the driver supplies no metadata the answer is [`Dirty::Unknown`],
    /// not an empty list: an empty list would read as "nothing changed" and
    /// silently understate the encoder's work.
    unsafe fn read_dirty(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Dirty {
        if info.TotalMetadataBufferSize == 0 {
            return Dirty::Unknown;
        }

        let need = info.TotalMetadataBufferSize as usize / size_of::<RECT>() + 1;
        if self.rect_scratch.len() < need {
            self.rect_scratch.resize(need, RECT::default());
        }

        let Ok(dupl) = self.dupl() else {
            return Dirty::Unknown;
        };
        let mut required: u32 = 0;
        let capacity = (self.rect_scratch.len() * size_of::<RECT>()) as u32;
        // SAFETY: the buffer is at least `capacity` bytes and `required` is a
        // live u32. Both live for the duration of the call.
        let ok = unsafe {
            dupl.GetFrameDirtyRects(capacity, self.rect_scratch.as_mut_ptr(), &mut required)
        };
        if ok.is_err() {
            return Dirty::Unknown;
        }

        let count = required as usize / size_of::<RECT>();
        self.dirty.clear();
        self.dirty.extend(self.rect_scratch[..count].iter().map(|r| Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }));
        Dirty::Rects(std::mem::take(&mut self.dirty))
    }

    /// Make sure the staging texture matches the frame, then hand both back.
    ///
    /// Split out of the copy paths so their timings measure copying and nothing
    /// else — a run where the texture happened to be rebuilt would otherwise
    /// charge that to whichever path went first.
    unsafe fn prepare(
        &mut self,
        resource: &IDXGIResource,
    ) -> Result<(ID3D11Texture2D, ID3D11Texture2D, D3D11_TEXTURE2D_DESC), CaptureError> {
        let src: ID3D11Texture2D = resource
            .cast()
            .map_err(|e| CaptureError::Failed(format!("кадр не текстура: {e}")))?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a live, correctly typed out-parameter.
        unsafe { src.GetDesc(&mut desc) };

        // Rebuild the staging texture when the desktop changes size. Reusing a
        // stale one would copy the wrong number of rows and read past the buffer.
        let needs_new = match &self.staging {
            None => true,
            Some(existing) => {
                let mut have = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: same as above.
                unsafe { existing.GetDesc(&mut have) };
                have.Width != desc.Width || have.Height != desc.Height || have.Format != desc.Format
            }
        };

        if needs_new {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                BindFlags: 0,
                MiscFlags: 0,
                ..desc
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            // SAFETY: `staging_desc` outlives the call; `tex` is a live out-param.
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut tex))
                    .map_err(|e| CaptureError::Failed(format!("staging-текстура: {e}")))?;
            }
            self.staging = tex;
            let mut alt: Option<ID3D11Texture2D> = None;
            // SAFETY: same description, same lifetime as above.
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut alt))
                    .map_err(|e| CaptureError::Failed(format!("вторая staging-текстура: {e}")))?;
            }
            self.staging_alt = alt;
            self.width = desc.Width;
            self.height = desc.Height;
            // A new staging texture shares nothing with the previous frame, and
            // whatever the buffered path had in flight went with the old one.
            self.force_full = true;
            self.pending = None;
            self.applied = None;
            self.write_alt = false;
        }

        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| CaptureError::Failed("staging-текстура отсутствует".into()))?
            .clone();

        let row_bytes = desc.Width as usize * 4;
        self.stride = row_bytes;
        let total = row_bytes * desc.Height as usize;
        if self.buf.len() != total {
            self.buf.resize(total, 0);
            self.force_full = true;
        }

        Ok((src, staging, desc))
    }

    /// Copy the whole frame. Returns microseconds and pixels moved.
    unsafe fn copy_full(
        &mut self,
        src: &ID3D11Texture2D,
        staging: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
    ) -> Result<(u64, u64), CaptureError> {
        let started = Instant::now();
        // SAFETY: both textures are live and share a description.
        unsafe { self.context.CopyResource(staging, src) };
        let full = Rect { left: 0, top: 0, right: desc.Width as i32, bottom: desc.Height as i32 };
        let px = unsafe { self.map_and_copy(staging, desc, std::slice::from_ref(&full))? };
        Ok((started.elapsed().as_micros() as u64, px))
    }

    /// Copy only the given regions. Returns microseconds and pixels moved.
    unsafe fn copy_regions(
        &mut self,
        src: &ID3D11Texture2D,
        staging: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
        rects: &[Rect],
    ) -> Result<(u64, u64), CaptureError> {
        let started = Instant::now();
        for r in rects {
            let box_ = D3D11_BOX {
                left: r.left as u32,
                top: r.top as u32,
                front: 0,
                right: r.right as u32,
                bottom: r.bottom as u32,
                back: 1,
            };
            // Destination coordinates match the source: the staging texture keeps
            // a full-frame image that is patched in place, which is what makes a
            // partial copy legal at all.
            // SAFETY: the box is clamped to the source dimensions by the caller
            // and both resources share a format.
            unsafe {
                self.context.CopySubresourceRegion(
                    staging,
                    0,
                    r.left as u32,
                    r.top as u32,
                    0,
                    src,
                    0,
                    Some(&box_),
                );
            }
        }
        let px = unsafe { self.map_and_copy(staging, desc, rects)? };
        Ok((started.elapsed().as_micros() as u64, px))
    }

    /// Start this frame's copy, then read the one started last frame.
    ///
    /// The synchronous paths issue a copy and map the same texture immediately,
    /// which parks the CPU until the GPU is done. Here the map targets the
    /// *other* texture, whose copy was issued a frame ago and has had the whole
    /// frame interval to finish. What it costs is one issued copy plus a map
    /// that should already be satisfiable.
    ///
    /// The staging textures need no history: [`Self::map_and_copy`] reads only
    /// the rectangles it is given, and those are the ones written into that same
    /// texture. Only `buf` accumulates, and it always has.
    ///
    /// The first frame has nothing in flight, so it maps what it just issued and
    /// pays the stall once. The second frame then re-reads the first frame's
    /// regions — the same bytes into the same place, which costs a copy and
    /// changes nothing.
    unsafe fn copy_buffered(
        &mut self,
        src: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
        rects: Option<&[Rect]>,
    ) -> Result<(u64, u64), CaptureError> {
        let started = Instant::now();

        let a = self
            .staging
            .as_ref()
            .ok_or_else(|| CaptureError::Failed("staging-текстура отсутствует".into()))?
            .clone();
        let b = self
            .staging_alt
            .as_ref()
            .ok_or_else(|| CaptureError::Failed("вторая staging-текстура отсутствует".into()))?
            .clone();
        let (write, read) = if self.write_alt { (b, a) } else { (a, b) };

        let now: Vec<Rect> = match rects {
            Some(r) => r.to_vec(),
            None => {
                vec![Rect { left: 0, top: 0, right: desc.Width as i32, bottom: desc.Height as i32 }]
            }
        };

        // Issue and do not wait.
        match rects {
            // SAFETY: both textures share a description.
            None => unsafe { self.context.CopyResource(&write, src) },
            Some(r) => {
                for rr in r {
                    let box_ = D3D11_BOX {
                        left: rr.left as u32,
                        top: rr.top as u32,
                        front: 0,
                        right: rr.right as u32,
                        bottom: rr.bottom as u32,
                        back: 1,
                    };
                    // SAFETY: the box is clamped to the source by the caller.
                    unsafe {
                        self.context.CopySubresourceRegion(
                            &write,
                            0,
                            rr.left as u32,
                            rr.top as u32,
                            0,
                            src,
                            0,
                            Some(&box_),
                        );
                    }
                }
            }
        }

        let applied = match self.pending.take() {
            Some(prev) => {
                // SAFETY: those regions were copied into `read` last frame.
                unsafe { self.map_and_copy(&read, desc, &prev)? };
                prev
            }
            None => {
                // SAFETY: just copied into `write` above.
                unsafe { self.map_and_copy(&write, desc, &now)? };
                now.clone()
            }
        };

        // **The caller must be told which regions actually moved.** `buf` now
        // holds last frame's changes, not this frame's, and a caller handed
        // this frame's rectangles would re-encode regions that still show the
        // old pixels while leaving the ones that really changed alone. Measured
        // before it was fixed: the delta frame went from 8607 bytes to 12761,
        // because the encoder was being handed a picture that drifted.
        let px = applied.iter().map(|r| r.area()).sum();
        self.applied = Some(applied);
        self.pending = Some(now);
        self.write_alt = !self.write_alt;
        Ok((started.elapsed().as_micros() as u64, px))
    }

    /// Map the staging texture and pull the given regions into the CPU buffer.
    unsafe fn map_and_copy(
        &mut self,
        staging: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
        rects: &[Rect],
    ) -> Result<u64, CaptureError> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `mapped` is live; the matching Unmap runs before returning.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Failed(format!("Map: {e}")))?;
        }

        let row_bytes = desc.Width as usize * 4;

        // SAFETY: the mapped region holds `RowPitch * Height` readable bytes, and
        // every row range is clamped to the texture by the caller. RowPitch is
        // often larger than width*4 — copying it wholesale would misalign rows.
        let px = unsafe {
            let src_ptr = mapped.pData as *const u8;
            let pitch = mapped.RowPitch as usize;
            let mut px = 0u64;
            for r in rects {
                let x0 = r.left as usize * 4;
                let span = (r.right - r.left) as usize * 4;
                for y in r.top as usize..r.bottom as usize {
                    std::ptr::copy_nonoverlapping(
                        src_ptr.add(y * pitch + x0),
                        self.buf.as_mut_ptr().add(y * row_bytes + x0),
                        span,
                    );
                }
                px += r.area();
            }
            px
        };

        // SAFETY: matches the successful Map above.
        unsafe { self.context.Unmap(staging, 0) };
        Ok(px)
    }

    /// Copy the frame out of GPU memory according to `mode`.
    ///
    /// Returns the cost of the path that would ship, the pixels it moved, and —
    /// in [`Readback::Compare`] — what the other path cost on this same frame.
    unsafe fn readback(
        &mut self,
        resource: &IDXGIResource,
        mode: Readback,
        dirty: &Dirty,
    ) -> Result<(u64, u64, Option<u64>), CaptureError> {
        // SAFETY: the resource is a live desktop frame.
        let (src, staging, desc) = unsafe { self.prepare(resource)? };

        // Regions to move. `Dirty::Unknown` means the driver told us nothing, so
        // the honest answer is the whole screen rather than an empty list. The
        // same goes for the first frame after a reset, where the rest of the
        // buffer holds nothing worth keeping.
        let partial: Option<Vec<Rect>> = match (dirty, self.force_full) {
            (Dirty::Rects(r), false) => Some(
                r.iter()
                    .map(|r| clamp(*r, desc.Width, desc.Height))
                    .filter(|r| r.area() > 0)
                    .collect(),
            ),
            _ => None,
        };

        let result = match (mode, &partial) {
            (Readback::Off, _) => (0, 0, None),

            (Readback::Buffered, _) => {
                // SAFETY: prepared above; any rects are clamped.
                let (us, px) = unsafe { self.copy_buffered(&src, &desc, partial.as_deref())? };
                (us, px, None)
            }

            (Readback::Full, _) | (Readback::Dirty, None) | (Readback::Compare, None) => {
                // SAFETY: prepared above.
                let (us, px) = unsafe { self.copy_full(&src, &staging, &desc)? };
                (us, px, None)
            }

            (Readback::Dirty, Some(rects)) => {
                // SAFETY: rects are clamped to the texture.
                let (us, px) = unsafe { self.copy_regions(&src, &staging, &desc, rects)? };
                (us, px, None)
            }

            (Readback::Compare, Some(rects)) => {
                // Alternate which path goes first. Whichever runs second finds
                // the source rows warm, and always giving that advantage to the
                // same path would bias the answer the run exists to produce.
                // SAFETY: prepared above; rects are clamped.
                let (dirty_us, px, full_us) = if self.frames_done % 2 == 0 {
                    let (f, _) = unsafe { self.copy_full(&src, &staging, &desc)? };
                    let (d, px) = unsafe { self.copy_regions(&src, &staging, &desc, rects)? };
                    (d, px, f)
                } else {
                    let (d, px) = unsafe { self.copy_regions(&src, &staging, &desc, rects)? };
                    let (f, _) = unsafe { self.copy_full(&src, &staging, &desc)? };
                    (d, px, f)
                };
                (dirty_us, px, Some(full_us))
            }
        };

        self.frames_done = self.frames_done.wrapping_add(1);
        if mode.wants_pixels() {
            self.force_full = false;
        }
        Ok(result)
    }
}

/// Clip a reported rectangle to the texture.
///
/// Cheap insurance rather than a known defect: the driver supplies these, a
/// rectangle reaching one pixel past the edge would read out of bounds, and the
/// cost of checking is nothing next to the cost of finding out.
fn clamp(r: Rect, width: u32, height: u32) -> Rect {
    Rect {
        left: r.left.clamp(0, width as i32),
        top: r.top.clamp(0, height as i32),
        right: r.right.clamp(0, width as i32),
        bottom: r.bottom.clamp(0, height as i32),
    }
}

impl FrameSource for DdaSource {
    fn describe(&self) -> String {
        format!(
            "DXGI Desktop Duplication, {}×{}, адаптер «{}»",
            self.width, self.height, self.adapter_name
        )
    }

    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn next_frame(
        &mut self,
        timeout: Duration,
        readback: Readback,
    ) -> Result<Option<Frame<'_>>, CaptureError> {
        let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let dupl = self.dupl()?;

        let wait_start = Instant::now();
        // SAFETY: both out-parameters are live for the call.
        let acquired = unsafe { dupl.AcquireNextFrame(timeout_ms, &mut info, &mut resource) };
        let wait_us = wait_start.elapsed().as_micros() as u64;

        if let Err(e) = acquired {
            return match e.code() {
                // The good case on an idle desktop, not a failure.
                DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
                DXGI_ERROR_ACCESS_LOST => Err(CaptureError::AccessLost),
                _ => Err(CaptureError::Failed(format!("AcquireNextFrame: {e}"))),
            };
        }

        let work_start = Instant::now();

        // LastPresentTime of zero means only the mouse pointer moved; the desktop
        // image is unchanged. The frame still has to be released.
        if info.LastPresentTime == 0 {
            // SAFETY: a frame was successfully acquired above.
            let _ = unsafe { dupl.ReleaseFrame() };
            return Ok(None);
        }

        // SAFETY: called while a frame is held.
        let dirty = unsafe { self.read_dirty(&info) };

        let (readback_us, copied_px, compare_us) = if readback.wants_pixels() {
            match resource.as_ref() {
                Some(res) => {
                    // SAFETY: the resource is valid until ReleaseFrame below.
                    let r = unsafe { self.readback(res, readback, &dirty) };
                    // Release before propagating: leaking the frame would wedge
                    // every later AcquireNextFrame with ACCESS_DENIED.
                    // SAFETY: a frame is held.
                    let _ = unsafe { dupl.ReleaseFrame() };
                    r?
                }
                None => {
                    // SAFETY: a frame is held.
                    let _ = unsafe { dupl.ReleaseFrame() };
                    return Err(CaptureError::Failed("кадр без ресурса".into()));
                }
            }
        } else {
            // SAFETY: a frame is held.
            let _ = unsafe { dupl.ReleaseFrame() };
            (0, 0, None)
        };

        // Subtract *both* copies, not just the one that would ship. In a
        // comparison run the rejected path also ran on this frame, and leaving
        // it in charged the whole-frame copy to "capture work" — which made the
        // stage look 70× more expensive than it is and double-counted it in the
        // frame budget, taking the verdict with it.
        let copies_us = u128::from(readback_us) + u128::from(compare_us.unwrap_or(0));
        let work_us = work_start.elapsed().as_micros().saturating_sub(copies_us) as u64;

        // The buffered path filled `buf` from the frame *before* this one, so
        // what changed on screen just now is not what changed in the buffer.
        // Report the regions that were actually written, or every consumer
        // downstream — conversion, encoder, the "changed area" line — is
        // working from the wrong set.
        let dirty = match self.applied.take() {
            Some(rects) => Dirty::Rects(rects),
            None => dirty,
        };

        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            stride: self.stride,
            bgra: (readback.wants_pixels() && !self.buf.is_empty())
                .then_some(self.buf.as_slice()),
            dirty,
            wait_us,
            work_us,
            readback_us,
            copied_px,
            compare_us,
        }))
    }

    fn caveats(&self) -> Vec<String> {
        let mut out = Vec::new();
        // "Microsoft Basic Render Driver" is WARP, the CPU rasteriser Windows
        // falls back to when no display driver is present — routine inside a VM.
        // Direct3D still works, so capture succeeds and the timings look sane,
        // but nothing here touched a GPU: the readback measured main memory, not
        // a bus transfer. On a real Braswell iGPU that stage behaves differently
        // in both directions, and reading these numbers as an answer about it
        // would be a mistake the harness has to prevent, not enable.
        let name = self.adapter_name.to_lowercase();
        if name.contains("basic render") || name.contains("basic display") || name.contains("warp") {
            out.push(format!(
                "адаптер «{}» — программный растеризатор, настоящего GPU нет. \
                 Строка «копирование в память» меряет память, а не шину GPU",
                self.adapter_name
            ));
        }
        out
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        // Drop the dead object *first*. DXGI allows one duplication per output
        // per device, so rebuilding while the old one is still alive fails with
        // DXGI_ERROR_NOT_CURRENTLY_AVAILABLE — and then the session never
        // recovers from a UAC prompt.
        self.dupl = None;

        // SAFETY: the output and device are still live; the previous duplication
        // has been released above.
        let dupl = unsafe { self.output.DuplicateOutput(&self.device) }
            .map_err(|e| CaptureError::Failed(format!("повторное дублирование: {e}")))?;

        // SAFETY: freshly created object.
        let desc = unsafe { dupl.GetDesc() };
        self.width = desc.ModeDesc.Width;
        self.height = desc.ModeDesc.Height;
        self.dupl = Some(dupl);

        // A resolution change is the common cause of ACCESS_LOST, so the staging
        // texture is thrown away rather than trusted to still match.
        self.staging = None;
        self.buf.clear();
        // Nothing in the CPU buffer survives a reinitialisation, so the next
        // copy must be a full one whatever the caller asks for.
        self.force_full = true;
        Ok(())
    }
}
