$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)

$root = $PSScriptRoot
$source = Join-Path $root "test.ku"
$exe = Join-Path $root "target\release\ku.exe"

Write-Host "Building Ku release executable..."
& cargo build --release --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) {
    throw "cargo build --release failed with exit code $LASTEXITCODE"
}

$startInfo = [System.Diagnostics.ProcessStartInfo]::new()
$startInfo.FileName = $exe
$startInfo.Arguments = '"' + $source + '"'
$startInfo.WorkingDirectory = $root
$startInfo.UseShellExecute = $false
$startInfo.CreateNoWindow = $true
$startInfo.RedirectStandardOutput = $true
$startInfo.RedirectStandardError = $true

$process = [System.Diagnostics.Process]::new()
$process.StartInfo = $startInfo
$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()

if (-not $process.Start()) {
    throw "failed to start $exe"
}

$peakWorkingSet = 0L
$peakPrivate = 0L
$peakThreads = 0
$cpuMs = 0L

while (-not $process.HasExited) {
    try {
        $sample = Get-Process -Id $process.Id -ErrorAction Stop
        $peakWorkingSet = [Math]::Max($peakWorkingSet, $sample.WorkingSet64)
        $peakPrivate = [Math]::Max($peakPrivate, $sample.PrivateMemorySize64)
        $peakThreads = [Math]::Max($peakThreads, $sample.Threads.Count)
        $cpuMs = [Math]::Max($cpuMs, [long]$sample.TotalProcessorTime.TotalMilliseconds)
    } catch {
        # The process may exit between HasExited and Get-Process.
    }

    if ($stopwatch.Elapsed.TotalSeconds -gt 120) {
        $process.Kill($true)
        throw "stress test exceeded the 120 second external deadline"
    }
    Start-Sleep -Milliseconds 2
}

$stdout = $process.StandardOutput.ReadToEnd()
$stderr = $process.StandardError.ReadToEnd()
$stopwatch.Stop()

if ($stdout) {
    Write-Host $stdout.TrimEnd()
}
if ($stderr) {
    Write-Error $stderr.TrimEnd()
}

Write-Host ""
Write-Host "=== External process resource sampling ==="
Write-Host ("ExitCode: {0}" -f $process.ExitCode)
Write-Host ("WallMs: {0}" -f [Math]::Round($stopwatch.Elapsed.TotalMilliseconds))
Write-Host ("CpuMs: {0}" -f $cpuMs)
Write-Host ("PeakWorkingSetMiB: {0:N2}" -f ($peakWorkingSet / 1MB))
Write-Host ("PeakPrivateMemoryMiB: {0:N2}" -f ($peakPrivate / 1MB))
Write-Host ("PeakThreads: {0}" -f $peakThreads)

if ($process.ExitCode -ne 0) {
    exit $process.ExitCode
}
