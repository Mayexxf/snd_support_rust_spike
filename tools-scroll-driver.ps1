# Крутит колесо в окне под курсором ровно и непрерывно заданное время.
#
# Существует, чтобы убрать человека из живого прогона: две ручные прокрутки
# никогда не совпадают, и README прямо говорит, что именно из-за этого числа
# двух прогонов делить нельзя.
#
# Чего здесь намеренно НЕТ: браузерной автоматизации и интерпретатора Python.
# Замер идёт на четырёх ядрах, и всё, что крутится рядом, попадает в его p95 —
# ровно так RDP однажды завысил кодер до 40%. Отрисовка прокручиваемого окна
# это законная нагрузка, она есть и в бою; драйвер прокрутки — нет.
#
# Поэтому: одно обращение к user32 на тик, приоритет ниже обычного, и отчёт о
# собственном процессорном времени в конце. Последнее не украшение: без него я
# меняю одну неизвестную нагрузку на другую.
param(
    [int]$Seconds = 60,
    [int]$NotchesPerSecond = 20,
    [int]$ReverseEvery = 40,
    [string]$WindowMatch = "Браузер"
)

Add-Type -Namespace Win -Name Api -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
[DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, int dx, int dy, int dwData, UIntPtr dwExtraInfo);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
'@

$MOUSEEVENTF_WHEEL = 0x0800
$WHEEL_DELTA = 120

$target = Get-Process | Where-Object { $_.MainWindowTitle -like "*$WindowMatch*" } | Select-Object -First 1
if (-not $target) { "окно с «$WindowMatch» в заголовке не найдено"; exit 1 }

$rect = New-Object Win.Api+RECT
if (-not [Win.Api]::GetWindowRect($target.MainWindowHandle, [ref]$rect)) { "не удалось узнать размеры окна"; exit 1 }

[void][Win.Api]::SetForegroundWindow($target.MainWindowHandle)
Start-Sleep -Milliseconds 400

# Курсор ставится один раз, в середину окна, и больше не двигается: колесо идёт
# в окно под курсором, а дрожание курсора само по себе даёт кадры, где сменился
# только указатель, и смазало бы как раз ту величину, которую мы теперь считаем
# отдельно.
$cx = [int](($rect.Left + $rect.Right) / 2)
$cy = [int](($rect.Top + $rect.Bottom) / 2)
[void][Win.Api]::SetCursorPos($cx, $cy)

$me = Get-Process -Id $PID
$me.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal
$cpuStart = $me.TotalProcessorTime

"кручу «$($target.MainWindowTitle)» $Seconds с, $NotchesPerSecond щелчков в секунду"

# Направление переворачивается, и это не украшение. Первая попытка крутила в
# одну сторону: короткая страница кончилась за несколько секунд, дальше экран
# стоял, и прогон дал 144 кадра с содержимым против 918 у человека — то есть
# мерил неподвижный стол под видом прокрутки. С переворотом длина страницы
# перестаёт иметь значение.
$intervalMs = [int](1000 / $NotchesPerSecond)
$deadline = (Get-Date).AddSeconds($Seconds)
$ticks = 0
$direction = -1
while ((Get-Date) -lt $deadline) {
    [Win.Api]::mouse_event($MOUSEEVENTF_WHEEL, 0, 0, $direction * $WHEEL_DELTA, [UIntPtr]::Zero)
    $ticks++
    if ($ticks % $ReverseEvery -eq 0) { $direction = -$direction }
    Start-Sleep -Milliseconds $intervalMs
}

$cpu = ((Get-Process -Id $PID).TotalProcessorTime - $cpuStart).TotalSeconds
"щелчков отдано: $ticks"
"процессорное время драйвера: $([math]::Round($cpu,2)) с из $Seconds — это $([math]::Round(100*$cpu/(4*$Seconds),2))% машины"
