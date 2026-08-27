. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

$src = "$sp\src.y4m"

$k = 2000
$crop = "crop=960:340:480:420"
$pick = "select=eq(n\,250)"

& $spike --source image:$SweepShot --motion scroll --frames 300 --encode vp9 --max-q 63 --bitrate $k --emit-ivf "$sp\L-ours.ivf" 2>&1 | Out-Null
& $ff -hide_banner -y -i $src -c:v hevc_qsv -b:v "${k}k" -maxrate "${k}k" -bufsize "${k}k" -async_depth 1 -g 300 "$sp\L-hevc.mp4" 2>&1 | Out-Null
& $ff -hide_banner -y -i $src -c:v libvpx-vp9 -b:v "${k}k" -maxrate "${k}k" -minrate "${k}k" -bufsize 1000k -deadline realtime -cpu-used 8 -row-mt 1 -tile-columns 1 -lag-in-frames 0 -qmax 63 "$sp\L-vp9ff.webm" 2>&1 | Out-Null

& $ff -hide_banner -y -i $src            -vf "$pick,$crop" -update 1 -frames:v 1 "$sp\L-0-src.png"   2>&1 | Out-Null
& $ff -hide_banner -y -i "$sp\L-hevc.mp4"  -vf "$pick,$crop" -update 1 -frames:v 1 "$sp\L-1-hevc28.png" 2>&1 | Out-Null
& $ff -hide_banner -y -i "$sp\L-ours.ivf"  -vf "$pick,$crop" -update 1 -frames:v 1 "$sp\L-2-ours44.png" 2>&1 | Out-Null
& $ff -hide_banner -y -i "$sp\L-vp9ff.webm" -vf "$pick,$crop" -update 1 -frames:v 1 "$sp\L-3-vp9ff64.png" 2>&1 | Out-Null

Get-ChildItem "$sp\L-*.png" | ForEach-Object { "$($_.Name) $($_.Length)" }
Write-Host "done"
