. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$src = "$sp\rd3.y4m"
$N = 300
$t = & $ff -hide_banner -benchmark -i $src -f null - 2>&1 | Out-String
$bu = if ($t -match "utime=([0-9.]+)s") { [double]$Matches[1] } else { 0 }
$bs = if ($t -match "stime=([0-9.]+)s") { [double]$Matches[1] } else { 0 }
$base = $bu + $bs
"baseline_read_cpu_s=$([math]::Round($base,3))"
"codec,target_kbps,got_kbps,hit,cpu_ms_frame,bytes_frame,ssim"

function P($name, [string[]]$o, $ext, $kbps) {
  $out = "$sp\r3-$name-$kbps.$ext"
  $full = @("-hide_banner","-y","-benchmark","-i",$src) + $o + @($out)
  $r = & $ff @full 2>&1 | Out-String
  if (-not (Test-Path $out)) { "$name,$kbps,-,FAIL,-,-,-"; return }
  $u = if ($r -match "utime=([0-9.]+)s") { [double]$Matches[1] } else { 0 }
  $s = if ($r -match "stime=([0-9.]+)s") { [double]$Matches[1] } else { 0 }
  $cpu = [math]::Max(0.0, ($u + $s - $base)) * 1000 / $N
  $bpf = (Get-Item $out).Length / $N
  $got = $bpf * 8 * 30 / 1000
  $ratio = $got / $kbps
  $hit = if ($ratio -gt 0.75 -and $ratio -lt 1.35) { "ok" } else { "MISS x$([math]::Round($ratio,2))" }
  $sr = & $ff -hide_banner -i $out -i $src -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | Out-String
  $ss = if ($sr -match "All:([0-9.]+)") { $Matches[1] } else { "?" }
  "$name,$kbps,$([math]::Round($got,0)),$hit,$([math]::Round($cpu,2)),$([math]::Round($bpf,0)),$ss"
  Remove-Item $out -ErrorAction SilentlyContinue
}

foreach ($k in 1000,2000,4000) {
  P "x264"    @("-c:v","libx264","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-preset","veryfast","-tune","zerolatency","-g","300") "mp4" $k
  P "h264qsv" @("-c:v","h264_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
  P "hevcqsv" @("-c:v","hevc_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300") "mp4" $k
}
"done"
