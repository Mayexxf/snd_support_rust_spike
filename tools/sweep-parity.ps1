. "$PSScriptRoot\sweep-common.ps1"

# Проверка самой машинерии паритета, не кодеков.
#
# Прогоняет каждую цель на порождённых ключах и требует, чтобы результат прошёл
# заслон: без переупорядочивания и с тем интервалом ключевых кадров, который
# задан. Если эта проверка не проходит, ни одна строка сводки не значит ничего.

$N = 60
$K = 2000
$src = Join-Path $SweepWork "parity-src.y4m"

if (-not (Test-Path $src)) {
  Write-Host "готовлю $src …"
  & $SweepBin --source "image:$SweepShot" --motion scroll --frames $N --scale 4 --export-yuv $src 2>&1 | Out-Null
}
if (-not (Test-Path $src)) { throw "не удалось подготовить $src" }

# Интервал ключевых кадров берётся оттуда же, откуда его берёт кодер.
$plan = & $SweepBin --plan libvpx-vp9 --bitrate $K --encode vp9 2>&1 | Out-String
$gop = if ($plan -match "-g (\d+)") { [int]$Matches[1] } else { throw "в плане нет -g" }
Write-Host "интервал ключевых кадров из настроек кодера: $gop`n"

$rows = 0
$good = 0
foreach ($t in @(
    @("libx264", "mp4"),
    @("h264_qsv", "mp4"),
    @("hevc_qsv", "mp4"),
    @("libvpx-vp9", "webm")
  )) {
  $name = $t[0]
  Write-Host "=== $name"
  $opts = Get-PlanArgs $name $K
  $f = Join-Path $SweepWork "parity-$($name -replace '[^a-z0-9]','_').$($t[1])"
  & $SweepFfmpeg @(@("-hide_banner", "-y", "-i", $src) + $opts + @($f)) 2>&1 | Out-Null
  $rows++
  if (-not (Test-Path $f)) { Write-Host "  ОТКАЗ $name : ffmpeg ничего не написал"; continue }
  if (Test-RowIsComparable $name $f $gop $N) { $good++ }
  Remove-Item $f -ErrorAction SilentlyContinue
}

Remove-Item $src -ErrorAction SilentlyContinue
Write-Host "`nстрок пригодных к сравнению: $good из $rows"
if ($good -ne $rows) { Write-Host "СВОДКУ СНИМАТЬ НЕЛЬЗЯ, пока не пройдут все" }
Write-Host "done"
