. "$PSScriptRoot\sweep-common.ps1"

# Диагностика: виновата ли перенесённая шкала квантователя.
#
# Наш потолок 56 по шкале VPx это 45 по шкале H.26x. Арифметика верна, но
# навязывать её чужому рейт-контролю -- не то же самое, что быть честным:
# у libx264 умолчание 51, и запрет уходить грубее 45 может заставить его
# промахнуться мимо битрейта в разы.

$N = 60
$K = 1000
$src = Join-Path $SweepWork "qt-src.y4m"
if (-not (Test-Path $src)) {
  & $SweepBin --source "image:$SweepShot" --motion scroll --step 61 --frames $N --scale 2 --export-yuv $src 2>&1 | Out-Null
}

function Try-One([string]$tag, [string[]]$opts) {
  $f = Join-Path $SweepWork "qt-$tag.mp4"
  & $SweepFfmpeg @(@("-hide_banner","-y","-i",$src) + $opts + @($f)) 2>&1 | Out-Null
  if (-not (Test-Path $f)) { Write-Host "$tag : ffmpeg ничего не написал"; return }
  $c = Get-RealisedConfig $f
  $got = [math]::Round((Get-Item $f).Length / $N * 8 * 30 / 1000, 0)
  Write-Host ("{0,-22} {1,6} кбит/с (цель {2})  кадров {3}  I={4} has_b={5}" -f `
    $tag, $got, $K, $c.frames, $c.keyframes, $c.has_b)
  Remove-Item $f -ErrorAction SilentlyContinue
}

$common = @("-b:v","${K}k","-maxrate","${K}k","-bufsize","500k","-g","300","-bf","0","-threads","2")

Try-One "x264 с qmax 45"   (@("-c:v","libx264") + $common + @("-qmin","3","-qmax","45","-preset","veryfast","-tune","zerolatency"))
Try-One "x264 без qmax"    (@("-c:v","libx264") + $common + @("-preset","veryfast","-tune","zerolatency"))
Try-One "h264qsv с qmax 45" (@("-c:v","h264_qsv") + $common + @("-qmin","3","-qmax","45","-async_depth","1"))
Try-One "h264qsv без qmax"  (@("-c:v","h264_qsv") + $common + @("-async_depth","1"))
Try-One "hevcqsv с qmax 45" (@("-c:v","hevc_qsv") + $common + @("-qmin","3","-qmax","45","-async_depth","1"))
Try-One "hevcqsv без qmax"  (@("-c:v","hevc_qsv") + $common + @("-async_depth","1"))

Remove-Item $src -ErrorAction SilentlyContinue
Write-Host "done"
