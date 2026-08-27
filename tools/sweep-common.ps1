# Общая часть всех замерных скриптов. Подключается точкой:  . "$PSScriptRoot\sweep-common.ps1"
#
# Зачем это существует. Все опубликованные таблицы сравнения кодеков были сняты
# скриптами, лежавшими во временном каталоге сессии, с путями вида
# C:\Users\<имя>\AppData\Local\Temp\... . То есть воспроизвести таблицу не мог
# никто, включая автора после перезагрузки, и одна уборка мусора уничтожила бы
# доказательства целиком. Числа, которые нельзя переснять, -- это не измерения,
# а утверждения.
#
# Настраивается переменными окружения; умолчания рассчитаны на то, что репозиторий
# собран, а ffmpeg распакован рядом.
#
#   SPIKE_WORK    куда класть многогигабайтные y4m и промежуточные файлы.
#                 По умолчанию каталог .sweep в корне репозитория (он в .gitignore).
#   SPIKE_FFMPEG  путь к ffmpeg.exe. По умолчанию ищется в SPIKE_WORK\ff и в PATH.
#   SPIKE_BIN     путь к spike.exe. По умолчанию target\release\spike.exe.
#
# ffmpeg НУЖЕН со сборкой qsv: без неё h264_qsv и hevc_qsv отсутствуют, и сравнение
# молча выродится в две программные строки.

$ErrorActionPreference = "Continue"

$SweepRepo = (Resolve-Path "$PSScriptRoot\..").Path

$SweepWork = if ($env:SPIKE_WORK) { $env:SPIKE_WORK } else { Join-Path $SweepRepo ".sweep" }
if (-not (Test-Path $SweepWork)) { New-Item -ItemType Directory -Force $SweepWork | Out-Null }

$SweepBin = if ($env:SPIKE_BIN) { $env:SPIKE_BIN } else { Join-Path $SweepRepo "target\release\spike.exe" }
if (-not (Test-Path $SweepBin)) {
  throw "не найден $SweepBin. Соберите: cargo build --release --features vpx (нужна VPX_LIB_DIR)"
}

function Find-Ffmpeg([string]$exe) {
  if ($env:SPIKE_FFMPEG) {
    $dir = Split-Path -Parent $env:SPIKE_FFMPEG
    $cand = Join-Path $dir "$exe.exe"
    if (Test-Path $cand) { return $cand }
  }
  $found = Get-ChildItem -Path (Join-Path $SweepWork "ff") -Recurse -Filter "$exe.exe" -ErrorAction SilentlyContinue |
    Select-Object -First 1
  if ($found) { return $found.FullName }
  $onPath = Get-Command $exe -ErrorAction SilentlyContinue
  if ($onPath) { return $onPath.Source }
  throw "не найден $exe. Положите сборку ffmpeg с qsv в $SweepWork\ff или задайте SPIKE_FFMPEG"
}

$SweepFfmpeg = Find-Ffmpeg "ffmpeg"
$SweepFfprobe = Find-Ffmpeg "ffprobe"

Set-Location $SweepRepo

# Снимок, на котором сняты все опубликованные таблицы. Не коммитится -- это кадр
# настоящего рабочего стола, а репозиторий публичный. Снимается через --grab.
$SweepShot = if ($env:SPIKE_SHOT) { $env:SPIKE_SHOT } else { "heavy.shot" }
if (-not (Test-Path $SweepShot)) {
  throw "нет $SweepShot. Снимите: .\target\release\spike.exe --grab heavy.shot"
}

Write-Host "стенд:  $SweepBin"
Write-Host "ffmpeg: $SweepFfmpeg"
Write-Host "работа: $SweepWork"
Write-Host "снимок: $SweepShot"

# Условия, в которых кодировал ffmpeg, прочитанные ИЗ РЕЗУЛЬТАТА, а не из ключей.
#
# Ключ, который драйвер принял и проигнорировал, ключами не ловится: -global_quality
# был молча съеден QSV и дал одинаковые байты в четырёх точках, а -bf 0 не убирает
# B-срезы у hevc_qsv (там они безвредны -- low-delay). Отличает одно: расходятся ли
# DTS с PTS. Возвращает объект с has_b, reorder, keyframes и гистограммой типов.
function Get-RealisedConfig([string]$file) {
  $hb = (& $SweepFfprobe -v error -select_streams v:0 -show_entries stream=has_b_frames -of csv=p=0 $file 2>&1 | Out-String).Trim()
  $types = (& $SweepFfprobe -v error -select_streams v:0 -show_entries frame=pict_type -of csv=p=0 $file 2>&1 | Out-String) -replace "[^IPB]", ""
  $reorder = 0
  foreach ($l in (& $SweepFfprobe -v error -select_streams v:0 -show_entries packet=pts,dts -of csv=p=0 $file 2>&1)) {
    $p = "$l" -split ","
    if ($p.Count -ge 2 -and $p[0] -ne $p[1]) { $reorder++ }
  }
  [pscustomobject]@{
    has_b     = $hb
    reorder   = $reorder
    keyframes = ([regex]::Matches($types, "I")).Count
    p_frames  = ([regex]::Matches($types, "P")).Count
    b_frames  = ([regex]::Matches($types, "B")).Count
    frames    = $types.Length
  }
}

# Отказ вместо числа, когда кодер переупорядочивал кадры.
#
# В сеансе удалённой поддержки следующего кадра ещё не существует, поэтому строка,
# снятая с переупорядочиванием, описывает режим, которого у продукта не будет.
# Все аппаратные строки во всех трёх отчётах были сняты именно так, и это не
# заметили, потому что число выглядело правдоподобно. Проверка стоит одного вызова
# ffprobe -- дешевле, чем ещё раз узнать об этом из отчёта.
# Ключи ffmpeg, ПОРОЖДЁННЫЕ из настроек нашего кодера.
#
# Рукописные ключи в этих скриптах пропустили четыре расхождения подряд, и ни
# одно не было видно в своей половине: ключевой кадр 128 против 300, буфер вдвое
# меньше (у libvpx миллисекунды, у ffmpeg биты), переупорядочивание кадров у QSV
# и экранный режим только у нас. Обе половины по отдельности выглядели разумно и
# вместе были неверны.
#
# Возвращает массив строк для передачи ffmpeg. Оговорки печатаются: то, что
# сопоставить нельзя, не повод молчать об этом -- это и есть блок оговорок,
# который обязана нести таблица.
function Get-PlanArgs([string]$target, [int]$kbps) {
  $out = & $SweepBin --plan $target --bitrate $kbps --encode vp9 2>&1 | Out-String
  $lines = $out -split "`r?`n"
  $argv = $null
  foreach ($l in $lines) {
    if ($l.TrimStart().StartsWith("-c:v")) { $argv = $l.Trim(); break }
  }
  if (-not $argv) { throw "не удалось получить ключи для $target :`n$out" }
  foreach ($l in $lines) { if ($l.StartsWith("#")) { Write-Host "  $l" } }
  return $argv -split " "
}

function Assert-NoReorder([string]$tag, [string]$file) {
  $c = Get-RealisedConfig $file
  if ($c.reorder -gt 0 -or $c.has_b -ne "0") {
    Write-Host "  ОТКАЗ $tag : кодер переупорядочивал кадры (has_b_frames=$($c.has_b), pts<>dts у $($c.reorder)). Нужен -bf 0."
    return $false
  }
  return $true
}

# Заслон по РЕЗУЛЬТАТУ, а не по переданным ключам.
#
# Ключ, который драйвер принял и проигнорировал, ключами не ловится: QSV молча
# съел -global_quality и выдал одинаковые байты в четырёх точках. Отличает
# только чтение самого битстрима. Проверяется переупорядочивание и фактический
# интервал ключевых кадров -- строка с чужим GOP несравнима, сколько бы -g ей
# ни передали.
#
# Возвращает $true, если строку можно печатать в сводку.
function Test-RowIsComparable([string]$tag, [string]$file, [int]$expectedGop, [int]$frames) {
  $c = Get-RealisedConfig $file
  $ok = $true

  if ($c.reorder -gt 0 -or $c.has_b -ne "0") {
    Write-Host "  ОТКАЗ $tag : переупорядочивание (has_b=$($c.has_b), pts<>dts $($c.reorder))"
    $ok = $false
  }

  # Сколько ключевых кадров должно быть при таком интервале.
  $want = [math]::Max(1, [math]::Ceiling($frames / [double]$expectedGop))
  if ($c.keyframes -ne $want) {
    Write-Host "  ОТКАЗ $tag : ключевых кадров $($c.keyframes), ожидалось $want при -g $expectedGop"
    Write-Host "         (у строки VP9 в опубликованной таблице их было 3 вместо 1: ключ -g просто не передали)"
    $ok = $false
  }

  if ($c.frames -ne $frames) {
    Write-Host "  ОТКАЗ $tag : кадров $($c.frames), ожидалось $frames"
    $ok = $false
  }

  if ($ok) {
    Write-Host "  $tag : I=$($c.keyframes) P=$($c.p_frames) B=$($c.b_frames) has_b=$($c.has_b) reorder=$($c.reorder)"
  }
  return $ok
}
