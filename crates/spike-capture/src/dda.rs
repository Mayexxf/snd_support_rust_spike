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
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use crate::{CaptureError, Dirty, Frame, FrameSource, Rect};

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
    /// Scratch for the dirty-rect metadata the API writes into.
    rect_scratch: Vec<RECT>,
    dirty: Vec<Rect>,
    buf: Vec<u8>,
    stride: usize,
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
                rect_scratch: Vec::new(),
                dirty: Vec::new(),
                buf: Vec::new(),
                stride: 0,
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

    /// Copy the frame out of GPU memory. Returns microseconds spent.
    unsafe fn readback(&mut self, resource: &IDXGIResource) -> Result<u64, CaptureError> {
        let started = Instant::now();

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
            self.width = desc.Width;
            self.height = desc.Height;
        }

        let staging = self
            .staging
            .as_ref()
            .ok_or_else(|| CaptureError::Failed("staging-текстура отсутствует".into()))?;

        // SAFETY: both textures are live and share a description.
        unsafe { self.context.CopyResource(staging, &src) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `mapped` is live; the matching Unmap runs before returning on
        // every path below.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Failed(format!("Map: {e}")))?;
        }

        let row_bytes = desc.Width as usize * 4;
        self.stride = row_bytes;
        let total = row_bytes * desc.Height as usize;
        if self.buf.len() != total {
            self.buf.resize(total, 0);
        }

        // SAFETY: the mapped region holds `RowPitch * Height` readable bytes;
        // we copy `row_bytes <= RowPitch` from each row. RowPitch is often larger
        // than width*4 — copying it wholesale would misalign every row.
        unsafe {
            let src_ptr = mapped.pData as *const u8;
            let pitch = mapped.RowPitch as usize;
            for y in 0..desc.Height as usize {
                std::ptr::copy_nonoverlapping(
                    src_ptr.add(y * pitch),
                    self.buf.as_mut_ptr().add(y * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(staging, 0);
        }

        Ok(started.elapsed().as_micros() as u64)
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
        readback: bool,
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

        let readback_us = if readback {
            match resource.as_ref() {
                Some(res) => {
                    // SAFETY: the resource is valid until ReleaseFrame below.
                    let r = unsafe { self.readback(res) };
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
            0
        };

        let work_us = work_start
            .elapsed()
            .as_micros()
            .saturating_sub(u128::from(readback_us)) as u64;

        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            stride: self.stride,
            bgra: (readback && !self.buf.is_empty()).then_some(self.buf.as_slice()),
            dirty,
            wait_us,
            work_us,
            readback_us,
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
        Ok(())
    }
}
