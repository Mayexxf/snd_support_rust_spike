. "$PSScriptRoot\sweep-common.ps1"

# Разбор единственной аномалии, оставшейся необъяснённой.
#
# h264_qsv с -bf 0 при низкой цели проваливается в разы: сначала 802-1323 при
# цели 2000, потом 58 кбит/с при цели 1000 -- промах в семнадцать раз. Без -bf 0
# тот же кодер в ту же цель попадает. Но -bf 0 обязателен: без него он
# переупорядочивает кадры, а сеансу поддержки это недоступно.
#
# Проверяются два подозрения по очереди, а не оба сразу:
#   1. дело в самом -bf 0 или в его сочетании с низким битрейтом;
#   2. дело в размере буфера (-bufsize 500 мс) при низком битрейте.
#
# Ничего не утверждается заранее: печатается достигнутый битрейт и структура
# потока, вывод делается по таблице.

$N = 120
$src = Join-Path $SweepWork "qd-src.y4m"
if (-not (Test-Path $src)) {
  & $SweepBin --source "image:$SweepShot" --motion scroll --step 61 --frames $N --export-yuv $src 2>&1 | Out-Null
}
if (-not (Test-Path $src)) { throw "не удалось подготовить $src" }

function Probe([string]$tag, [string[]]$opts, [int]$target) {
  $f = Join-Path $SweepWork "qd.mp4"
  Remove-Item $f -ErrorAction SilentlyContinue
  & $SweepFfmpeg @(@("-hide_banner", "-y", "-i", $src) + $opts + @($f)) 2>&1 | Out-Null
  if (-not (Test-Path $f)) { Write-Host ("{0,-34} ffmpeg ничего не написал" -f $tag); return }
  $c = Get-RealisedConfig $f
  $got = [math]::Round((Get-Item $f).Length / $N * 8 * 30 / 1000, 0)
  $ratio = if ($target -gt 0) { $got / $target } else { 0 }
  Write-Host ("{0,-34} {1,6} кбит/с (цель {2,5}, x{3:N2})  кадров {4}  I={5} B={6} has_b={7}" -f `
    $tag, $got, $target, $ratio, $c.frames, $c.keyframes, $c.b_frames, $c.has_b)
  Remove-Item $f -ErrorAction SilentlyContinue
}

Write-Host "`n--- h264_qsv: -bf 0 против умолчания, по битрейтам"
foreach ($k in 500, 1000, 2000, 4000) {
  $base = @("-c:v", "h264_qsv", "-b:v", "${k}k", "-maxrate", "${k}k",
    "-bufsize", "$([int]($k/2))k", "-g", "300", "-async_depth", "1")
  Probe "bf=0    цель $k" ($base + @("-bf", "0")) $k
  Probe "умолчание цель $k" $base $k
}

Write-Host "`n--- h264_qsv с -bf 0: размер буфера при цели 1000"
foreach ($buf in 250, 500, 1000, 2000) {
  $o = @("-c:v", "h264_qsv", "-b:v", "1000k", "-maxrate", "1000k",
    "-bufsize", "${buf}k", "-g", "300", "-async_depth", "1", "-bf", "0")
  Probe "буфер ${buf}k" $o 1000
}

Write-Host "`n--- для сравнения: hevc_qsv с -bf 0 по тем же битрейтам"
foreach ($k in 500, 1000, 2000) {
  $o = @("-c:v", "hevc_qsv", "-b:v", "${k}k", "-maxrate", "${k}k",
    "-bufsize", "$([int]($k/2))k", "-g", "300", "-async_depth", "1", "-bf", "0")
  Probe "hevc bf=0 цель $k" $o $k
}

Remove-Item $src -ErrorAction SilentlyContinue
Write-Host "done"
