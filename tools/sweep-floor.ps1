. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$src = "$sp\src.y4m"
$dec = "$sp\fdec.y4m"
$out = "$sp\floor-results.csv"
$N = 300

"codec,target_kbps,got_kbps,p50_24,p95_24,ssim" | Out-File -FilePath $out -Encoding utf8

function Score($name, $kbps, $file) {
  $got = [math]::Round((Get-Item $file).Length / $N * 8 * 30 / 1000, 0)
  $sr = & $ff -hide_banner -i $file -i $src -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | Out-String
  $ss = if ($sr -match "All:([0-9.]+)") { $Matches[1] } else { "?" }

  Remove-Item $dec -ErrorAction SilentlyContinue
  & $ff -hide_banner -y -i $file -pix_fmt yuv420p $dec 2>&1 | Out-Null
  $q = & $spike --quality $src $dec 2>&1 | Out-String
  $m = "?"; $p = "?"
  foreach ($x in [regex]::Matches($q, '>(\d+)\s+([\d.]+)\s+([\d.]+)')) {
    if ($x.Groups[1].Value -eq "24") { $m = $x.Groups[2].Value; $p = $x.Groups[3].Value }
  }
  Remove-Item $dec -ErrorAction SilentlyContinue

  "$name,$kbps,$got,$m,$p,$ss" | Out-File -FilePath $out -Append -Encoding utf8
  Write-Host "$name $kbps got=$got p50_24=$m p95_24=$p"
}

function FF($name, [string[]]$o, $ext, $kbps) {
  $f = "$sp\f-$name-$kbps.$ext"
  & $ff @(@("-hide_banner","-y","-i",$src) + $o + @($f)) 2>&1 | Out-Null
  if (-not (Test-Path $f)) { Write-Host "$name $kbps FAIL"; return }
  Score $name $kbps $f
  Remove-Item $f -ErrorAction SilentlyContinue
}

function Ours($kbps) {
  $f = "$sp\f-ours-$kbps.ivf"
  & $spike --source image:$SweepShot --motion scroll --frames $N --encode vp9 --max-q 63 --bitrate $kbps --emit-ivf $f 2>&1 | Out-Null
  if (Test-Path $f) { Score "vp9ours" $kbps $f; Remove-Item $f -ErrorAction SilentlyContinue }
}

# How far down the leader goes before it stops holding the bar.
foreach ($k in 400, 600, 800) {
  FF "hevcqsv" @("-c:v","hevc_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
}
# Where the runner-up crosses.
foreach ($k in 1400, 1700) {
  FF "h264qsv" @("-c:v","h264_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
}
# Where ours crosses.
foreach ($k in 2800, 3400) { Ours $k }

Write-Host "done"
