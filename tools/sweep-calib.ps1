. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$src = "$sp\src.y4m"
$dec = "$sp\dec.y4m"
$out = "$sp\calib-results.csv"
$N = 300

if (-not (Test-Path $src)) {
  & $spike --source image:$SweepShot --motion scroll --frames $N --export-yuv $src 2>&1 | Out-Null
}

"codec,target_kbps,got_kbps,ssim,m8,m16,m24,m32,p8,p16,p24,p32,all16" | Out-File -FilePath $out -Encoding utf8

function Score($name, $kbps, $file) {
  $got = [math]::Round((Get-Item $file).Length / $N * 8 * 30 / 1000, 0)

  $sr = & $ff -hide_banner -i $file -i $src -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | Out-String
  $ss = if ($sr -match "All:([0-9.]+)") { $Matches[1] } else { "?" }

  Remove-Item $dec -ErrorAction SilentlyContinue
  & $ff -hide_banner -y -i $file -pix_fmt yuv420p $dec 2>&1 | Out-Null

  $q = & $spike --quality $src $dec 2>&1 | Out-String
  $m = @{}; $p = @{}
  foreach ($x in [regex]::Matches($q, '>(\d+)\s+([\d.]+)\s+([\d.]+)')) {
    $m[$x.Groups[1].Value] = $x.Groups[2].Value
    $p[$x.Groups[1].Value] = $x.Groups[3].Value
  }
  $a16 = if ($q -match 'p95 ([\d.]+)%') { $Matches[1] } else { "?" }

  "$name,$kbps,$got,$ss,$($m['8']),$($m['16']),$($m['24']),$($m['32']),$($p['8']),$($p['16']),$($p['24']),$($p['32']),$a16" |
    Out-File -FilePath $out -Append -Encoding utf8
  Write-Host "$name $kbps got=$got ssim=$ss p50_16=$($m['16']) p50_24=$($m['24']) p95_24=$($p['24'])"
  Remove-Item $dec -ErrorAction SilentlyContinue
}

function P($name, [string[]]$o, $ext, $kbps) {
  $f = "$sp\c-$name-$kbps.$ext"
  $full = @("-hide_banner", "-y", "-i", $src) + $o + @($f)
  & $ff @full 2>&1 | Out-Null
  if (-not (Test-Path $f)) { Write-Host "$name $kbps FAIL"; return }
  Score $name $kbps $f
  Remove-Item $f -ErrorAction SilentlyContinue
}

foreach ($k in 1000, 2000, 4000) {
  P "x264"    @("-c:v","libx264","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-preset","veryfast","-tune","zerolatency","-g","300") "mp4" $k
  P "h264qsv" @("-c:v","h264_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
  P "hevcqsv" @("-c:v","hevc_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
  P "vp9ff"   @("-c:v","libvpx-vp9","-b:v","${k}k","-maxrate","${k}k","-minrate","${k}k","-bufsize","$([int]($k/2))k","-deadline","realtime","-cpu-used","8","-row-mt","1","-tile-columns","1","-lag-in-frames","0","-qmax","63") "webm" $k
}

foreach ($k in 1000, 2000, 4000) {
  $f = "$sp\c-vp9ours-$k.ivf"
  & $spike --source image:$SweepShot --motion scroll --frames $N --encode vp9 --max-q 63 --bitrate $k --emit-ivf $f 2>&1 | Out-Null
  if (Test-Path $f) { Score "vp9ours" $k $f; Remove-Item $f -ErrorAction SilentlyContinue }
}

Write-Host "done"
