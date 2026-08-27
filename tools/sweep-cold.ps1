. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$dec = "$sp\cdec.y4m"
$out = "$sp\cold-results.csv"
$N = 300
$k = 2000

"step,codec,got_kbps,first_stop_frames" | Out-File -FilePath $out -Encoding utf8

function FirstStop($name, $step, $file, $src) {
  $got = [math]::Round((Get-Item $file).Length / $N * 8 * 30 / 1000, 0)
  Remove-Item $dec -ErrorAction SilentlyContinue
  & $ff -hide_banner -y -i $file -pix_fmt yuv420p $dec 2>&1 | Out-Null
  $q = & $spike --motion settle --quality $src $dec 2>&1 | Out-String
  Remove-Item $dec -ErrorAction SilentlyContinue

  $first = "never"
  if ($q -match '(?m)^\s*\S+\s+1:\s+\S+\s+\S+\s+(\d+)\s') { $first = $Matches[1] }
  foreach ($line in ($q -split "`n")) {
    if ($line -match '1:') {
      if ($line -match '(\d+)\s+\S+\(\S+\)') { $first = $Matches[1] } else { $first = "never" }
      break
    }
  }
  "$step,$name,$got,$first" | Out-File -FilePath $out -Append -Encoding utf8
  Write-Host "step=$step $name got=$got firststop=$first"
}

foreach ($step in 40, 60, 80) {
  $src = "$sp\cold-$step.y4m"
  & $spike --source image:$SweepShot --motion settle --step $step --frames $N --export-yuv $src 2>&1 | Out-Null

  foreach ($c in @(
    @("hevcqsv", @("-c:v","hevc_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300"), "mp4"),
    @("h264qsv", @("-c:v","h264_qsv","-b:v","${k}k","-maxrate","${k}k","-bufsize","${k}k","-async_depth","1","-g","300"), "mp4")
  )) {
    $f = "$sp\cold-$step-$($c[0]).$($c[2])"
    & $ff @(@("-hide_banner","-y","-i",$src) + $c[1] + @($f)) 2>&1 | Out-Null
    if (Test-Path $f) { FirstStop $c[0] $step $f $src; Remove-Item $f -ErrorAction SilentlyContinue }
  }

  $f = "$sp\cold-$step-ours.ivf"
  & $spike --source image:$SweepShot --motion settle --step $step --frames $N --encode vp9 --max-q 63 --bitrate $k --emit-ivf $f 2>&1 | Out-Null
  if (Test-Path $f) { FirstStop "ours" $step $f $src; Remove-Item $f -ErrorAction SilentlyContinue }

  Remove-Item $src -ErrorAction SilentlyContinue
}
Write-Host "done"
