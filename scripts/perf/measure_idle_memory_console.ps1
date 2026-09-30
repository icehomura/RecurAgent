# 真实控制台口径的空闲内存测量。
#
# 背景: measure_idle_memory_win.ps1 用重定向 stdio 启动 ra，实测发现进程
# CPU 5 秒零增长、无任何输出 —— 非 tty 环境下 TUI 根本没进事件循环，
# 测到的是"躺尸"内存，不代表交互会话。
#
# 本脚本用 UseShellExecute=$true 让 Windows 分配真实控制台窗口，进程才会
# 走完整 TUI 初始化路径。代价是会弹出窗口（结束后由脚本杀掉）。
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string[]]$Args = @(),
    [double]$SettleSeconds = 8.0,
    [int]$SampleCount = 5,
    [double]$IntervalSeconds = 1.0,
    [string]$Label = "idle_real_console"
)

function Snap([System.Diagnostics.Process]$p) {
    try { $p.Refresh() } catch { return $null }
    if ($p.HasExited) { return $null }
    [pscustomobject]@{
        ws_bytes      = $p.WorkingSet64
        private_bytes = $p.PrivateMemorySize64
        peak_ws_bytes = $p.PeakWorkingSet64
        threads       = $p.Threads.Count
        cpu_ms        = [math]::Round($p.TotalProcessorTime.TotalMilliseconds, 1)
        handles       = $p.HandleCount
    }
}

# Start-Process 不接受空的 -ArgumentList，无参数时必须整个省略。
$p = if ($Args.Count -gt 0) {
    Start-Process -FilePath $Exe -ArgumentList $Args -PassThru -WindowStyle Normal
} else {
    Start-Process -FilePath $Exe -PassThru -WindowStyle Normal
}
Start-Sleep -Milliseconds ([int]($SettleSeconds * 1000))

$samples = @()
for ($i = 0; $i -lt $SampleCount; $i++) {
    $s = Snap $p
    if ($null -eq $s) { break }
    $samples += $s
    Start-Sleep -Milliseconds ([int]($IntervalSeconds * 1000))
}

$exited = $p.HasExited
if (-not $exited) {
    try { $p.Kill($true) } catch { }
    try { $p.WaitForExit(3000) } catch { }
}

$r = [ordered]@{
    label        = $Label
    exe          = $Exe
    argv         = @($Args)
    pid          = $p.Id
    mode         = "real_console"
    sample_count = $samples.Count
    exited_early = $exited
    samples      = $samples
}

if ($samples.Count -gt 0) {
    $ws = $samples | ForEach-Object { $_.ws_bytes } | Sort-Object
    $pv = $samples | ForEach-Object { $_.private_bytes } | Sort-Object
    $cpu = $samples | ForEach-Object { $_.cpu_ms }
    $r["ws_median_mib"]       = [math]::Round($ws[[int][math]::Floor($ws.Count / 2)] / 1MB, 1)
    $r["ws_max_mib"]          = [math]::Round($ws[-1] / 1MB, 1)
    $r["ws_spread_bytes"]     = $ws[-1] - $ws[0]
    $r["private_median_mib"]  = [math]::Round($pv[[int][math]::Floor($pv.Count / 2)] / 1MB, 1)
    $r["peak_ws_mib"]         = [math]::Round(($samples | Measure-Object -Property peak_ws_bytes -Maximum).Maximum / 1MB, 1)
    $r["threads"]             = $samples[-1].threads
    $r["cpu_delta_ms"]        = [math]::Round($cpu[-1] - $cpu[0], 1)
    $r["cpu_delta_mib_note"]  = "cpu_delta_ms>0 means the process is actually running (ticking), not parked"
}

$r | ConvertTo-Json -Depth 5
