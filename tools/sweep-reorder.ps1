. "$PSScriptRoot\sweep-common.ps1"
$sp = $SweepWork
$s = $SweepWork
$ff = $SweepFfmpeg
$fp = $SweepFfprobe
$spike = $SweepBin

# Свой вход, а не оставшийся от соседнего прогона. Скрипт, полагающийся на
# готовый файл, молча меряет не то, что думает: именно так строка кодека
# оказывалась снятой с чужого содержимого.
$bf = Join-Path $s "bf.y4m"
if (-not (Test-Path $bf)) {
  Write-Host "готовлю $bf …"
  & $spike --source "image:$SweepShot" --motion scroll --frames 60 --export-yuv $bf 2>&1 | Out-Null
}
if (-not (Test-Path $bf)) { throw "не удалось подготовить $bf" }


function Test-Reorder([string]$tag, [string[]]$opts) {
  $f = "$s\ro-$tag.mp4"
  & $ff @(@("-hide_banner", "-y", "-i", "$bf") + $opts + @($f)) 2>&1 | Out-Null
  if (-not (Test-Path $f)) { Write-Host "$tag FAIL"; return }

  $hb = (& $fp -v error -select_streams v:0 -show_entries stream=has_b_frames -of csv=p=0 $f 2>&1 | Out-String).Trim()
  $types = (& $fp -v error -select_streams v:0 -show_entries frame=pict_type -of csv=p=0 $f 2>&1 | Out-String) -replace "[^IPB]", ""
  $lines = & $fp -v error -select_streams v:0 -show_entries packet=pts,dts -of csv=p=0 $f 2>&1
  $mismatch = 0
  $neg = 0
  foreach ($l in $lines) {
    $p = "$l" -split ","
    if ($p.Count -ge 2) {
      if ($p[0] -ne $p[1]) { $mismatch++ }
      if ([int]$p[1] -lt 0) { $neg++ }
    }
  }
  $kbps = [math]::Round((Get-Item $f).Length / 60 * 8 * 30 / 1000, 0)
  $head = ($lines | Select-Object -First 5) -join "  "
  Write-Host ("{0,-14} has_b={1}  pts<>dts={2}  dts<0={3}  B={4}  {5} kbps" -f `
    $tag, $hb, $mismatch, $neg, ([regex]::Matches($types, "B")).Count, $kbps)
  Write-Host "               pts,dts: $head"
  Remove-Item $f -ErrorAction SilentlyContinue
}

Test-Reorder "h264-default" @("-c:v","h264_qsv","-b:v","2000k","-maxrate","2000k","-bufsize","2000k","-async_depth","1","-g","300")
Test-Reorder "h264-bf0"     @("-c:v","h264_qsv","-b:v","2000k","-maxrate","2000k","-bufsize","2000k","-async_depth","1","-bf","0","-g","300")
Test-Reorder "hevc-default" @("-c:v","hevc_qsv","-b:v","2000k","-maxrate","2000k","-bufsize","2000k","-async_depth","1","-g","300")
Test-Reorder "hevc-bf0"     @("-c:v","hevc_qsv","-b:v","2000k","-maxrate","2000k","-bufsize","2000k","-async_depth","1","-bf","0","-g","300")
Write-Host "done"
