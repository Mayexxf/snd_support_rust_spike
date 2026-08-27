. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$dec = "$sp\bdec.y4m"
$out = "$sp\bf0-results.csv"
$N = 300
$k = 2000

"step,codec,bf,got_kbps,first_stop,p50_24,p95_24" | Out-File -FilePath $out -Encoding utf8

function Measure-One([string]$name, [string]$bf, [int]$step, [string]$file, [string]$src) {
  $got = [math]::Round((Get-Item $file).Length / $N * 8 * 30 / 1000, 0)
  Remove-Item $dec -ErrorAction SilentlyContinue
  & $ff -hide_banner -y -i $file -pix_fmt yuv420p $dec 2>&1 | Out-Null
  $q = & $spike --motion settle --quality $src $dec 2>&1 | Out-String
  Remove-Item $dec -ErrorAction SilentlyContinue

  $m = "?"; $p = "?"
  foreach ($x in [regex]::Matches($q, '>(\d+)\s+([\d.]+)\s+([\d.]+)')) {
    if ($x.Groups[1].Value -eq "24") { $m = $x.Groups[2].Value; $p = $x.Groups[3].Value }
  }
  $first = "never"
  foreach ($line in ($q -split "`n")) {
    if ($line -match '\s1:') {
      if ($line -match '(\d+)\s+\S+\(\S+\)') { $first = $Matches[1] }
      break
    }
  }
  "$step,$name,$bf,$got,$first,$m,$p" | Out-File -FilePath $out -Append -Encoding utf8
  Write-Host ("step={0} {1} bf={2} got={3} first={4} p50={5} p95={6}" -f $step, $name, $bf, $got, $first, $m, $p)
}

foreach ($step in 40, 60, 80) {
  $src = "$sp\b0-$step.y4m"
  & $spike --source image:$SweepShot --motion settle --step $step --frames $N --export-yuv $src 2>&1 | Out-Null

  foreach ($enc in "hevc_qsv", "h264_qsv") {
    foreach ($bf in "on", "off") {
      $o = @("-c:v", $enc, "-b:v", "${k}k", "-maxrate", "${k}k", "-bufsize", "${k}k", "-async_depth", "1", "-g", "300")
      if ($bf -eq "off") { $o += @("-bf", "0") }
      $f = "$sp\b0-$step-$enc-$bf.mp4"
      & $ff @(@("-hide_banner", "-y", "-i", $src) + $o + @($f)) 2>&1 | Out-Null
      if (Test-Path $f) { Measure-One $enc $bf $step $f $src; Remove-Item $f -ErrorAction SilentlyContinue }
      else { Write-Host "step=$step $enc bf=$bf FAIL" }
    }
  }
  Remove-Item $src -ErrorAction SilentlyContinue
}
Write-Host "done"
