. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$src = "$sp\settle-src.y4m"
$dec = "$sp\sdec.y4m"
$N = 300

if (-not (Test-Path $src)) {
  & $spike --source image:$SweepShot --motion settle --frames $N --export-yuv $src 2>&1 | Out-Null
}

function Score($name, $kbps, $file) {
  $got = [math]::Round((Get-Item $file).Length / $N * 8 * 30 / 1000, 0)
  Remove-Item $dec -ErrorAction SilentlyContinue
  & $ff -hide_banner -y -i $file -pix_fmt yuv420p $dec 2>&1 | Out-Null
  $q = & $spike --motion settle --quality $src $dec --quality-csv "$sp\curve-$name-$kbps.csv" 2>&1 | Out-String
  Remove-Item $dec -ErrorAction SilentlyContinue
  Write-Host "===== $name at $got kbps"
  Write-Host $q
}

function FF($name, [string[]]$o, $ext, $kbps) {
  $f = "$sp\s-$name-$kbps.$ext"
  & $ff @(@("-hide_banner","-y","-i",$src) + $o + @($f)) 2>&1 | Out-Null
  if (-not (Test-Path $f)) { Write-Host "$name $kbps FAIL"; return }
  Score $name $kbps $f
  Remove-Item $f -ErrorAction SilentlyContinue
}

foreach ($k in 1000, 2000) {
  FF "hevcqsv" @("-c:v","hevc_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
  FF "h264qsv" @("-c:v","h264_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
  $f = "$sp\s-ours-$k.ivf"
  & $spike --source image:$SweepShot --motion settle --frames $N --encode vp9 --max-q 63 --bitrate $k --emit-ivf $f 2>&1 | Out-Null
  if (Test-Path $f) { Score "ours" $k $f; Remove-Item $f -ErrorAction SilentlyContinue }
}

Write-Host "done"
