//! Phase-0 measurement runner.
//!
//! Answers two of the three questions the plan puts before any architecture:
//! whether the target machine can capture its own screen fast enough, and how
//! much of that screen actually changes. The third — end-to-end latency through
//! the relay — needs the transport and is not part of this harness.
//!
//! Deliberately dependency-free apart from the two local crates: this binary has
//! to be copied to a machine that may have nothing installed, and every
//! dependency is one more thing that can fail to build on a VM at the wrong
//! moment.

use std::time::{Duration, Instant};

use spike_capture::{CaptureError, Dirty, FrameSource, Readback, Rect, synthetic};
use spike_encode::{Codec, convert::I420};
use spike_metrics::{FrameStat, Recorder, env::Machine};

const DEFAULT_SECONDS: u64 = 30;
const DEFAULT_FPS: u32 = 30;

/// Raise the system timer resolution for the duration of the run.
///
/// Measured, not assumed. On the first Windows run a 33 ms timeout produced a
/// 46.7 ms wait — and 15.625 × 3 = 46.875. Windows' default timer granularity is
/// 15.6 ms, so every wait is rounded *up* to the next multiple, and a harness
/// asking for 30 fps quietly polls at about 21.
///
/// Left unfixed this understates the frame rate of every machine measured, and
/// on the target it would look like the Celeron falling behind when the truth is
/// that nobody ever asked it for 30 fps. A real capture product raises the
/// resolution for the same reason.
///
/// The guard restores the previous resolution on drop: leaving it raised costs
/// the whole machine battery life.
mod timer {
    pub struct Resolution {
        #[cfg(windows)]
        period_ms: Option<u32>,
    }

    impl Resolution {
        #[cfg(windows)]
        pub fn raise() -> Self {
            use windows::Win32::Media::{TIMECAPS, timeBeginPeriod, timeGetDevCaps};

            let mut caps = TIMECAPS { wPeriodMin: 0, wPeriodMax: 0 };
            // SAFETY: `caps` is a live, correctly sized TIMECAPS.
            let ok = unsafe { timeGetDevCaps(&mut caps, size_of::<TIMECAPS>() as u32) };
            if ok != 0 || caps.wPeriodMin == 0 {
                return Self { period_ms: None };
            }
            // SAFETY: the period is within the range the system just reported.
            if unsafe { timeBeginPeriod(caps.wPeriodMin) } != 0 {
                return Self { period_ms: None };
            }
            Self { period_ms: Some(caps.wPeriodMin) }
        }

        #[cfg(not(windows))]
        pub fn raise() -> Self {
            Self {}
        }

        pub fn describe(&self) -> String {
            #[cfg(windows)]
            {
                return match self.period_ms {
                    Some(ms) => format!("разрешение таймера поднято до {ms} мс"),
                    None => "разрешение таймера поднять не удалось — ожидания                              квантуются шагом 15,6 мс, частота кадров занижена"
                        .to_owned(),
                };
            }
            #[cfg(not(windows))]
            "разрешение таймера — как в системе".to_owned()
        }
    }

    impl Drop for Resolution {
        fn drop(&mut self) {
            #[cfg(windows)]
            if let Some(ms) = self.period_ms {
                // SAFETY: matches the successful timeBeginPeriod above.
                unsafe { windows::Win32::Media::timeEndPeriod(ms) };
            }
        }
    }
}

struct Args {
    source: String,
    motion: synthetic::Motion,
    seconds: u64,
    fps: u32,
    width: u32,
    height: u32,
    readback: Readback,
    codec: Codec,
    bitrate_kbps: u32,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            // Auto: desktop duplication where it exists, synthetic elsewhere. The
            // development Mac and a locked-down VM both land on synthetic without
            // the operator having to know that in advance.
            source: "auto".to_owned(),
            motion: synthetic::Motion::Scroll,
            seconds: DEFAULT_SECONDS,
            fps: DEFAULT_FPS,
            width: 1920,
            height: 1080,
            readback: Readback::default(),
            codec: Codec::default(),
            bitrate_kbps: 2_000,
        }
    }
}

fn usage() -> &'static str {
    "\
Стенд замеров фазы 0.

    spike [ключи]

    --source <auto|dda|gdi|synthetic>  источник кадров (по умолчанию auto)
    --motion <still|cursor|scroll|full> что «происходит» на синтетическом экране
    --seconds <N>                      длительность прогона (по умолчанию 30)
    --fps <N>                          целевая частота (по умолчанию 30)
    --size <ШxВ>                       размер для синтетического источника
    --readback <dirty|full|compare|off>  что копировать из памяти GPU:
                                       dirty   — только изменившиеся области (по умолчанию)
                                       full    — весь кадр каждый раз
                                       compare — оба пути на одном кадре, с замером
                                       off     — ничего, замер одного лишь захвата
    --encode <none|vp8|vp9>            кодировать кадры (нужна сборка --features vpx)
    --bitrate <кбит/с>                 целевой битрейт, по умолчанию 2000
    -h, --help                         эта справка

Сценарии, которые нужны плану:

    spike --source dda --seconds 60                    статичный стол: не трогать мышь
    spike --source dda --seconds 60                    прокрутка: крутить документ вручную
    spike --source synthetic --motion scroll           проверка стенда без настоящего экрана

Сравнение путей копирования — ОДИН прогон, оба пути на каждом кадре:

    spike --readback compare --seconds 30
"
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);

    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("у ключа {arg} нет значения"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", usage());
                std::process::exit(0);
            }
            "--source" => args.source = value()?,
            "--motion" => {
                let v = value()?;
                args.motion = synthetic::Motion::parse(&v)
                    .ok_or(format!("неизвестное движение: {v}"))?;
            }
            "--seconds" => {
                args.seconds = value()?.parse().map_err(|_| "--seconds ждёт число")?;
            }
            "--fps" => {
                args.fps = value()?.parse().map_err(|_| "--fps ждёт число")?;
            }
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once(['x', 'X', '×']).ok_or("--size ждёт вид 1920x1080")?;
                args.width = w.trim().parse().map_err(|_| "ширина не число")?;
                args.height = h.trim().parse().map_err(|_| "высота не число")?;
            }
            "--readback" => {
                let v = value()?;
                args.readback =
                    Readback::parse(&v).ok_or(format!("неизвестный режим копирования: {v}"))?;
            }
            // Kept as an alias: it was in the first version of the harness and
            // in the notes people are working from.
            "--no-readback" => args.readback = Readback::Off,
            "--encode" => {
                let v = value()?;
                args.codec = Codec::parse(&v).ok_or(format!("неизвестный кодек: {v}"))?;
            }
            "--bitrate" => {
                args.bitrate_kbps = value()?.parse().map_err(|_| "--bitrate ждёт число")?;
            }
            other => return Err(format!("неизвестный ключ: {other}")),
        }
    }
    Ok(args)
}

/// Build the requested source, saying plainly what happened when the preferred
/// one is unavailable.
fn build_source(args: &Args) -> Result<Box<dyn FrameSource>, String> {
    let synthetic = || -> Box<dyn FrameSource> {
        Box::new(synthetic::SyntheticSource::new(
            args.width,
            args.height,
            args.motion,
            args.fps,
        ))
    };

    match args.source.as_str() {
        "synthetic" => Ok(synthetic()),
        "dda" => open_dda().map_err(|e| format!("DDA не открылась: {e}")),
        "gdi" => open_gdi().map_err(|e| format!("GDI не открылась: {e}")),
        "auto" => match open_dda() {
            Ok(src) => Ok(src),
            Err(dda_err) => {
                eprintln!("· DDA недоступна ({dda_err}); пробую GDI");
                match open_gdi() {
                    Ok(src) => Ok(src),
                    Err(gdi_err) => {
                        eprintln!("· GDI тоже недоступна ({gdi_err}); беру синтетический источник");
                        eprintln!("  Это проверка стенда, а НЕ замер захвата экрана.");
                        Ok(synthetic())
                    }
                }
            }
        },
        other => Err(format!("неизвестный источник: {other}")),
    }
}

#[cfg(windows)]
fn open_dda() -> Result<Box<dyn FrameSource>, CaptureError> {
    spike_capture::dda::DdaSource::open().map(|s| Box::new(s) as Box<dyn FrameSource>)
}

#[cfg(not(windows))]
fn open_dda() -> Result<Box<dyn FrameSource>, CaptureError> {
    Err(CaptureError::Unavailable("только Windows".to_owned()))
}

#[cfg(windows)]
fn open_gdi() -> Result<Box<dyn FrameSource>, CaptureError> {
    spike_capture::gdi::GdiSource::open().map(|s| Box::new(s) as Box<dyn FrameSource>)
}

#[cfg(not(windows))]
fn open_gdi() -> Result<Box<dyn FrameSource>, CaptureError> {
    Err(CaptureError::Unavailable("только Windows".to_owned()))
}

/// Build the encoder, or explain why there is none.
///
/// Returns `Ok(None)` when no encoding was asked for. An explicit request that
/// cannot be honoured is an error, not a silent downgrade: a run that quietly
/// skipped encoding would print a comfortable budget and answer nothing.
#[cfg(feature = "vpx")]
fn build_encoder(
    args: &Args,
    width: u32,
    height: u32,
) -> Result<Option<spike_encode::vpx::VpxEncoder>, String> {
    use spike_encode::vpx::{Settings, VpxEncoder};
    if args.codec == Codec::None {
        return Ok(None);
    }
    let mut settings = Settings::new(width, height, args.fps);
    settings.bitrate_kbps = args.bitrate_kbps;
    VpxEncoder::new(args.codec, settings).map(Some)
}

#[cfg(not(feature = "vpx"))]
fn build_encoder(args: &Args, _width: u32, _height: u32) -> Result<Option<Never>, String> {
    if args.codec == Codec::None {
        return Ok(None);
    }
    Err(format!(
        "стенд собран без libvpx, кодировать {} нечем.\n\
         Пересоберите с --features vpx (нужны vcpkg, libvpx и LLVM — см. README)",
        args.codec.name()
    ))
}

/// Stand-in for the encoder type when the feature is off, so the call sites
/// stay identical instead of sprouting cfg blocks.
#[cfg(not(feature = "vpx"))]
enum Never {}

#[cfg(feature = "vpx")]
fn encode_one(
    enc: &mut spike_encode::vpx::VpxEncoder,
    yuv: &I420,
) -> Result<spike_encode::Encoded, String> {
    enc.encode(yuv)
}

#[cfg(not(feature = "vpx"))]
fn encode_one(enc: &mut Never, _yuv: &I420) -> Result<spike_encode::Encoded, String> {
    match *enc {}
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Ошибка: {e}\n");
            eprint!("{}", usage());
            std::process::exit(2);
        }
    };

    // The machine comes first, before any number, so a VM run cannot be mistaken
    // for a target run further down the page.
    let machine = Machine::detect();
    print!("{}", machine.render());
    println!("  сборка стенда           {}", env!("SPIKE_BUILD"));

    let mut source = match build_source(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\nОшибка: {e}");
            std::process::exit(1);
        }
    };

    let _timer = timer::Resolution::raise();

    let (w, h) = source.dimensions();
    println!("\n=== Источник ===\n  {}", source.describe());
    println!("  {}", _timer.describe());
    for caveat in source.caveats() {
        println!("\n  ⚠ {caveat}");
    }
    println!();
    println!(
        "Прогон {} с при цели {} к/с, копирование: {}.",
        args.seconds,
        args.fps,
        match args.readback {
            Readback::Off => "выключено",
            Readback::Full => "весь кадр",
            Readback::Dirty => "только изменившиеся области",
            Readback::Compare => "оба пути на каждом кадре (сравнение)",
        }
    );
    if matches!(args.source.as_str(), "dda" | "auto") {
        println!("Не трогайте мышь, если меряете статичный стол; крутите документ, если прокрутку.");
    }
    println!();

    let mut encoder = match build_encoder(&args, w, h) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("\nОшибка: {e}");
            std::process::exit(1);
        }
    };
    if args.codec != Codec::None {
        if !args.readback.wants_pixels() {
            eprintln!("\nОшибка: кодировать нечего — копирование пикселей выключено.");
            std::process::exit(2);
        }
        println!(
            "Кодирование: {} при {} кбит/с.",
            args.codec.name(),
            args.bitrate_kbps
        );
    }

    // The YUV frame persists between frames for the same reason the BGRA buffer
    // does: conversion only touches what changed, so everything else has to
    // still hold the previous frame.
    let mut yuv = I420::new(w, h);
    let mut first_frame = true;

    let mut rec = Recorder::new(format!("Захват · {}", source.describe()), w, h, args.fps);
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    // One frame interval, so a still screen blocks instead of spinning.
    let timeout = Duration::from_secs_f64(1.0 / args.fps.max(1) as f64);
    let started = Instant::now();

    while Instant::now() < deadline {
        let t0 = Instant::now();
        match source.next_frame(timeout, args.readback) {
            Ok(Some(frame)) => {
                // Convert and encode before building the stat, so both stages
                // are timed inside the frame they belong to.
                let mut convert_us = 0;
                let mut encoded = None;
                if let Some(bgra) = frame.bgra {
                    let whole = Rect {
                        left: 0,
                        top: 0,
                        right: frame.width as i32,
                        bottom: frame.height as i32,
                    };
                    // The first frame has no previous frame to leave alone, and
                    // an unknown dirty set says nothing about what is stale.
                    let rects: &[Rect] = match (&frame.dirty, first_frame) {
                        (Dirty::Rects(r), false) => r,
                        _ => std::slice::from_ref(&whole),
                    };
                    let t = Instant::now();
                    yuv.convert_bgra(bgra, frame.stride, rects);
                    convert_us = t.elapsed().as_micros() as u64;
                    first_frame = false;

                    if let Some(enc) = encoder.as_mut() {
                        let t = Instant::now();
                        match encode_one(enc, &yuv) {
                            Ok(out) => encoded = Some((t.elapsed().as_micros() as u64, out)),
                            Err(e) => {
                                eprintln!("Кодирование прекращено: {e}");
                                break;
                            }
                        }
                    }
                }

                let stat = FrameStat {
                    wait_us: frame.wait_us,
                    work_us: frame.work_us,
                    readback_us: frame.readback_us,
                    is_new: true,
                    changed_px: frame.dirty.area(frame.width, frame.height),
                    dirty_rects: frame.dirty.count(),
                    copied_px: frame.copied_px,
                    compare_us: frame.compare_us,
                    convert_us,
                    encode_us: encoded.map(|(us, _)| us),
                    encoded_bytes: encoded.map(|(_, out)| out.bytes),
                    is_keyframe: encoded.is_some_and(|(_, out)| out.keyframe),
                };
                rec.record(&stat);
            }
            Ok(None) => rec.record(&FrameStat {
                wait_us: t0.elapsed().as_micros() as u64,
                is_new: false,
                ..Default::default()
            }),
            Err(CaptureError::AccessLost) => {
                // Expected on a live machine: lock screen, UAC prompt, resolution
                // change. Counted, not fatal.
                rec.note_access_lost();
                match source.reinit() {
                    Ok(()) => rec.note_reinit(),
                    Err(e) => {
                        eprintln!("Переинициализация не удалась: {e}");
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("Захват прекращён: {e}");
                break;
            }
        }
    }

    let report = rec.finish(started.elapsed());
    println!("{report}");

    if report.frames_new == 0 {
        println!("\n⚠ Ни одного кадра с содержимым. Экран был статичен весь прогон,");
        println!("  либо источник не отдаёт кадры. Смотрите строки выше про источник.");
    }
}
