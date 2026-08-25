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

use spike_capture::{CaptureError, FrameSource, synthetic};
use spike_metrics::{FrameStat, Recorder, env::Machine};

const DEFAULT_SECONDS: u64 = 30;
const DEFAULT_FPS: u32 = 30;

struct Args {
    source: String,
    motion: synthetic::Motion,
    seconds: u64,
    fps: u32,
    width: u32,
    height: u32,
    readback: bool,
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
            readback: true,
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
    --no-readback                      не копировать пиксели в память
    -h, --help                         эта справка

Сценарии, которые нужны плану:

    spike --source dda --motion still --seconds 60     статичный рабочий стол
    spike --source dda --seconds 60                    прокрутка документа (крутить вручную)
    spike --source synthetic --motion scroll           проверка стенда без настоящего экрана
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
            "--no-readback" => args.readback = false,
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

    let mut source = match build_source(&args) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("\nОшибка: {e}");
            std::process::exit(1);
        }
    };

    let (w, h) = source.dimensions();
    println!("\n=== Источник ===\n  {}\n", source.describe());
    println!(
        "Прогон {} с при цели {} к/с, копирование пикселей {}.",
        args.seconds,
        args.fps,
        if args.readback { "включено" } else { "выключено" }
    );
    if matches!(args.source.as_str(), "dda" | "auto") {
        println!("Не трогайте мышь, если меряете статичный стол; крутите документ, если прокрутку.");
    }
    println!();

    let mut rec = Recorder::new(format!("Захват · {}", source.describe()), w, h, args.fps);
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    // One frame interval, so a still screen blocks instead of spinning.
    let timeout = Duration::from_secs_f64(1.0 / args.fps.max(1) as f64);
    let started = Instant::now();

    while Instant::now() < deadline {
        let t0 = Instant::now();
        match source.next_frame(timeout, args.readback) {
            Ok(Some(frame)) => {
                let stat = FrameStat {
                    wait_us: frame.wait_us,
                    work_us: frame.work_us,
                    readback_us: frame.readback_us,
                    is_new: true,
                    changed_px: frame.dirty.area(frame.width, frame.height),
                    dirty_rects: frame.dirty.count(),
                    // Encoding is wired in the second pass, once libvpx builds on
                    // the target. The accounting for it is already in place.
                    encode_us: None,
                    encoded_bytes: None,
                    is_keyframe: false,
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
