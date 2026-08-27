. "$PSScriptRoot\sweep-common.ps1"

# Каноническая таблица сравнения кодеков. Заменяет всё, что было снято раньше.
#
# Отличия от прежних заходов, каждое из которых стоило неверного вывода:
#
#   ключи ПОРОЖДЕНЫ из настроек нашего кодера (Get-PlanArgs), а не выписаны
#     руками -- рукописные пропустили четыре расхождения подряд;
#   каждая строка проверяется ПО БИТСТРИМУ (Test-RowIsComparable) и
#     выбрасывается, если кодер переупорядочивал кадры или поставил не столько
#     ключевых, сколько задано;
#   шаг прокрутки 61, а не 60: 61 взаимно прост с высотой окна 648, поэтому
#     появляются все 648 положений вместо 54, и кодер не выигрывает от того, что
#     видел это содержимое пять раз;
#   качество меряется мерой читаемости, а не только SSIM, и сравнение
#     отказывается считать смещённую или совпадающую пару.

$N = 300
$STEP = 61
$KBPS = 1000, 2000, 4000
$out = Join-Path $SweepWork "table-results.csv"
$src = Join-Path $SweepWork "table-src.y4m"
$dec = Join-Path $SweepWork "table-dec.y4m"

"codec,target_kbps,got_kbps,keyframes,has_b,reorder,p50_24,p95_24,ssim" |
  Out-File -FilePath $out -Encoding utf8

Write-Host "готовлю исходник: $N кадров, шаг $STEP …"
& $SweepBin --source "image:$SweepShot" --motion scroll --step $STEP --frames $N --export-yuv $src 2>&1 |
  Select-String "отпечаток|период|опрошено"
if (-not (Test-Path $src)) { throw "не удалось подготовить $src" }

$plan = & $SweepBin --plan libvpx-vp9 --bitrate 2000 --encode vp9 2>&1 | Out-String
$gop = if ($plan -match "-g (\d+)") { [int]$Matches[1] } else { throw "в плане нет -g" }
Write-Host "интервал ключевых кадров: $gop`n"

function Add-Row([string]$name, [int]$kbps, [string]$file) {
  if (-not (Test-RowIsComparable $name $file $gop $N)) {
    "$name,$kbps,-,-,-,-,ОТКАЗ,ОТКАЗ,ОТКАЗ" | Out-File -FilePath $out -Append -Encoding utf8
    return
  }
  $c = Get-RealisedConfig $file
  $got = [math]::Round((Get-Item $file).Length / $N * 8 * 30 / 1000, 0)

  $sr = & $SweepFfmpeg -hide_banner -i $file -i $src -lavfi "[0:v][1:v]ssim" -f null - 2>&1 | Out-String
  $ss = if ($sr -match "All:([0-9.]+)") { $Matches[1] } else { "?" }

  Remove-Item $dec -ErrorAction SilentlyContinue
  & $SweepFfmpeg -hide_banner -y -i $file -pix_fmt yuv420p $dec 2>&1 | Out-Null
  $q = & $SweepBin --quality $src $dec 2>&1 | Out-String
  Remove-Item $dec -ErrorAction SilentlyContinue

  $m = "?"; $p = "?"
  foreach ($x in [regex]::Matches($q, '>(\d+)\s+([\d.]+)\s+([\d.]+)')) {
    if ($x.Groups[1].Value -eq "24") { $m = $x.Groups[2].Value; $p = $x.Groups[3].Value }
  }
  if ($q -match "не совпадают|ни один пиксель") {
    Write-Host "  ОТКАЗ $name : сравнение отвергло пару"
    $m = "ОТКАЗ"; $p = "ОТКАЗ"
  }

  "$name,$kbps,$got,$($c.keyframes),$($c.has_b),$($c.reorder),$m,$p,$ss" |
    Out-File -FilePath $out -Append -Encoding utf8
  Write-Host "  $name $kbps -> $got кбит/с, порча p50 $m / p95 $p, ssim $ss"
}

foreach ($k in $KBPS) {
  Write-Host "=== $k кбит/с"
  foreach ($t in @(@("libx264","mp4"), @("h264_qsv","mp4"), @("hevc_qsv","mp4"), @("libvpx-vp9","webm"))) {
    $opts = Get-PlanArgs $t[0] $k
    $f = Join-Path $SweepWork "tb-$($t[0] -replace '[^a-z0-9]','_')-$k.$($t[1])"
    & $SweepFfmpeg @(@("-hide_banner","-y","-i",$src) + $opts + @($f)) 2>&1 | Out-Null
    if (Test-Path $f) { Add-Row $t[0] $k $f; Remove-Item $f -ErrorAction SilentlyContinue }
    else { Write-Host "  ОТКАЗ $($t[0]) : ffmpeg ничего не написал" }
  }

  # Наш кодер -- своим битстримом, на тех же кадрах.
  $f = Join-Path $SweepWork "tb-ours-$k.ivf"
  & $SweepBin --source "image:$SweepShot" --motion scroll --step $STEP --frames $N `
    --encode vp9 --bitrate $k --emit-ivf $f 2>&1 | Out-Null
  if (Test-Path $f) { Add-Row "ours-vp9" $k $f; Remove-Item $f -ErrorAction SilentlyContinue }
}

Remove-Item $src -ErrorAction SilentlyContinue
Write-Host "`nтаблица: $out"
Write-Host "done"
