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

/// Only the picture dump uses it, and that needs a decoder.
#[cfg(feature = "vpx")]
mod bmp;

/// Frames written out for an encoder that is not ours. Needs no codec of its
/// own — the conversion to I420 happens either way.
mod yuv;

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
                    None => "разрешение таймера поднять не удалось — ожидания квантуются шагом 15,6 мс, частота кадров занижена"
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
    /// Encode, decode and write three pictures under this prefix, then stop.
    dump: Option<PathBuf>,
    /// Withhold this frame from the decoder, to see what a lost packet costs.
    drop_frame: Option<u64>,
    /// Write the frames the encoder would see to a file, and stop.
    export: Option<PathBuf>,
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
    /// Tile columns in log2 units, VP9 only.
    tile_columns: u32,
    /// Row-level multithreading, VP9 only. Off is libvpx's own default.
    row_mt: bool,
    /// How different a block must be before the encoder looks at it again.
    static_threshold: u32,
    /// Kept as text for the same reason `motion` is: [`spike_encode::vpx`] only
    /// exists when the harness is built with libvpx, and the argument has to
    /// parse either way.
    rc_mode: String,
    min_quantizer: u32,
    max_quantizer: u32,
    error_resilient: bool,
    cq_level: u32,
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
            dump: None,
            drop_frame: None,
            export: None,
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
            // Repeat what the encoder would have done unasked, so that a run
            // with none of these keys given is the same run as before they
            // existed. See `Settings::new` for which is whose default.
            tile_columns: 6,
            row_mt: false,
            static_threshold: 0,
            rc_mode: "cbr".to_owned(),
            min_quantizer: 4,
            max_quantizer: 56,
            error_resilient: false,
            cq_level: 10,
            compare: Vec::new(),
        }
    }
}

/// One encoder configuration in a comparison run.
///
/// Written as `vp9:s2:t4:c9:b1500` — the codec first, then any of scale (`s`),
/// threads (`t`), cpu-used (`c`), bitrate (`b`), tile columns (`k`), row-mt
/// (`r`), static threshold (`n`), minimum quantizer (`q`), maximum quantizer
/// (`m`), loss resilience (`e`), rate control (`u`) and CQ level (`l`), in any
/// order. Anything left out falls back to the
/// run-wide key of the same name, so the common case stays short:
/// `--compare vp9,vp8` compares two codecs at the same settings.
#[derive(Debug, Clone)]
struct Spec {
    label: String,
    codec: Codec,
    scale: u32,
    threads: u32,
    cpu_used: i32,
    bitrate_kbps: u32,
    tile_columns: u32,
    row_mt: bool,
    static_threshold: u32,
    rc_mode: String,
    min_quantizer: u32,
    max_quantizer: u32,
    error_resilient: bool,
    cq_level: u32,
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
            tile_columns: args.tile_columns,
            row_mt: args.row_mt,
            static_threshold: args.static_threshold,
            rc_mode: args.rc_mode.clone(),
            min_quantizer: args.min_quantizer,
            max_quantizer: args.max_quantizer,
            error_resilient: args.error_resilient,
            cq_level: args.cq_level,
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
                'k' => {
                    let n = number("столбцы плиток k")?;
                    if !(0..=6).contains(&n) {
                        return Err(format!("«{text}»: столбцы плиток вне диапазона 0..6"));
                    }
                    spec.tile_columns = n as u32;
                }
                'r' => {
                    let n = number("row-mt r")?;
                    if !(0..=1).contains(&n) {
                        return Err(format!("«{text}»: row-mt — это 0 или 1, а не {n}"));
                    }
                    spec.row_mt = n == 1;
                }
                'n' => {
                    let n = number("порог покоя n")?;
                    if !(0..=100_000).contains(&n) {
                        return Err(format!("«{text}»: порог покоя вне диапазона 0..100000"));
                    }
                    spec.static_threshold = n as u32;
                }
                'q' => {
                    let n = number("минимальный квантователь q")?;
                    if !(0..=63).contains(&n) {
                        return Err(format!("«{text}»: квантователь вне диапазона 0..63"));
                    }
                    spec.min_quantizer = n as u32;
                }
                'm' => {
                    let n = number("максимальный квантователь m")?;
                    if !(0..=63).contains(&n) {
                        return Err(format!("«{text}»: квантователь вне диапазона 0..63"));
                    }
                    spec.max_quantizer = n as u32;
                }
                'e' => {
                    let n = number("устойчивость к потерям e")?;
                    if !(0..=1).contains(&n) {
                        return Err(format!(
                            "«{text}»: устойчивость к потерям — это 0 или 1, а не {n}"
                        ));
                    }
                    spec.error_resilient = n == 1;
                }
                'l' => {
                    let n = number("уровень CQ l")?;
                    if !(0..=63).contains(&n) {
                        return Err(format!("«{text}»: уровень CQ вне диапазона 0..63"));
                    }
                    spec.cq_level = n as u32;
                }
                // The one part that is a word rather than a number: writing the
                // rate control mode as u0/u1/u2 would put the least memorable
                // thing in the run into the label of every row of the table.
                'u' => {
                    if !matches!(value, "cbr" | "vbr" | "cq") {
                        return Err(format!(
                            "«{text}»: режим рейт-контроля — cbr, vbr или cq, а не «{value}»"
                        ));
                    }
                    spec.rc_mode = value.to_owned();
                }
                other => {
                    return Err(format!(
                        "«{text}»: непонятная часть «{part}». Ожидались s (масштаб), \
                         t (потоки), c (cpu-used), b (битрейт), k (столбцы плиток), \
                         r (row-mt), n (порог покоя), q (мин. квантователь), \
                         m (макс. квантователь), e (устойчивость к потерям), \
                         u (рейт-контроль), l (уровень CQ), а не «{other}»"
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
    --dump <префикс>                   закодировать, декодировать и записать три
                                       картинки: -src (источник), -enc (что
                                       отдали кодеру) и -dec (что вернул
                                       декодер). Не замер: отвечает на вопрос
                                       «читается ли текст», на который таблицы
                                       не отвечают
    --step <N>                         шаг прокрутки снимка, пикселей за кадр
    --frames <N>                       остановиться после N кадров с содержимым.
                                       Для сравнения машин между собой: обе должны
                                       закодировать одну и ту же последовательность.
                                       Со снимком отключает выдержку темпа
    --seconds <N>                      длительность прогона (по умолчанию 30)
    --fps <N>                          целевая частота (по умолчанию 30)
    --size <ШxВ>                       размер для синтетического источника
    --readback <dirty|full|compare|buffered|off>  что копировать из памяти GPU:
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
    --row-mt <0|1>                     построчная многопоточность VP9, по
                                       умолчанию 0 — как в libvpx. Без неё
                                       потокам достаются только столбцы плиток,
                                       а их на 960 пикселях ширины два
    --tile-columns <0..6>              столбцов плиток, в log2: 0 — один, 1 —
                                       два. По умолчанию 6, как в libvpx: это
                                       «сколько разрешит ширина», то есть рычаг
                                       работает вниз, а не вверх
    --static-threshold <N>             насколько блок должен измениться, чтобы
                                       его кодировали заново. 0 (умолчание)
                                       смотрит все блоки на каждом кадре
    --rc <cbr|vbr|cq>                  режим рейт-контроля, по умолчанию cbr
    --min-q <0..63>                    нижняя граница квантователя, по умолчанию 4
    --max-q <0..63>                    верхняя граница, по умолчанию 56. Упирается
                                       раньше нижней: при масштабе 1 кодер достаёт
                                       до потолка и промахивается мимо битрейта
                                       вместо того, чтобы сжать сильнее
    --export-yuv <файл>                записать кадры, которые получил бы кодер,
                                       и выйти. Формат по расширению: .y4m несёт
                                       размер и частоту в заголовке, .nv12 сырой
                                       и нужен аппаратному пути, чтобы в замер не
                                       вошла чужая перепаковка плоскостей.
                                       Только с воспроизводимым источником
    --drop-frame <N>                   не отдать декодеру кадр N. Только с --dump:
                                       показывает, что видит получатель после
                                       потерянного пакета и надолго ли
    --error-resilient <0|1>            пережить потерю кадра, по умолчанию 0.
                                       Ноль был зашит и не назывался выбором:
                                       без этого одна потеря даёт кашу до
                                       следующего ключевого кадра
    --cq-level <0..63>                 цель качества для --rc cq
    --compare <конф,конф,...>          мерить несколько конфигураций кодера на
                                       ОДНИХ И ТЕХ ЖЕ кадрах, в одном прогоне.
                                       Конфигурация: кодек[:sМасштаб][:tПотоки]
                                       [:cCpuUsed][:bБитрейт][:kПлитки][:rRowMt]
                                       [:nПорог][:qМинQ][:uРежим][:lУровеньCQ],
                                       опущенное берётся из ключей выше. Пример:
                                         --compare vp9:s2:t2,vp9:s2:t4,vp8:s2
                                         --compare vp9:s2:t4,vp9:s2:t4:r1
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
            "--dump" => args.dump = Some(PathBuf::from(value()?)),
            "--export-yuv" => args.export = Some(PathBuf::from(value()?)),
            "--drop-frame" => {
                let n: u64 = value()?.parse().map_err(|_| "--drop-frame ждёт число")?;
                args.drop_frame = Some(n);
            }
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
            "--tile-columns" => {
                args.tile_columns =
                    value()?.parse().map_err(|_| "--tile-columns ждёт число")?;
                if args.tile_columns > 6 {
                    return Err("--tile-columns вне диапазона 0..6".to_owned());
                }
            }
            "--row-mt" => {
                let v = value()?;
                args.row_mt = match v.as_str() {
                    "1" | "on" | "да" => true,
                    "0" | "off" | "нет" => false,
                    other => return Err(format!("--row-mt ждёт 0 или 1, а не {other}")),
                };
            }
            "--static-threshold" => {
                args.static_threshold =
                    value()?.parse().map_err(|_| "--static-threshold ждёт число")?;
            }
            "--error-resilient" => {
                args.error_resilient = match value()?.as_str() {
                    "1" | "on" => true,
                    "0" | "off" => false,
                    other => {
                        return Err(format!("--error-resilient ждёт 0 или 1, а не «{other}»"));
                    }
                };
            }
            "--max-q" => {
                args.max_quantizer = value()?.parse().map_err(|_| "--max-q ждёт число")?;
                if args.max_quantizer > 63 {
                    return Err("--max-q вне диапазона 0..63".to_owned());
                }
            }
            "--min-q" => {
                args.min_quantizer = value()?.parse().map_err(|_| "--min-q ждёт число")?;
                if args.min_quantizer > 63 {
                    return Err("--min-q вне диапазона 0..63".to_owned());
                }
            }
            "--cq-level" => {
                args.cq_level = value()?.parse().map_err(|_| "--cq-level ждёт число")?;
                if args.cq_level > 63 {
                    return Err("--cq-level вне диапазона 0..63".to_owned());
                }
            }
            // Checked here rather than at the encoder so a typo costs an error
            // message instead of a run. The list is repeated from `RcMode`
            // because that type only exists in a build with libvpx.
            "--rc" => {
                let v = value()?;
                if !matches!(v.as_str(), "cbr" | "vbr" | "cq") {
                    return Err(format!("--rc ждёт cbr, vbr или cq, а не {v}"));
                }
                args.rc_mode = v;
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
/// Write out the frames the encoder would have been handed, and stop.
///
/// Exists so the codec choice can be settled against encoders this harness does
/// not contain. Writing our own FFI to the hardware path is days before the
/// first number; handing the same frames to a tool that already speaks to it is
/// an afternoon — but only if they really are the same frames, which is why the
/// conversion here is the same `I420` the encoder gets, built the same way, and
/// why a fingerprint over every emitted byte is printed at the end.
///
/// Refuses a live source, and that refusal is the point rather than a
/// limitation. A second pass over `dda` captures a different desktop, so the
/// exported file would not hold the frames any measured run encoded, and every
/// comparison built on it would be quietly about two different things. A
/// screenshot source is deterministic by frame number, so a separate pass gives
/// back the same planes to the byte.
fn export(args: &Args, path: &Path) -> Result<(), String> {
    let layout = match path.extension().and_then(|e| e.to_str()) {
        Some(e) if yuv::Layout::parse(e).is_some() => yuv::Layout::parse(e).unwrap(),
        Some(e) => {
            return Err(format!(
                "по расширению «{e}» не понять формат. Назовите файл .y4m или .nv12"
            ));
        }
        None => return Err("у файла нет расширения — назовите его .y4m или .nv12".to_owned()),
    };

    if !args.source.starts_with("image:") && !args.source.starts_with("synthetic") {
        return Err(format!(
            "экспорт идёт только с воспроизводимого источника, а «{}» таким не является.\n\
             Живой экран во втором проходе даст другие кадры, и сравнивать по ним\n\
             будет нечего. Снимите снимок через --grab и экспортируйте с него.",
            args.source
        ));
    }

    let mut source = build_source(args)?;
    let (w, h) = source.dimensions();
    let mut plane = I420::new(w, h, args.scale);
    let want = args.frames.unwrap_or(300).max(1);

    println!("Источник: {}", source.describe());
    println!(
        "Пишу {want} кадров {}×{} в {} ({}) …",
        plane.width,
        plane.height,
        path.display(),
        layout.name()
    );

    let mut writer = yuv::Writer::create(path, layout, plane.width, plane.height, args.fps)?;
    let mut done = 0u64;
    let deadline = Instant::now() + Duration::from_secs(300);

    while done < want && Instant::now() < deadline {
        let grabbed = match source.next_frame(Duration::from_millis(200), Readback::Full) {
            Ok(Some(f)) => f.bgra.map(|b| (f.width, f.height, f.stride, b.to_vec())),
            Ok(None) => None,
            Err(CaptureError::AccessLost) => {
                source.reinit().map_err(|e| e.to_string())?;
                None
            }
            Err(e) => return Err(e.to_string()),
        };
        let Some((fw, fh, stride, bgra)) = grabbed else { continue };

        // Whole-frame conversion, not the dirty regions: the file has to hold a
        // complete picture per frame however little of it moved. The measured
        // path converts only what changed, which is right there and wrong here.
        let whole = Rect { left: 0, top: 0, right: fw as i32, bottom: fh as i32 };
        plane.convert_bgra(&bgra, stride, std::slice::from_ref(&whole));
        writer.frame(&plane)?;
        done += 1;
    }

    if done < want {
        return Err(format!("успел записать только {done} кадров из {want}"));
    }

    let (frames, bytes, fingerprint) = writer.finish()?;
    println!("\nЗаписано: {frames} кадров, {} МБ", bytes / 1_048_576);
    println!("  отпечаток кадров {fingerprint}");
    println!("\nЭто ровно те плоскости, которые получил бы наш кодер при --scale {}.", args.scale);
    println!("Отпечаток печатается для того, чтобы это можно было проверить, а не");
    println!("принимать на слово: два экспорта одних и тех же кадров дают его же.");
    Ok(())
}

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
    use spike_encode::vpx::VpxEncoder;
    if args.codec == Codec::None {
        return Ok(None);
    }
    let settings = encoder_settings(args, width, height)?;
    VpxEncoder::new(args.codec, settings).map(Some)
}

/// Every run-wide encoder key in one place.
///
/// Two callers build an encoder from `Args` — the measured run and `--dump` —
/// and a knob wired into one but not the other would mean looking at a picture
/// that no measurement produced.
#[cfg(feature = "vpx")]
fn encoder_settings(
    args: &Args,
    width: u32,
    height: u32,
) -> Result<spike_encode::vpx::Settings, String> {
    use spike_encode::vpx::{RcMode, Settings};
    let mut settings = Settings::new(width, height, args.fps);
    settings.bitrate_kbps = args.bitrate_kbps;
    settings.cpu_used = args.cpu_used;
    settings.threads = args.threads;
    settings.tile_columns = args.tile_columns;
    settings.row_mt = args.row_mt;
    settings.static_threshold = args.static_threshold;
    settings.min_quantizer = args.min_quantizer;
    settings.max_quantizer = args.max_quantizer;
    settings.error_resilient = args.error_resilient;
    settings.cq_level = args.cq_level;
    settings.rc_mode = RcMode::parse(&args.rc_mode)
        .ok_or(format!("неизвестный режим рейт-контроля: {}", args.rc_mode))?;
    Ok(settings)
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

/// Encode a few frames, decode them back, and write out three pictures.
///
/// Answers the one question the whole harness is built not to ask: is the text
/// still readable. Everything else here prices an image; for most of phase 0
/// nobody had seen one, and the lever that made every number look better —
/// giving the encoder fewer pixels — is precisely the one that destroys what
/// the operator on the other end is trying to read.
///
/// Three files, because the pipeline inflicts two separate losses and blaming
/// the wrong one would send the work in the wrong direction:
///
///   `-src`  the source frame, untouched
///   `-enc`  what the encoder was handed, so the downscale alone
///   `-dec`  what came back out, so the downscale and the quantiser together
///
/// Nothing here is timed, and nothing here runs during a measurement.
#[cfg(feature = "vpx")]
fn dump(args: &Args, prefix: &Path) -> Result<(), String> {
    use spike_encode::vpx::VpxEncoder;
    use spike_encode::vpx::decode::VpxDecoder;

    let codec = if args.codec == Codec::None { Codec::Vp9 } else { args.codec };
    let mut source = build_source(args)?;
    println!("Источник: {}", source.describe());

    // Past the keyframe that opens the stream, deliberately. A keyframe is the
    // one frame the encoder never skimps on, and judging legibility by it would
    // flatter every setting in this harness.
    let want = args.frames.unwrap_or(30).max(2);
    println!(
        "Кодирую {want} кадров: {} при масштабе {}, {} кбит/с, cpu-used {} …",
        codec.name(),
        args.scale,
        args.bitrate_kbps,
        args.cpu_used
    );

    let mut plane: Option<I420> = None;
    let mut encoder: Option<VpxEncoder> = None;
    let mut decoder = VpxDecoder::new(codec)?;
    let mut decoded: Option<spike_encode::vpx::decode::Decoded> = None;
    let mut source_frame: Option<(u32, u32, usize, Vec<u8>)> = None;
    let mut payload: Vec<u8> = Vec::new();
    let mut last = None;
    let mut done = 0u64;
    let mut decode_failures = 0u64;

    let deadline = Instant::now() + Duration::from_secs(120);
    while done < want && Instant::now() < deadline {
        // Copy the pixels out and let the borrow on the source end here. This
        // path is not timed, and the alternative is a borrow dance for nothing.
        let grabbed = match source.next_frame(Duration::from_millis(200), Readback::Full) {
            Ok(Some(f)) => f.bgra.map(|b| (f.width, f.height, f.stride, b.to_vec())),
            Ok(None) => None,
            Err(CaptureError::AccessLost) => {
                source.reinit().map_err(|e| e.to_string())?;
                None
            }
            Err(e) => return Err(e.to_string()),
        };
        let Some((w, h, stride, bgra)) = grabbed else { continue };

        if plane.is_none() {
            let p = I420::new(w, h, args.scale);
            let settings = encoder_settings(args, p.width, p.height)?;
            encoder = Some(VpxEncoder::new(codec, settings)?);
            plane = Some(p);
        }
        let (Some(p), Some(enc)) = (plane.as_mut(), encoder.as_mut()) else { continue };

        let whole = Rect { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        p.convert_bgra(&bgra, stride, std::slice::from_ref(&whole));

        payload.clear();
        let out = enc.encode_keeping(p, &mut payload)?;
        // A lost packet, simulated the only honest way available here: the
        // encoder is not told, the decoder simply never sees this frame. That
        // is what a drop on the wire looks like from both ends.
        //
        // What happens next is the whole question. Without error resilience VP9
        // keeps adapting its entropy contexts from the last frame it decoded, so
        // the two sides part company here and stay parted until a keyframe. With
        // it, they should converge again. Neither had ever been looked at.
        let lost = args.drop_frame == Some(done);
        if lost {
            println!("  кадр {done} потерян — декодеру не отдан");
        }
        if !payload.is_empty() && !lost {
            // A decode failure is reported and stepped over rather than ending
            // the run. A receiver does not get to stop: the next frame arrives
            // whether or not the last one made sense, and the question this mode
            // exists to answer is how many frames it takes to make sense again.
            //
            // Measured: after one withheld frame, a stream encoded without
            // error resilience fails here on every single frame that follows,
            // to the end of the run. It is not a degraded picture, it is a dead
            // stream.
            match decoder.decode(&payload) {
                Ok(Some(d)) => decoded = Some(d),
                Ok(None) => {}
                Err(e) => {
                    decode_failures += 1;
                    if decode_failures == 1 {
                        println!("  кадр {done}: {e}");
                    }
                }
            }
        }
        last = Some(out);
        source_frame = Some((w, h, stride, bgra));
        done += 1;
    }

    if decode_failures > 0 {
        println!(
            "  декодер не справился с {decode_failures} кадрами из {done} после потери — \
             то есть поток не восстановился сам"
        );
    } else if args.drop_frame.is_some() {
        println!("  после потери декодер справился со всеми остальными кадрами");
    }

    let Some((w, h, stride, bgra)) = source_frame else {
        return Err("ни одного кадра с пикселями за 120 секунд".to_owned());
    };
    let plane = plane.ok_or("кадр не сконвертирован")?;
    let decoded = decoded.ok_or("декодер не отдал ни одного кадра")?;
    let last = last.ok_or("кодер не отдал ни одного кадра")?;

    let name = |suffix: &str| -> PathBuf {
        let mut p = prefix.as_os_str().to_owned();
        p.push(suffix);
        PathBuf::from(p)
    };

    let src = name("-src.bmp");
    bmp::write(&src, w, h, &bmp::from_bgra(w, h, stride, &bgra))?;
    let enc_path = name("-enc.bmp");
    bmp::write(
        &enc_path,
        plane.width,
        plane.height,
        &bmp::from_i420(plane.width, plane.height, &plane.y, &plane.u, &plane.v),
    )?;
    let dec_path = name("-dec.bmp");
    bmp::write(
        &dec_path,
        decoded.width,
        decoded.height,
        &bmp::from_i420(decoded.width, decoded.height, &decoded.y, &decoded.u, &decoded.v),
    )?;

    println!("\nКадров закодировано: {done}, последний — {} Б{}", last.bytes, if last.keyframe { ", ключевой" } else { "" });
    println!("  {}  источник {w}×{h}", src.display());
    println!("  {}  вход кодера {}×{}", enc_path.display(), plane.width, plane.height);
    println!("  {}  выход декодера {}×{}", dec_path.display(), decoded.width, decoded.height);
    println!("\nСмотреть глазами. Стенд может сказать, сколько это стоит,");
    println!("и не может сказать, читается ли это.");
    Ok(())
}

#[cfg(not(feature = "vpx"))]
fn dump(_args: &Args, _prefix: &Path) -> Result<(), String> {
    Err("стенд собран без libvpx, декодировать нечем. Пересоберите с --features vpx".to_owned())
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
    use spike_encode::vpx::{RcMode, Settings, VpxEncoder};
    specs
        .iter()
        .zip(plane_of)
        .map(|(spec, &pi)| {
            let plane = &planes[pi];
            let mut settings = Settings::new(plane.width, plane.height, fps);
            settings.bitrate_kbps = spec.bitrate_kbps;
            settings.cpu_used = spec.cpu_used;
            settings.threads = spec.threads;
            settings.tile_columns = spec.tile_columns;
            settings.row_mt = spec.row_mt;
            settings.static_threshold = spec.static_threshold;
            settings.min_quantizer = spec.min_quantizer;
            settings.max_quantizer = spec.max_quantizer;
            settings.error_resilient = spec.error_resilient;
            settings.cq_level = spec.cq_level;
            settings.rc_mode = RcMode::parse(&spec.rc_mode)
                .ok_or(format!("«{}»: неизвестный режим {}", spec.label, spec.rc_mode))?;
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

    if let Some(path) = args.export.clone() {
        if let Err(e) = export(&args, &path) {
            eprintln!("\nОшибка: {e}");
            std::process::exit(1);
        }
        return;
    }

    if let Some(prefix) = args.dump.clone() {
        if let Err(e) = dump(&args, &prefix) {
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
            Readback::Buffered =>
                "только изменившиеся области, чтение отстаёт на кадр (конвейер)",
        }
    );
    if args.readback == Readback::Buffered {
        println!("Пиксели здесь на кадр старше кадра, который их принёс: путь начинает");
        println!("копию и читает предыдущую. Сравнивать с dirty — отдельным прогоном при");
        println!("том же --frames и том же источнике; на одном кадре эти два не сравнить.");
    }
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
            // Only settings that were actually turned appear. Printing all six
            // on every row would bury the one that differs, and the one that
            // differs is the entire point of the table.
            let mut extra = String::new();
            if spec.row_mt {
                extra.push_str(", row-mt");
            }
            if spec.tile_columns != 6 {
                extra.push_str(&format!(", плиток log2 {}", spec.tile_columns));
            }
            if spec.static_threshold != 0 {
                extra.push_str(&format!(", порог покоя {}", spec.static_threshold));
            }
            if spec.rc_mode != "cbr" {
                extra.push_str(&format!(", {}", spec.rc_mode));
                if spec.rc_mode == "cq" {
                    extra.push_str(&format!(" на {}", spec.cq_level));
                }
            }
            if spec.min_quantizer != 4 {
                extra.push_str(&format!(", мин. q {}", spec.min_quantizer));
            }
            if spec.max_quantizer != 56 {
                extra.push_str(&format!(", макс. q {}", spec.max_quantizer));
            }
            if spec.error_resilient {
                extra.push_str(", устойчив к потерям");
            }
            println!(
                "  {:<14} {} в {}×{}, {} кбит/с, cpu-used {}, потоков {}{}",
                spec.label,
                spec.codec.name(),
                plane.width,
                plane.height,
                spec.bitrate_kbps,
                spec.cpu_used,
                spec.threads,
                extra
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
    // `--frames` over a screenshot drops the pacing on purpose (see `build_image`),
    // so the wall clock stops being the timeline the stream would occupy. Only the
    // bitrate depends on that, and it depended on it silently until now.
    //
    // The condition mirrors `build_image` exactly rather than asking the source,
    // because the synthetic source is also a stand-in and is paced regardless of
    // `--frames`: gating on `stand_in()` would mark it unpaced and quietly move
    // its bitrate instead.
    if args.frames.is_some() && args.source.starts_with("image:") {
        rec.note_unpaced();
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
                        //
                        // Rotated for the same reason the encoding loop below is
                        // rotated, and it was not: whoever converts first reads a
                        // cold BGRA frame and every later scale finds it in cache.
                        // The scales are sorted ascending, so without this the
                        // largest one paid that cost on every frame of every run —
                        // a fixed bias pointing straight at the conclusion this
                        // table exists to test, that downscaling is worth it.
                        //
                        // Rotated over its own length rather than sharing the
                        // encoder's counter, so a configuration is not first at
                        // both stages on the same frames.
                        let np = planes.len();
                        for k in 0..np {
                            let i = (k + rotate) % np;
                            let t = Instant::now();
                            planes[i].convert_bgra(bgra, frame.stride, rects);
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
                                        dropped: out.dropped,
                                        quantizer: out.quantizer,
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
                    encode_dropped: encoded.is_some_and(|(_, out)| out.dropped),
                    quantizer: encoded.and_then(|(_, out)| out.quantizer),
                    // The whole iteration, wait included. Already measured and
                    // until now thrown away on every frame that carried content:
                    // `t0` was read only on the still branch. `ИТОГО` is the sum
                    // of four stage timers, so everything between them — reading
                    // the dirty list, building this struct, the loop itself —
                    // fell outside it, one-sidedly, making every verdict softer
                    // than the truth. Reported so the gap is visible instead of
                    // assumed small.
                    iter_us: t0.elapsed().as_micros() as u64,
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

    // Asked at the end because these polls never reach the recorder: they are
    // the ones the loop threw away as "no new frame", which is how a moving
    // cursor came to be counted as a still screen.
    rec.note_pointer_only(source.pointer_only_polls());
    let (moved_px, moved_rects) = source.moved_pixels();
    rec.note_moved(moved_px, moved_rects);
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
