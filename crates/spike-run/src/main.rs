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

use std::path::{Path, PathBuf};

use spike_capture::image::{ImageSource, Scenario};
use spike_capture::shot::Shot;
use spike_capture::{CaptureError, Dirty, FrameSource, Readback, Rect, image, synthetic};
use spike_encode::{Codec, convert::I420};
use spike_metrics::{FrameStat, Recorder, TrackStat, env::Machine};

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
    /// Kept as text: the synthetic desktop and a real screenshot understand
    /// different scenario names, and which one applies is not known until the
    /// source is chosen.
    motion: String,
    seconds: u64,
    /// Stop after this many frames of content, whatever the clock says.
    ///
    /// The point of the whole exercise is dividing one machine's number by
    /// another's, and that only works if both encoded the same frames. A
    /// sixty-second run gives 422 frames here and maybe a hundred on the target
    /// — different frames, different content, nothing to divide.
    frames: Option<u64>,
    /// Capture one frame to a file and stop.
    grab: Option<PathBuf>,
    /// Scroll step for a screenshot source, in pixels per frame.
    step: u32,
    fps: u32,
    width: u32,
    height: u32,
    readback: Readback,
    codec: Codec,
    bitrate_kbps: u32,
    /// Integer downscale before encoding. The lever that actually matters.
    scale: u32,
    cpu_used: i32,
    threads: u32,
    /// Encoder configurations to measure side by side on identical frames.
    /// Empty for an ordinary single-encoder run.
    compare: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            // Auto: desktop duplication where it exists, synthetic elsewhere. The
            // development Mac and a locked-down VM both land on synthetic without
            // the operator having to know that in advance.
            source: "auto".to_owned(),
            motion: "scroll".to_owned(),
            seconds: DEFAULT_SECONDS,
            frames: None,
            grab: None,
            step: image::DEFAULT_STEP,
            fps: DEFAULT_FPS,
            width: 1920,
            height: 1080,
            readback: Readback::default(),
            codec: Codec::default(),
            bitrate_kbps: 2_000,
            scale: 1,
            cpu_used: 8,
            threads: 2,
            compare: Vec::new(),
        }
    }
}

/// One encoder configuration in a comparison run.
///
/// Written as `vp9:s2:t4:c9:b1500` — the codec first, then any of scale,
/// threads, cpu-used and bitrate in any order. Anything left out falls back to
/// the run-wide `--scale`, `--threads`, `--cpu-used`, `--bitrate`, so the common
/// case stays short: `--compare vp9,vp8` compares two codecs at the same
/// settings.
#[derive(Debug, Clone)]
struct Spec {
    label: String,
    codec: Codec,
    scale: u32,
    threads: u32,
    cpu_used: i32,
    bitrate_kbps: u32,
}

impl Spec {
    fn parse(text: &str, args: &Args) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("пустая конфигурация в --compare".to_owned());
        }
        let mut parts = text.split(':');
        let codec_text = parts.next().unwrap_or_default();
        let codec = Codec::parse(codec_text)
            .ok_or(format!("неизвестный кодек «{codec_text}» в «{text}»"))?;
        if codec == Codec::None {
            return Err(format!("«{text}»: сравнивать нечего, none — это не кодек"));
        }

        let mut spec = Spec {
            label: text.to_owned(),
            codec,
            scale: args.scale,
            threads: args.threads,
            cpu_used: args.cpu_used,
            bitrate_kbps: args.bitrate_kbps,
        };

        for part in parts {
            // Split by character rather than by byte: a typo with a Cyrillic
            // letter would otherwise land mid-codepoint and panic instead of
            // producing the error message the operator needs.
            let mut chars = part.chars();
            let tag = chars.next().ok_or(format!("пустая часть в «{text}»"))?;
            let value = chars.as_str();
            let number = |what: &str| -> Result<i64, String> {
                value
                    .parse::<i64>()
                    .map_err(|_| format!("«{text}»: {what} ждёт число, а не «{value}»"))
            };
            match tag {
                's' => {
                    let n = number("масштаб s")?;
                    if !(1..=8).contains(&n) {
                        return Err(format!("«{text}»: масштаб вне диапазона 1..8"));
                    }
                    spec.scale = n as u32;
                }
                't' => {
                    let n = number("потоки t")?;
                    if !(1..=64).contains(&n) {
                        return Err(format!("«{text}»: потоков вне диапазона 1..64"));
                    }
                    spec.threads = n as u32;
                }
                'c' => {
                    let n = number("cpu-used c")?;
                    if !(-16..=16).contains(&n) {
                        return Err(format!("«{text}»: cpu-used вне диапазона -16..16"));
                    }
                    spec.cpu_used = n as i32;
                }
                'b' => {
                    let n = number("битрейт b")?;
                    if !(1..=100_000).contains(&n) {
                        return Err(format!("«{text}»: битрейт вне диапазона 1..100000"));
                    }
                    spec.bitrate_kbps = n as u32;
                }
                other => {
                    return Err(format!(
                        "«{text}»: непонятная часть «{part}». Ожидались s (масштаб), \
                         t (потоки), c (cpu-used), b (битрейт), а не «{other}»"
                    ));
                }
            }
        }
        Ok(spec)
    }
}

fn usage() -> &'static str {
    "\
Стенд замеров фазы 0.

    spike [ключи]

    --source <auto|dda|gdi|synthetic>  источник кадров (по умолчанию auto)
    --source image:<файл>              снимок экрана, движется по сценарию
    --motion <сценарий>                что происходит на экране:
                                       синтетический — still, cursor, scroll, full
                                       снимок — still, caret, edit, scroll, drag
    --grab <файл>                      снять один кадр в файл и выйти
    --step <N>                         шаг прокрутки снимка, пикселей за кадр
    --frames <N>                       остановиться после N кадров с содержимым.
                                       Для сравнения машин между собой: обе должны
                                       закодировать одну и ту же последовательность.
                                       Со снимком отключает выдержку темпа
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
    --scale <1..8>                     уменьшить кадр перед кодированием во
                                       столько раз (при 2: 1920×1080 → 960×540)
    --cpu-used <N>                     скорость кодера: больше — быстрее и хуже
    --threads <N>                      потоков кодера, по умолчанию 2
    --compare <конф,конф,...>          мерить несколько конфигураций кодера на
                                       ОДНИХ И ТЕХ ЖЕ кадрах, в одном прогоне.
                                       Конфигурация: кодек[:sМасштаб][:tПотоки]
                                       [:cCpuUsed][:bБитрейт], опущенное берётся
                                       из ключей выше. Пример:
                                         --compare vp9:s2:t2,vp9:s2:t4,vp8:s2
    -h, --help                         эта справка

Сценарии, которые нужны плану:

    spike --source dda --seconds 60                    статичный стол: не трогать мышь
    spike --source dda --seconds 60                    прокрутка: крутить документ вручную
    spike --source synthetic --motion scroll           проверка стенда без настоящего экрана

Сравнение путей копирования — ОДИН прогон, оба пути на каждом кадре:

    spike --readback compare --seconds 30

Сравнение настроек кодера — тоже ОДИН прогон. Отдельные прогоны сравнивать
нельзя: содержимое экрана меняется между ними и объясняет разницу целиком.

    spike --compare vp9:s2:t2,vp9:s2:t4,vp8:s2 --seconds 60
    spike --compare vp9:s1,vp9:s2,vp9:s3,vp9:s4 --seconds 60

Сравнение МАШИН между собой — через снимок, одинаковый на обеих:

    spike --grab heavy.shot                       снять один раз, где есть
    spike --source image:heavy.shot --motion scroll --frames 300 --compare vp9:s2

Снимок не коммитится: это кадр настоящего стола, а репозиторий публичный.
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
            "--motion" => args.motion = value()?,
            "--frames" => {
                let n: u64 = value()?.parse().map_err(|_| "--frames ждёт число")?;
                if n == 0 {
                    return Err("--frames 0 — нечего мерить".to_owned());
                }
                args.frames = Some(n);
            }
            "--grab" => args.grab = Some(PathBuf::from(value()?)),
            "--step" => {
                args.step = value()?.parse().map_err(|_| "--step ждёт число")?;
                if args.step == 0 {
                    return Err("--step 0 — это сценарий still".to_owned());
                }
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
            "--scale" => {
                args.scale = value()?.parse().map_err(|_| "--scale ждёт число")?;
                if !(1..=8).contains(&args.scale) {
                    return Err("--scale вне диапазона 1..8".to_owned());
                }
            }
            "--cpu-used" => {
                args.cpu_used = value()?.parse().map_err(|_| "--cpu-used ждёт число")?;
            }
            "--threads" => {
                args.threads = value()?.parse().map_err(|_| "--threads ждёт число")?;
            }
            "--compare" => {
                args.compare =
                    value()?.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
                if args.compare.is_empty() {
                    return Err("--compare без конфигураций".to_owned());
                }
            }
            other => return Err(format!("неизвестный ключ: {other}")),
        }
    }
    Ok(args)
}

/// Build the requested source, saying plainly what happened when the preferred
/// one is unavailable.
fn build_source(args: &Args) -> Result<Box<dyn FrameSource>, String> {
    let synthetic = || -> Result<Box<dyn FrameSource>, String> {
        let motion = synthetic::Motion::parse(&args.motion)
            .ok_or(format!("синтетический источник не знает движения «{}»", args.motion))?;
        Ok(Box::new(synthetic::SyntheticSource::new(
            args.width,
            args.height,
            motion,
            args.fps,
        )))
    };

    if let Some(path) = args.source.strip_prefix("image:") {
        return build_image(args, Path::new(path));
    }

    match args.source.as_str() {
        "synthetic" => synthetic(),
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
                        synthetic()
                    }
                }
            }
        },
        other => Err(format!("неизвестный источник: {other}")),
    }
}

/// A saved screenshot, moved on a fixed script.
///
/// Pacing is dropped whenever `--frames` is given: with a fixed number of
/// frames the run is a benchmark, and waiting out a 33 ms interval on a fast
/// machine would only hide how fast it is.
fn build_image(args: &Args, path: &Path) -> Result<Box<dyn FrameSource>, String> {
    let scenario = Scenario::parse(&args.motion).ok_or(format!(
        "снимок не знает сценария «{}». Есть still, caret, edit, scroll, drag",
        args.motion
    ))?;
    let interval = args
        .frames
        .is_none()
        .then(|| Duration::from_secs_f64(1.0 / args.fps.max(1) as f64));
    ImageSource::open(path, scenario, args.step, interval)
        .map(|s| Box::new(s) as Box<dyn FrameSource>)
        .map_err(|e| format!("{e}"))
}

/// Take one frame and write it out, then stop.
///
/// Whatever `--source` says is what gets grabbed, so a synthetic frame can be
/// saved on a machine with no desktop duplication — which is how the replay path
/// gets tested without Windows.
fn grab(args: &Args, path: &Path) -> Result<(), String> {
    let mut source = build_source(args)?;
    println!("Источник: {}", source.describe());
    println!("Снимаю один кадр в {} …", path.display());
    println!("Если ничего не происходит — пошевелите мышью: пока экран не меняется,");
    println!("захват честно не отдаёт ни одного кадра.");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match source.next_frame(Duration::from_millis(200), Readback::Full) {
            Ok(Some(frame)) => {
                let Some(bgra) = frame.bgra else {
                    return Err("захват не отдал пиксели".to_owned());
                };
                let (w, h, stride) = (frame.width, frame.height, frame.stride);
                Shot::save(path, w, h, stride, bgra)?;
                let shot = Shot::load(path)?;
                let fp = shot.fingerprint();
                println!("\nСнято: {w}×{h}, {} МБ", shot.bgra.len() / 1_048_576);
                println!("  отпечаток {}  ({})", fp.short(), fp.describe());
                if args.source.starts_with("synthetic") {
                    println!("\nЭто синтетический кадр, а не рабочий стол: годится только для");
                    println!("проверки самого стенда.");
                } else {
                    println!("\nЭто кадр настоящего рабочего стола. В git он не попадёт —");
                    println!("расширения .shot, .raw и .bgra в .gitignore. Возите файлом.");
                }
                return Ok(());
            }
            Ok(None) => continue,
            Err(CaptureError::AccessLost) => source.reinit().map_err(|e| e.to_string())?,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("за 10 секунд экран ни разу не изменился, снимать нечего".to_owned())
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
    settings.cpu_used = args.cpu_used;
    settings.threads = args.threads;
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

/// Build one encoder per configuration under comparison.
///
/// All of them are built before the run starts: a configuration libvpx refuses
/// should cost nothing but an error message, not sixty seconds of scrolling
/// followed by one.
#[cfg(feature = "vpx")]
fn build_compare_encoders(
    specs: &[Spec],
    planes: &[I420],
    plane_of: &[usize],
    fps: u32,
) -> Result<Vec<spike_encode::vpx::VpxEncoder>, String> {
    use spike_encode::vpx::{Settings, VpxEncoder};
    specs
        .iter()
        .zip(plane_of)
        .map(|(spec, &pi)| {
            let plane = &planes[pi];
            let mut settings = Settings::new(plane.width, plane.height, fps);
            settings.bitrate_kbps = spec.bitrate_kbps;
            settings.cpu_used = spec.cpu_used;
            settings.threads = spec.threads;
            VpxEncoder::new(spec.codec, settings).map_err(|e| format!("«{}»: {e}", spec.label))
        })
        .collect()
}

#[cfg(not(feature = "vpx"))]
fn build_compare_encoders(
    _specs: &[Spec],
    _planes: &[I420],
    _plane_of: &[usize],
    _fps: u32,
) -> Result<Vec<Never>, String> {
    Err("стенд собран без libvpx, сравнивать кодеры нечем.\n\
         Пересоберите с --features vpx (см. README)"
        .to_owned())
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

    // Parsed before anything is opened: a typo in --compare should cost an error
    // message, not a capture session.
    let specs = match args
        .compare
        .iter()
        .map(|t| Spec::parse(t, &args))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Ошибка: {e}");
            std::process::exit(2);
        }
    };
    if !specs.is_empty() && args.codec != Codec::None {
        eprintln!(
            "Ошибка: --encode и --compare вместе не работают. --compare задаёт\n\
             кодеки сам; --encode здесь означал бы ещё один, невидимый в таблице."
        );
        std::process::exit(2);
    }

    if let Some(path) = args.grab.clone() {
        if let Err(e) = grab(&args, &path) {
            eprintln!("\nОшибка: {e}");
            std::process::exit(1);
        }
        return;
    }

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
    let caveats = source.caveats();
    if !caveats.is_empty() {
        println!();
        for caveat in caveats {
            // A caveat may be several lines. Only the first is marked; the rest
            // line up under it instead of each shouting again.
            for (i, line) in caveat.lines().enumerate() {
                println!("{}{line}", if i == 0 { "  ⚠ " } else { "    " });
            }
        }
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

    // The YUV frame persists between frames for the same reason the BGRA buffer
    // does: conversion only touches what changed, so everything else has to
    // still hold the previous frame. It also fixes the encoded resolution.
    // One conversion buffer per distinct scale. Each persists between frames for
    // the same reason the BGRA buffer does: conversion only touches what
    // changed, so everything else has to still hold the previous frame.
    let mut scales: Vec<u32> = if specs.is_empty() {
        vec![args.scale]
    } else {
        specs.iter().map(|s| s.scale).collect()
    };
    scales.sort_unstable();
    scales.dedup();
    let mut planes: Vec<I420> = scales.iter().map(|&s| I420::new(w, h, s)).collect();
    // Which buffer each configuration reads. Two configurations at the same
    // scale share one conversion: it happens once and is charged to both, which
    // is what each would cost alone rather than what they cost together.
    let plane_of: Vec<usize> = specs
        .iter()
        .map(|s| {
            scales
                .iter()
                .position(|&x| x == s.scale)
                .expect("масштаб взят из этого же списка")
        })
        .collect();
    let mut convert_by_scale = vec![0u64; planes.len()];

    let mut encoder = None;
    let mut encoders = Vec::new();
    let mut track_labels: Vec<(String, u32, u32)> = Vec::new();

    if specs.is_empty() {
        let (ew, eh) = (planes[0].width, planes[0].height);
        encoder = match build_encoder(&args, ew, eh) {
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
                "Кодирование: {} в {ew}×{eh} при {} кбит/с, cpu-used {}, потоков {}.",
                args.codec.name(),
                args.bitrate_kbps,
                args.cpu_used,
                args.threads
            );
        }
        // Outside the codec branch: --scale changes the conversion cost too, and
        // a run that quietly measured a different resolution than the operator
        // thinks is a run whose numbers mean something else.
        if args.scale > 1 {
            println!(
                "Кадр уменьшен в {} раза: {ew}×{eh}, то есть {:.0}% пикселей.",
                args.scale,
                100.0 / (args.scale * args.scale) as f64
            );
        }
    } else {
        if !args.readback.wants_pixels() {
            eprintln!("\nОшибка: сравнивать нечего — копирование пикселей выключено.");
            std::process::exit(2);
        }
        encoders = match build_compare_encoders(&specs, &planes, &plane_of, args.fps) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("\nОшибка: {e}");
                std::process::exit(1);
            }
        };
        println!(
            "Сравнение на одних и тех же кадрах, {}:",
            spike_metrics::plural(specs.len() as u64, "конфигурация", "конфигурации", "конфигураций")
        );
        for (i, spec) in specs.iter().enumerate() {
            let plane = &planes[plane_of[i]];
            println!(
                "  {:<14} {} в {}×{}, {} кбит/с, cpu-used {}, потоков {}",
                spec.label,
                spec.codec.name(),
                plane.width,
                plane.height,
                spec.bitrate_kbps,
                spec.cpu_used,
                spec.threads
            );
            track_labels.push((spec.label.clone(), plane.width, plane.height));
        }
        println!("Порядок кодирования чередуется по кадрам.");
        println!(
            "Частота кадров здесь занижена: на каждый кадр {} подряд.",
            spike_metrics::plural(specs.len() as u64, "кодирование", "кодирования", "кодирований")
        );
    }

    let mut first_frame = true;
    let mut rotate = 0usize;

    let mut rec = Recorder::new(format!("Захват · {}", source.describe()), w, h, args.fps);
    if let Some(why) = source.stand_in() {
        rec.note_stand_in(why);
    }
    for (label, tw, th) in track_labels {
        rec.add_track(label, tw, th);
    }

    // A frame budget makes the run a benchmark: both machines encode the same
    // sequence and the two numbers can be divided. The clock stays as a backstop
    // so a still scenario cannot hang waiting for content that never comes.
    let frame_budget = args.frames.unwrap_or(u64::MAX);
    let mut frames_done = 0u64;
    if let Some(n) = args.frames {
        println!(
            "Остановка после {} с содержимым; {} с — предохранитель.",
            spike_metrics::plural(n, "кадра", "кадров", "кадров"),
            args.seconds
        );
    }
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    // One frame interval, so a still screen blocks instead of spinning.
    let timeout = Duration::from_secs_f64(1.0 / args.fps.max(1) as f64);
    let started = Instant::now();

    'run: while Instant::now() < deadline && frames_done < frame_budget {
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
                    first_frame = false;

                    if specs.is_empty() {
                        let t = Instant::now();
                        planes[0].convert_bgra(bgra, frame.stride, rects);
                        convert_us = t.elapsed().as_micros() as u64;

                        if let Some(enc) = encoder.as_mut() {
                            let t = Instant::now();
                            match encode_one(enc, &planes[0]) {
                                Ok(out) => encoded = Some((t.elapsed().as_micros() as u64, out)),
                                Err(e) => {
                                    eprintln!("Кодирование прекращено: {e}");
                                    break;
                                }
                            }
                        }
                    } else {
                        // Convert once per distinct scale, before any encoding,
                        // so no configuration is charged for another's work.
                        for (i, plane) in planes.iter_mut().enumerate() {
                            let t = Instant::now();
                            plane.convert_bgra(bgra, frame.stride, rects);
                            convert_by_scale[i] = t.elapsed().as_micros() as u64;
                        }

                        // Capture and copy happened once and belong to every
                        // configuration equally.
                        let shared_us = frame.work_us + frame.readback_us;
                        let n = specs.len();
                        for k in 0..n {
                            // Rotate the order every frame. Whoever encodes first
                            // pays for a cold cache and gets the processor before
                            // it has heated up; over a run that would otherwise
                            // always be the same configuration, and the operator
                            // would read it as a property of the codec.
                            let i = (k + rotate) % n;
                            let plane = &planes[plane_of[i]];
                            let t = Instant::now();
                            match encode_one(&mut encoders[i], plane) {
                                Ok(out) => rec.record_track(
                                    i,
                                    &TrackStat {
                                        shared_us,
                                        convert_us: convert_by_scale[plane_of[i]],
                                        encode_us: t.elapsed().as_micros() as u64,
                                        bytes: out.bytes,
                                        keyframe: out.keyframe,
                                    },
                                ),
                                Err(e) => {
                                    eprintln!(
                                        "Кодирование прекращено на «{}»: {e}",
                                        specs[i].label
                                    );
                                    break 'run;
                                }
                            }
                        }
                        rotate += 1;
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
                frames_done += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args::default()
    }

    #[test]
    fn a_bare_codec_takes_everything_from_the_run() {
        let s = Spec::parse("vp9", &args()).expect("vp9 разбирается");
        assert_eq!(s.codec, Codec::Vp9);
        assert_eq!(s.scale, args().scale);
        assert_eq!(s.threads, args().threads);
        assert_eq!(s.cpu_used, args().cpu_used);
        assert_eq!(s.bitrate_kbps, args().bitrate_kbps);
        // The label is what the operator typed, so the table names the run back
        // to them rather than a reconstruction of it.
        assert_eq!(s.label, "vp9");
    }

    #[test]
    fn parts_may_come_in_any_order() {
        let a = Spec::parse("vp8:s2:t4:c9:b1500", &args()).expect("прямой порядок");
        let b = Spec::parse("vp8:b1500:c9:t4:s2", &args()).expect("обратный порядок");
        assert_eq!((a.scale, a.threads, a.cpu_used, a.bitrate_kbps), (2, 4, 9, 1_500));
        assert_eq!((a.scale, a.threads, a.cpu_used, a.bitrate_kbps),
                   (b.scale, b.threads, b.cpu_used, b.bitrate_kbps));
        assert_eq!(a.codec, Codec::Vp8);
    }

    #[test]
    fn a_negative_cpu_used_is_a_real_setting_not_a_typo() {
        let s = Spec::parse("vp9:c-1", &args()).expect("libvpx принимает отрицательные");
        assert_eq!(s.cpu_used, -1);
    }

    #[test]
    fn nonsense_is_refused_rather_than_guessed() {
        for text in ["", "h264", "none", "vp9:s0", "vp9:s9", "vp9:t0", "vp9:z2", "vp9:sдва", "vp9:"] {
            assert!(Spec::parse(text, &args()).is_err(), "«{text}» должно было отвергнуться");
        }
    }

    /// A Cyrillic letter where a Latin tag was meant must produce a message, not
    /// a panic: splitting the part by byte would have cut a two-byte codepoint in
    /// half. Easy to type by accident with a Russian keyboard layout, and `с`
    /// looks exactly like `s`.
    #[test]
    fn a_cyrillic_typo_gets_an_error_and_not_a_panic() {
        assert!(Spec::parse("vp9:с2", &args()).is_err());
        assert!(Spec::parse("vp9:—", &args()).is_err());
        assert!(Spec::parse("вп9", &args()).is_err());
    }

    #[test]
    fn whitespace_around_a_configuration_is_forgiven() {
        let s = Spec::parse("  vp9:s2  ", &args()).expect("пробелы от --compare с пробелами");
        assert_eq!(s.scale, 2);
        assert_eq!(s.label, "vp9:s2");
    }
}
