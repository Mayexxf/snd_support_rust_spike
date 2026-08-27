. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$src = "$s\src.y4m"
$out = "$s\bridge-results.csv"

"maxq,target_kbps,got_kbps,bytes_frame,ssim" | Out-File -FilePath $out -Encoding utf8

foreach ($q in 56, 63) {
  foreach ($k in 1000, 2000, 4000) {
    $ivf = "$s\b-$q-$k.ivf"
    $log = & $spike --source image:$SweepShot --motion scroll --frames 300 `
      --encode vp9 --max-q $q --bitrate $k --emit-ivf $ivf 2>&1 | Out-String

    $got = 0; $bpf = 0
    if ($log -match "(\d+) кбит/с при \d+ к/с, (\d+) Б на кадр") {
      $got = [int]$Matches[1]; $bpf = [int]$Matches[2]
    }

    $ss = & $ff -hide_banner -i $ivf -i $src -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | Out-String
    $ssim = "?"
    if ($ss -match "All:([0-9.]+)") { $ssim = $Matches[1] }

    "$q,$k,$got,$bpf,$ssim" | Out-File -FilePath $out -Append -Encoding utf8
    Write-Host "maxq=$q target=$k got=$got ssim=$ssim"
  }
}
Write-Host "done"
