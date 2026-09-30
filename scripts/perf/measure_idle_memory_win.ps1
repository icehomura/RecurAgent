# scripts/perf/measure_idle_memory.py 的 Windows 替代实现。
#
# 原脚本的 rss() 非 Darwin 一律走 rss_linux()（读 /proc/<pid>/status），
# Windows 上必然返回 None，样本数不足直接 exit 2。此脚本用 .NET 进程计数器
# 取等价指标：
#   WorkingSet64        <-> Linux VmRSS（物理内存驻留集）
#   PrivateMemorySize64 <-> 提交的私有内存（更接近"进程真实占用"）
#   PeakWorkingSet64    <-> 生命周期峰值
#
# 用法: powershell -File measure_idle_memory_win.ps1 -Exe <path> [-Args <string[]>]
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [string[]]$Args = @(),
    [double]$SettleSeconds = 5.0,
    [int]$SampleCount = 5,
    [double]$IntervalSeconds = 1.0,
    [string]$Label = "idle"
)

function Get-MemSnapshot([System.Diagnostics.Process]$p) {
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

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Exe
foreach ($a in $Args) { [void]$psi.ArgumentList.Add($a) }
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true   # 保持 stdin 打开，TUI 不会因 EOF 退出
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true

$proc = New-Object System.Diagnostics.Process
$proc.StartInfo = $psi
$sw = [System.Diagnostics.Stopwatch]::StartNew()
[void]$proc.Start()

# 异步抽干 stdout/stderr，避免管道缓冲写满导致进程阻塞。
# 注意: Stream.ReadAsync() 无参重载在 .NET Framework 的 PS 5.1 绑定层不可用，
# 必须用 ReadToEndAsync()（Task<string>），否则脚本会静默跳过抽流。
$stdoutTask = $proc.StandardOutput.ReadToEndAsync()
$stderrTask = $proc.StandardError.ReadToEndAsync()

Start-Sleep -Milliseconds ([int]($SettleSeconds * 1000))
$settle_ms = $sw.ElapsedMilliseconds

$samples = @()
for ($i = 0; $i -lt $SampleCount; $i++) {
    $s = Get-MemSnapshot $proc
    if ($null -eq $s) { break }
    $samples += $s
    Start-Sleep -Milliseconds ([int]($IntervalSeconds * 1000))
}

$exited = $proc.HasExited
$exitCode = if ($exited) { $proc.ExitCode } else { $null }

if (-not $exited) {
    try { $proc.Kill($true) } catch { }
    try { $proc.WaitForExit(3000) } catch { }
}

$result = [ordered]@{
    label            = $Label
    exe              = $Exe
    argv             = @($Args)
    pid              = $proc.Id
    settle_ms        = $settle_ms
    sample_count     = $samples.Count
    exited_early     = $exited
    exit_code        = $exitCode
    samples          = $samples
}

# 进程已终止（自然退出或被 kill）后取回被抽干的输出，用于判定进程是否真的
# 进入了渲染循环。空 stdout == 非 tty 环境下可能退化/静默，此时采样值
# 不代表真实 TUI 会话，必须在报告里标注。
$result["stdout_head"] = ""
$result["stderr_head"] = ""
foreach ($pair in @(@("stdout_head", $stdoutTask), @("stderr_head", $stderrTask))) {
    try {
        if ($pair[1].Wait(2000)) {
            $txt = $pair[1].Result
            $result[$pair[0]] = $txt.Substring(0, [Math]::Min(400, $txt.Length))
        } else {
            $result[$pair[0]] = "<drain timeout>"
        }
    } catch { $result[$pair[0]] = "<drain error: $($_.Exception.Message)>" }
}

if ($samples.Count -gt 0) {
    $ws = $samples | ForEach-Object { $_.ws_bytes } | Sort-Object
    $pv = $samples | ForEach-Object { $_.private_bytes } | Sort-Object
    $result["ws_median_bytes"]      = $ws[[int][math]::Floor($ws.Count / 2)]
    $result["ws_median_mib"]        = [math]::Round($ws[[int][math]::Floor($ws.Count / 2)] / 1MB, 1)
    $result["ws_min_bytes"]         = $ws[0]
    $result["ws_max_bytes"]         = $ws[-1]
    $result["ws_spread_bytes"]      = $ws[-1] - $ws[0]
    $result["private_median_mib"]   = [math]::Round($pv[[int][math]::Floor($pv.Count / 2)] / 1MB, 1)
    $result["peak_ws_mib"]          = [math]::Round(($samples | Measure-Object -Property peak_ws_bytes -Maximum).Maximum / 1MB, 1)
}

$result | ConvertTo-Json -Depth 5
