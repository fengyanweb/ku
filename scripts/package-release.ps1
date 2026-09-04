[CmdletBinding()]
param(
    [switch]$InstallExtension,
    [switch]$CheckOnly,
    [Parameter(DontShow = $true)]
    [switch]$ArchiveInternal,
    [Parameter(DontShow = $true)]
    [switch]$SelfTest
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
if ($PSVersionTable.PSVersion.Major -lt 7) { throw "package-release.ps1 requires PowerShell 7 or newer" }
if ($CheckOnly -and $InstallExtension) { throw "-CheckOnly cannot be combined with -InstallExtension" }
if ($ArchiveInternal -and $InstallExtension) { throw "archive packaging cannot install the VS Code extension" }
if ($SelfTest -and ($CheckOnly -or $InstallExtension -or $ArchiveInternal)) { throw "-SelfTest cannot be combined with packaging switches" }
$utf8 = [Text.UTF8Encoding]::new($false, $true)
$outputUtf8 = [Text.UTF8Encoding]::new($false, $false)
$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$pathComparison = if ($IsWindows) { [StringComparison]::OrdinalIgnoreCase } else { [StringComparison]::Ordinal }
$processOutputLimit = 4MB
$cleanupEntryLimit = 250000
$script:archiveVerifier = $null
$script:releaseWorkRoot = $null
$script:toolWrapper = $null
$script:expectedTlsHeaderHash = $null

function Get-HostContract {
    $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    if ($IsWindows -and $architecture -eq "X64") { return @{ Target = "x86_64-pc-windows-msvc"; Executable = "ku.exe"; ObjectFormat = "coff-x86_64"; Archive = "ku_native_tls.lib"; Flavor = "msvc"; LinkContract = "rust-1.89.0-windows-msvc-v1"; Crt = "msvc-dynamic" } }
    if ($IsLinux -and $architecture -eq "X64") { return @{ Target = "x86_64-unknown-linux-gnu"; Executable = "ku"; ObjectFormat = "elf-x86_64"; Archive = "libku_native_tls.a"; Flavor = "gnu"; LinkContract = "rust-1.89.0-linux-gnu-v1"; Crt = "system-dynamic" } }
    if ($IsMacOS -and $architecture -eq "Arm64") { return @{ Target = "aarch64-apple-darwin"; Executable = "ku"; ObjectFormat = "macho-arm64"; Archive = "libku_native_tls.a"; Flavor = "apple"; LinkContract = "rust-1.89.0-darwin-v1"; Crt = "system-dynamic" } }
    throw "release packaging supports only x86_64 Windows, x86_64 Linux, and arm64 macOS; current architecture is '$architecture'"
}

function Test-PathSafeSemVer([string]$Value) {
    if ([string]::IsNullOrEmpty($Value) -or $Value.Length -gt 64) { return $false }
    $identifier = '(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)'
    $pattern = '^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-' + $identifier + '(?:\.' + $identifier + ')*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$'
    return [regex]::IsMatch($Value, $pattern, [Text.RegularExpressions.RegexOptions]::CultureInvariant)
}

function Test-PlainDirectory([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return $false }
    return (([IO.File]::GetAttributes($Path) -band [IO.FileAttributes]::ReparsePoint) -eq 0)
}

function Test-PlainFile([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    return (([IO.File]::GetAttributes($Path) -band [IO.FileAttributes]::ReparsePoint) -eq 0)
}

function Ensure-PlainDirectory([string]$Path) {
    if (Test-Path -LiteralPath $Path) {
        if (-not (Test-PlainDirectory $Path)) { throw "release path must be a plain directory: '$Path'" }
        return
    }
    [IO.Directory]::CreateDirectory($Path) | Out-Null
    if (-not $IsWindows) {
        $directoryMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute -bor [IO.UnixFileMode]::GroupRead -bor [IO.UnixFileMode]::GroupExecute -bor [IO.UnixFileMode]::OtherRead -bor [IO.UnixFileMode]::OtherExecute
        [IO.File]::SetUnixFileMode($Path, $directoryMode)
    }
    if (-not (Test-PlainDirectory $Path)) { throw "failed to create a plain release directory: '$Path'" }
    if (-not $IsWindows -and [IO.File]::GetUnixFileMode($Path) -ne $directoryMode) { throw "release directory did not receive the fixed 0755 mode" }
}

function Assert-PathUnder([string]$Path, [string]$Root) {
    $full = [IO.Path]::GetFullPath($Path)
    $rootFull = [IO.Path]::GetFullPath($Root).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    if (-not $full.StartsWith($rootFull + [IO.Path]::DirectorySeparatorChar, $pathComparison)) { throw "release operation escaped its allowed root: '$full'" }
    return $full
}

function New-OwnedDirectory([string]$Root, [string]$Prefix, [string]$OwnerToken) {
    if ($OwnerToken -cnotmatch '^[0-9a-f]{32}$') { throw "private release owner token is invalid" }
    Ensure-PlainDirectory $Root
    $path = Join-Path $Root ("$Prefix$PID-$([Guid]::NewGuid().ToString('N'))")
    $marker = Join-Path $path ".ku-release-owner"
    try {
        if (Test-Path -LiteralPath $path) { throw "private release path unexpectedly exists: '$path'" }
        [IO.Directory]::CreateDirectory($path) | Out-Null
        if (-not $IsWindows) { [IO.File]::SetUnixFileMode($path, [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute) }
        if (-not (Test-PlainDirectory $path)) { throw "private release path is not a plain directory: '$path'" }
        $stream = [IO.FileStream]::new($marker, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $bytes = [Text.Encoding]::ASCII.GetBytes($OwnerToken); $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
    }
    catch {
        try {
            if ((Test-PlainDirectory $path) -and (Test-PlainFile $marker)) { [IO.File]::Delete($marker) }
            if ((Test-PlainDirectory $path) -and [IO.Directory]::GetFileSystemEntries($path).Length -eq 0) { [IO.Directory]::Delete($path, $false) }
        }
        catch { }
        throw
    }
    return $path
}

function Read-OwnedMarkerToken([string]$Marker) {
    if (-not (Test-PlainFile $Marker)) { return $null }
    $stream = [IO.File]::Open($Marker, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -ne 32) { return $null }
        $bytes = [byte[]]::new(32); $offset = 0
        while ($offset -lt $bytes.Length) { $read = $stream.Read($bytes, $offset, $bytes.Length - $offset); if ($read -eq 0) { return $null }; $offset += $read }
        $token = [Text.Encoding]::ASCII.GetString($bytes)
        if ($token -cnotmatch '^[0-9a-f]{32}$') { return $null }
        return $token
    }
    finally { $stream.Dispose() }
}

function Assert-OwnedDirectory([string]$Path, [string]$Root, [string]$Prefix, [string]$OwnerToken) {
    $full = Assert-PathUnder $Path $Root
    if (-not (Test-PlainDirectory $full) -or -not [IO.Path]::GetFileName($full).StartsWith($Prefix, [StringComparison]::Ordinal)) { throw "refusing to operate on an unverified private release path: '$full'" }
    $marker = Join-Path $full ".ku-release-owner"
    if ($OwnerToken -cnotmatch '^[0-9a-f]{32}$' -or (Read-OwnedMarkerToken $marker) -cne $OwnerToken) { throw "private release ownership marker changed" }
}

function Remove-OwnedDirectory([string]$Path, [string]$Root, [string]$Prefix, [string]$OwnerToken) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    Assert-OwnedDirectory $Path $Root $Prefix $OwnerToken
    $full = [IO.Path]::GetFullPath($Path); $marker = Join-Path $full ".ku-release-owner"
    $directories = [Collections.Generic.Stack[string]]::new()
    $postorder = [Collections.Generic.Stack[string]]::new()
    $directories.Push($full)
    $count = 0
    while ($directories.Count -ne 0) {
        $directory = $directories.Pop(); $postorder.Push($directory)
        foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($directory)) {
            if ([IO.Path]::GetFullPath($entry).Equals($marker, $pathComparison)) { continue }
            $count++
            if ($count -gt $cleanupEntryLimit) { throw "private release tree exceeds $cleanupEntryLimit entries" }
            $attributes = [IO.File]::GetAttributes($entry)
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0 -and ($attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) { $directories.Push($entry) }
            elseif (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { [IO.Directory]::Delete($entry, $false) }
            else { [IO.File]::Delete($entry) }
        }
    }
    while ($postorder.Count -ne 0) {
        $directory = $postorder.Pop()
        if (-not $directory.Equals($full, $pathComparison)) { [IO.Directory]::Delete($directory, $false); continue }
        Assert-OwnedDirectory $full $Root $Prefix $OwnerToken
        $remaining = @([IO.Directory]::EnumerateFileSystemEntries($full))
        if ($remaining.Count -ne 1 -or -not [IO.Path]::GetFullPath($remaining[0]).Equals($marker, $pathComparison)) { throw "private release root changed during cleanup" }
        [IO.File]::Delete($marker)
        try { [IO.Directory]::Delete($full, $false) }
        catch {
            $failure = $_
            if ((Test-PlainDirectory $full) -and -not (Test-Path -LiteralPath $marker)) {
                $stream = [IO.FileStream]::new($marker, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                try { $bytes = [Text.Encoding]::ASCII.GetBytes($OwnerToken); $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
            }
            throw $failure
        }
    }
}

if (-not ("Ku.Release.BoundedProcess" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;
namespace Ku.Release {
    public sealed class BoundedProcessResult {
        public int ExitCode { get; internal set; }
        public bool TimedOut { get; internal set; }
        public bool OutputLimitExceeded { get; internal set; }
        public bool DescendantHeldOutputPipe { get; internal set; }
    }
    public static class BoundedProcess {
        private const uint JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000;
        private const int JobObjectExtendedLimitInformation = 9;

        [StructLayout(LayoutKind.Sequential)]
        private struct IO_COUNTERS {
            public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount;
            public ulong ReadTransferCount, WriteTransferCount, OtherTransferCount;
        }
        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
            public long PerProcessUserTimeLimit, PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize, MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass, SchedulingClass;
        }
        [StructLayout(LayoutKind.Sequential)]
        private struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            public JOBOBJECT_BASIC_LIMIT_INFORMATION BasicLimitInformation;
            public IO_COUNTERS IoInfo;
            public UIntPtr ProcessMemoryLimit, JobMemoryLimit, PeakProcessMemoryUsed, PeakJobMemoryUsed;
        }
        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        private static extern IntPtr CreateJobObject(IntPtr attributes, string name);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool SetInformationJobObject(IntPtr job, int infoClass, ref JOBOBJECT_EXTENDED_LIMIT_INFORMATION info, uint length);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
        [DllImport("kernel32.dll", SetLastError = true)]
        private static extern bool CloseHandle(IntPtr handle);
        [DllImport("libc", SetLastError = true)]
        private static extern int kill(int pid, int signal);

        private sealed class Budget {
            internal readonly Process Process; internal readonly long Maximum; internal readonly string GroupReadyPath;
            internal long Total; internal int Exceeded; internal int StopPumps;
            internal Budget(Process process, long maximum, string groupReadyPath) { Process = process; Maximum = maximum; GroupReadyPath = groupReadyPath; }
        }
        private static void KillLeader(Process process) { try { if (!process.HasExited) process.Kill(); } catch (InvalidOperationException) { } }

        private static IntPtr AssignKillOnCloseJob(Process process) {
            IntPtr job = CreateJobObject(IntPtr.Zero, null);
            if (job == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObject failed");
            var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, ref info, (uint)Marshal.SizeOf<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()) || !AssignProcessToJobObject(job, process.Handle)) {
                int error = Marshal.GetLastWin32Error(); CloseHandle(job); KillLeader(process);
                throw new Win32Exception(error, "failed to place release subprocess in its kill-on-close Job Object");
            }
            return job;
        }

        private static void SignalStart(string path) {
            using (var stream = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.Read)) { stream.WriteByte(1); stream.Flush(true); }
        }

        private static void KillUnixProcessGroup(int leaderPid, string readyPath) {
            if (OperatingSystem.IsWindows() || !File.Exists(readyPath)) return;
            if (kill(-leaderPid, 9) != 0) {
                int error = Marshal.GetLastWin32Error();
                if (error != 3) throw new Win32Exception(error, "failed to terminate release subprocess group");
            }
        }
        private static void StopForOutputLimit(Budget budget) {
            try {
                if (!OperatingSystem.IsWindows() && File.Exists(budget.GroupReadyPath)) KillUnixProcessGroup(budget.Process.Id, budget.GroupReadyPath);
                else KillLeader(budget.Process);
            } catch {
                // The bounded wait below remains authoritative and retries cleanup.
                try { KillLeader(budget.Process); } catch { }
            }
        }
        private static async Task Pump(Stream input, string path, Budget budget) {
            using (var output = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.Read, 8192, true)) {
                var buffer = new byte[8192];
                try {
                    while (true) {
                        int read = await input.ReadAsync(buffer, 0, buffer.Length).ConfigureAwait(false); if (read == 0) break;
                        long after = Interlocked.Add(ref budget.Total, read); long before = after - read;
                        int permitted = before >= budget.Maximum ? 0 : (int)Math.Min(read, budget.Maximum - before);
                        if (permitted != 0) await output.WriteAsync(buffer, 0, permitted).ConfigureAwait(false);
                        if (after > budget.Maximum && Interlocked.CompareExchange(ref budget.Exceeded, 1, 0) == 0) {
                            Interlocked.Exchange(ref budget.StopPumps, 1);
                            StopForOutputLimit(budget);
                        }
                    }
                } catch (ObjectDisposedException) when (Volatile.Read(ref budget.StopPumps) != 0 || Volatile.Read(ref budget.Exceeded) != 0) { }
                  catch (IOException) when (Volatile.Read(ref budget.StopPumps) != 0 || Volatile.Read(ref budget.Exceeded) != 0) { }
                await output.FlushAsync().ConfigureAwait(false);
            }
        }
        public static BoundedProcessResult Run(string fileName, string[] arguments, string workingDirectory, string stdoutPath, string stderrPath, string startGatePath, string groupReadyPath, int timeoutMilliseconds, long maximumOutputBytes) {
            var start = new ProcessStartInfo { FileName = fileName, WorkingDirectory = workingDirectory, UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true, CreateNoWindow = true, WindowStyle = ProcessWindowStyle.Hidden };
            foreach (string argument in arguments) start.ArgumentList.Add(argument);
            using (var process = new Process { StartInfo = start }) {
                if (!process.Start()) throw new InvalidOperationException("failed to start release subprocess");
                IntPtr job = IntPtr.Zero;
                try {
                    if (OperatingSystem.IsWindows()) { job = AssignKillOnCloseJob(process); SignalStart(startGatePath); }
                    var budget = new Budget(process, maximumOutputBytes, groupReadyPath);
                    Task stdout = Pump(process.StandardOutput.BaseStream, stdoutPath, budget); Task stderr = Pump(process.StandardError.BaseStream, stderrPath, budget);
                    bool timedOut = !process.WaitForExit(timeoutMilliseconds);
                    if (OperatingSystem.IsWindows()) { if (job != IntPtr.Zero) { CloseHandle(job); job = IntPtr.Zero; } }
                    else { KillUnixProcessGroup(process.Id, groupReadyPath); }
                    if (timedOut) KillLeader(process);
                    if (!process.WaitForExit(10000)) { KillLeader(process); throw new TimeoutException("release subprocess tree cleanup was not confirmed"); }
                    Task allOutput = Task.WhenAll(stdout, stderr); bool heldPipe = !allOutput.Wait(10000);
                    if (heldPipe) {
                        Interlocked.Exchange(ref budget.StopPumps, 1); KillLeader(process); process.StandardOutput.Close(); process.StandardError.Close();
                        if (!allOutput.Wait(10000)) throw new TimeoutException("release subprocess output cleanup was not confirmed");
                    }
                    if (allOutput.IsFaulted) throw new IOException("release subprocess output capture failed", allOutput.Exception);
                    return new BoundedProcessResult { ExitCode = process.ExitCode, TimedOut = timedOut, OutputLimitExceeded = Volatile.Read(ref budget.Exceeded) != 0, DescendantHeldOutputPipe = heldPipe };
                } finally {
                    if (job != IntPtr.Zero) CloseHandle(job);
                }
            }
        }
    }
}
'@
}

function New-ToolWrapper([string]$WorkRoot) {
    $path = Join-Path $WorkRoot "invoke-tool.ps1"
    $source = @'
param([Parameter(Mandatory = $true)][string]$Spec)
$ErrorActionPreference = "Stop"
$item = Get-Content -LiteralPath $Spec -Raw -Encoding UTF8 | ConvertFrom-Json
try {
    if ($IsWindows) {
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        while (-not (Test-Path -LiteralPath ([string]$item.startGate) -PathType Leaf)) {
            if ([DateTime]::UtcNow -ge $deadline) { throw "release Job Object start gate timed out" }
            Start-Sleep -Milliseconds 10
        }
    }
    else {
        Add-Type -TypeDefinition 'using System.Runtime.InteropServices; public static class KuReleaseUnixGroup { [DllImport("libc", SetLastError=true)] public static extern int setpgid(int pid, int pgid); }'
        if ([KuReleaseUnixGroup]::setpgid(0, 0) -ne 0) { throw "cannot create an isolated release process group" }
        $ready = [IO.FileStream]::new([string]$item.groupReady, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
        try { $ready.WriteByte(1); $ready.Flush($true) } finally { $ready.Dispose() }
    }
    & ([string]$item.tool) @([string[]]$item.arguments)
    if ($LASTEXITCODE -is [int]) { exit $LASTEXITCODE }
    exit 0
}
catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
'@
    [IO.File]::WriteAllText($path, $source, [Text.UTF8Encoding]::new($false)); return $path
}

function Invoke-BoundedTool([string]$Name, [string[]]$Arguments, [string]$WorkingDirectory, [string]$WorkRoot, [string]$LogName, [int]$TimeoutMilliseconds, [string]$Wrapper) {
    $command = Get-Command $Name -ErrorAction Stop | Select-Object -First 1
    if ($command.CommandType -ne [Management.Automation.CommandTypes]::Application -and $command.CommandType -ne [Management.Automation.CommandTypes]::ExternalScript) { throw "release tool '$Name' must resolve to an application or external script" }
    $pwsh = Get-Command "pwsh" -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $specPath = Join-Path $WorkRoot ("$LogName-$([Guid]::NewGuid().ToString('N')).json")
    $runId = [Guid]::NewGuid().ToString('N'); $startGatePath = Join-Path $WorkRoot "$runId.start"; $groupReadyPath = Join-Path $WorkRoot "$runId.group"
    $spec = [ordered]@{ tool = [string]$command.Source; arguments = @($Arguments); startGate = $startGatePath; groupReady = $groupReadyPath } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText($specPath, $spec, [Text.UTF8Encoding]::new($false))
    $stdoutPath = Join-Path $WorkRoot "$LogName-$runId.stdout"; $stderrPath = Join-Path $WorkRoot "$LogName-$runId.stderr"
    $result = [Ku.Release.BoundedProcess]::Run([string]$pwsh.Source, @("-NoLogo", "-NoProfile", "-NonInteractive", "-File", $Wrapper, "-Spec", $specPath), $WorkingDirectory, $stdoutPath, $stderrPath, $startGatePath, $groupReadyPath, $TimeoutMilliseconds, $processOutputLimit)
    $stdout = [IO.File]::ReadAllText($stdoutPath, $outputUtf8); $stderr = [IO.File]::ReadAllText($stderrPath, $outputUtf8)
    $output = if ($stdout.Length -eq 0) { $stderr } elseif ($stderr.Length -eq 0) { $stdout } else { "$stdout`n$stderr" }
    if ($result.OutputLimitExceeded) { throw "$Name output exceeded $processOutputLimit bytes; its process tree was terminated" }
    if ($result.TimedOut) { throw "$Name exceeded its $TimeoutMilliseconds-ms deadline; its process tree was terminated" }
    if ($result.DescendantHeldOutputPipe) { throw "$Name left an inherited output pipe open after exit; the release command was rejected" }
    return [pscustomobject]@{ ExitCode = $result.ExitCode; Output = $output }
}

function Invoke-CheckedTool([string]$Name, [string[]]$Arguments, [string]$WorkingDirectory, [string]$WorkRoot, [string]$LogName, [int]$TimeoutMilliseconds, [string]$Wrapper) {
    $result = Invoke-BoundedTool $Name $Arguments $WorkingDirectory $WorkRoot $LogName $TimeoutMilliseconds $Wrapper
    if ($result.ExitCode -ne 0) { throw "$Name failed with exit code $($result.ExitCode):`n$($result.Output)" }
    return $result.Output
}

function Read-Bytes([IO.FileStream]$Stream, [long]$Offset, [int]$Count) {
    if ($Offset -lt 0 -or $Count -lt 0 -or $Offset + $Count -gt $Stream.Length) { throw "binary release artifact is truncated" }
    $Stream.Position = $Offset; $buffer = [byte[]]::new($Count); $readTotal = 0
    while ($readTotal -lt $Count) { $read = $Stream.Read($buffer, $readTotal, $Count - $readTotal); if ($read -eq 0) { throw "binary release artifact is truncated" }; $readTotal += $read }
    return ,$buffer
}

function Get-U16([byte[]]$Bytes, [int]$Offset) { return [uint16](([uint32]$Bytes[$Offset]) -bor (([uint32]$Bytes[$Offset + 1]) -shl 8)) }
function Get-U32([byte[]]$Bytes, [int]$Offset) { return [uint32](([uint32]$Bytes[$Offset]) -bor (([uint32]$Bytes[$Offset + 1]) -shl 8) -bor (([uint32]$Bytes[$Offset + 2]) -shl 16) -bor (([uint32]$Bytes[$Offset + 3]) -shl 24)) }

function Assert-ExecutableArchitecture([string]$Path, [hashtable]$Contract) {
    if (-not (Test-PlainFile $Path)) { throw "missing plain release executable: '$Path'" }
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -eq 0 -or $stream.Length -gt 512MB) { throw "release executable has an invalid size" }
        if ($Contract.ObjectFormat -eq "coff-x86_64") {
            $dos = Read-Bytes $stream 0 64; if ($dos[0] -ne 0x4d -or $dos[1] -ne 0x5a) { throw "release executable is not PE" }
            $peOffset = [long](Get-U32 $dos 0x3c)
            if ($peOffset -lt 64 -or $peOffset -gt 1MB -or $peOffset + 24 -gt $stream.Length) { throw "release PE header offset is invalid" }
            $pe = Read-Bytes $stream $peOffset 24
            if ($pe[0] -ne 0x50 -or $pe[1] -ne 0x45 -or $pe[2] -ne 0 -or $pe[3] -ne 0 -or (Get-U16 $pe 4) -ne 0x8664 -or ((Get-U16 $pe 22) -band 0x0002) -eq 0) { throw "release executable is not an x86_64 PE executable" }
        }
        elseif ($Contract.ObjectFormat -eq "elf-x86_64") {
            $header = Read-Bytes $stream 0 20
            if ($header[0] -ne 0x7f -or $header[1] -ne 0x45 -or $header[2] -ne 0x4c -or $header[3] -ne 0x46 -or $header[4] -ne 2 -or $header[5] -ne 1 -or $header[6] -ne 1 -or (Get-U16 $header 18) -ne 62 -or (Get-U16 $header 16) -notin @(2, 3)) { throw "release executable is not an x86_64 ELF executable" }
        }
        else {
            $header = Read-Bytes $stream 0 16
            if ($header[0] -ne 0xcf -or $header[1] -ne 0xfa -or $header[2] -ne 0xed -or $header[3] -ne 0xfe -or (Get-U32 $header 4) -ne 0x0100000c -or (Get-U32 $header 12) -ne 2) { throw "release executable is not an arm64 Mach-O executable" }
        }
    }
    finally { $stream.Dispose() }
    if (-not $IsWindows) { $mode = [IO.File]::GetUnixFileMode($Path); $execute = [IO.UnixFileMode]::UserExecute -bor [IO.UnixFileMode]::GroupExecute -bor [IO.UnixFileMode]::OtherExecute; if (($mode -band $execute) -eq 0) { throw "release executable has no Unix execute bit" } }
}

function Assert-RlibArchitecture([string]$Path, [hashtable]$Contract) {
    if (-not (Test-PlainFile $Path)) { throw "missing plain release library: '$Path'" }
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -lt 68 -or $stream.Length -gt 512MB -or [Text.Encoding]::ASCII.GetString((Read-Bytes $stream 0 8)) -cne "!<arch>`n") { throw "libku.rlib is not a bounded complete archive" }
        $offset = 8L; $members = 0; $matchingObjects = 0
        while ($offset -lt $stream.Length) {
            if ($offset + 60 -gt $stream.Length) { throw "libku.rlib has a truncated member header" }
            $header = Read-Bytes $stream $offset 60
            if ($header[58] -ne 0x60 -or $header[59] -ne 0x0a) { throw "libku.rlib has an invalid member header" }
            $sizeText = [Text.Encoding]::ASCII.GetString($header, 48, 10).Trim()
            if ($sizeText -notmatch '^(0|[1-9][0-9]*)$') { throw "libku.rlib has a non-canonical member size" }
            $size = [uint64]$sizeText; $dataOffset = $offset + 60
            if ($size -gt [uint64]($stream.Length - $dataOffset)) { throw "libku.rlib member exceeds the archive" }
            $members++; if ($members -gt $cleanupEntryLimit) { throw "libku.rlib has too many members" }
            $bodyOffset = $dataOffset; $bodySize = [long]$size; $name = [Text.Encoding]::ASCII.GetString($header, 0, 16).Trim()
            if ($name.StartsWith("#1/", [StringComparison]::Ordinal)) {
                $nameLengthText = $name.Substring(3); if ($nameLengthText -notmatch '^[1-9][0-9]*$') { throw "libku.rlib has an invalid extended name" }
                $nameLength = [long][uint64]$nameLengthText; if ($nameLength -gt $bodySize) { throw "libku.rlib extended name exceeds its member" }; $bodyOffset += $nameLength; $bodySize -= $nameLength
            }
            if ($bodySize -ge 8) {
                $prefix = Read-Bytes $stream $bodyOffset ([Math]::Min(20, [int]$bodySize))
                $isElf = $prefix.Length -ge 20 -and $prefix[0] -eq 0x7f -and $prefix[1] -eq 0x45 -and $prefix[2] -eq 0x4c -and $prefix[3] -eq 0x46
                $isMach = $prefix.Length -ge 16 -and $prefix[0] -eq 0xcf -and $prefix[1] -eq 0xfa -and $prefix[2] -eq 0xed -and $prefix[3] -eq 0xfe
                $machine = if ($prefix.Length -ge 2) { Get-U16 $prefix 0 } else { 0 }
                $isCoff = $machine -in @(0x014c, 0x8664, 0xaa64, 0x01c0, 0x01c4)
                $isBigObj = $prefix.Length -ge 8 -and $prefix[0] -eq 0 -and $prefix[1] -eq 0 -and $prefix[2] -eq 0xff -and $prefix[3] -eq 0xff
                if ($isElf) { if ($Contract.ObjectFormat -ne "elf-x86_64" -or $prefix[4] -ne 2 -or $prefix[5] -ne 1 -or (Get-U16 $prefix 18) -ne 62) { throw "libku.rlib contains an object for the wrong target" }; $matchingObjects++ }
                elseif ($isMach) { if ($Contract.ObjectFormat -ne "macho-arm64" -or (Get-U32 $prefix 4) -ne 0x0100000c) { throw "libku.rlib contains an object for the wrong target" }; $matchingObjects++ }
                elseif ($isCoff -or $isBigObj) { $coffMachine = if ($isBigObj) { Get-U16 $prefix 6 } else { $machine }; if ($Contract.ObjectFormat -ne "coff-x86_64" -or $coffMachine -ne 0x8664) { throw "libku.rlib contains an object for the wrong target" }; $matchingObjects++ }
            }
            $offset = $dataOffset + [long]$size
            if (($size -band 1) -ne 0) { if ($offset -ge $stream.Length -or (Read-Bytes $stream $offset 1)[0] -ne 0x0a) { throw "libku.rlib has invalid member padding" }; $offset++ }
        }
        if ($offset -ne $stream.Length -or $matchingObjects -eq 0) { throw "libku.rlib contains no object for '$($Contract.Target)'" }
    }
    finally { $stream.Dispose() }
}

function Get-NativeTlsBuildId([string]$BuilderPath) {
    if (-not (Test-PlainFile $BuilderPath)) { throw "native TLS pack builder must be a plain file" }
    $info = Get-Item -LiteralPath $BuilderPath
    if ($info.Length -eq 0 -or $info.Length -gt 1MB) { throw "native TLS pack builder has an invalid size" }
    $text = [IO.File]::ReadAllText($BuilderPath, $utf8)
    $matches = [regex]::Matches($text, '(?m)^\$kuTlsBuildId = "(?<value>[^"\r\n]+)"\r?$')
    if ($matches.Count -ne 1) { throw "cannot read exactly one native TLS build id from the pack builder" }
    return $matches[0].Groups['value'].Value
}

function New-NativeTlsArchiveVerifier([string]$Repo, [string]$WorkRoot, [string]$Wrapper) {
    $moduleSource = Join-Path $Repo "src/native_tls_archive.rs"
    $moduleCopy = Join-Path $WorkRoot "native_tls_archive.rs"
    Copy-PlainFile $moduleSource $moduleCopy 1MB
    $mainSource = Join-Path $WorkRoot "validate_tls_archive.rs"
    $source = @'
mod native_tls_archive;
use native_tls_archive::{validate, NativeTlsArchiveFormat};
use std::{env, fs::File, process};
fn main() {
    let args = env::args().collect::<Vec<_>>();
    if args.len() != 4 { eprintln!("usage: validate <archive> <format> <build-id>"); process::exit(2); }
    let format = match args[2].as_str() {
        "coff-x86_64" => NativeTlsArchiveFormat::CoffX86_64,
        "elf-x86_64" => NativeTlsArchiveFormat::ElfX86_64,
        "macho-arm64" => NativeTlsArchiveFormat::MachOArm64,
        _ => { eprintln!("unknown archive format"); process::exit(2); }
    };
    let mut file = File::open(&args[1]).unwrap_or_else(|error| { eprintln!("open archive: {error}"); process::exit(2); });
    let len = file.metadata().unwrap_or_else(|error| { eprintln!("archive metadata: {error}"); process::exit(2); }).len();
    if let Err(error) = validate(&mut file, len, format, args[3].as_bytes()) { eprintln!("{error}"); process::exit(1); }
}
'@
    [IO.File]::WriteAllText($mainSource, $source, [Text.UTF8Encoding]::new($false))
    $binary = Join-Path $WorkRoot $(if ($IsWindows) { "validate_tls_archive.exe" } else { "validate_tls_archive" })
    [void](Invoke-CheckedTool "rustc" @("+1.89.0", "--edition=2021", "-C", "panic=abort", "-o", $binary, $mainSource) $WorkRoot $WorkRoot "archive-validator-build" 30000 $Wrapper)
    if (-not (Test-PlainFile $binary)) { throw "native TLS archive validator was not produced" }
    return $binary
}

function Invoke-NativeTlsArchiveValidation([string]$Archive, [string]$Format, [string]$BuildId) {
    if ([string]::IsNullOrEmpty([string]$script:archiveVerifier) -or [string]::IsNullOrEmpty([string]$script:releaseWorkRoot) -or [string]::IsNullOrEmpty([string]$script:toolWrapper)) { throw "native TLS archive validator is not initialized" }
    [void](Invoke-CheckedTool ([string]$script:archiveVerifier) @($Archive, $Format, $BuildId) ([string]$script:releaseWorkRoot) ([string]$script:releaseWorkRoot) "archive-validator-run" 30000 ([string]$script:toolWrapper))
}

function Assert-TlsPack([string]$Leaf, [hashtable]$Contract, [string]$BuildId) {
    if (-not (Test-PlainDirectory $Leaf) -or -not (Test-PlainDirectory (Join-Path $Leaf "include")) -or -not (Test-PlainDirectory (Join-Path $Leaf "lib"))) { throw "native TLS pack and its include/lib paths must be plain directories" }
    $manifestPath = Join-Path $Leaf "manifest.kutls"; $headerPath = Join-Path $Leaf "include/ku_native_tls.h"; $archivePath = Join-Path $Leaf ("lib/" + $Contract.Archive)
    foreach ($path in @($manifestPath, $headerPath, $archivePath)) { if (-not (Test-PlainFile $path)) { throw "native TLS pack file must be plain and present: '$path'" } }
    $manifestInfo = Get-Item -LiteralPath $manifestPath
    if ($manifestInfo.Length -eq 0 -or $manifestInfo.Length -gt 64KB) { throw "native TLS manifest size is invalid" }
    $bytes = [IO.File]::ReadAllBytes($manifestPath)
    if (@($bytes | Where-Object { $_ -gt 127 -or $_ -eq 0 -or $_ -eq 13 }).Count -ne 0) { throw "native TLS manifest must be canonical ASCII" }
    $text = [Text.Encoding]::ASCII.GetString($bytes)
    if (-not $text.EndsWith("`n") -or $text.EndsWith("`n`n")) { throw "native TLS manifest must have one trailing LF" }
    $names = @("format", "target", "flavor", "object_format", "abi_version", "panic", "build_id", "archive_size", "archive_sha256", "header_size", "header_sha256", "link_contract", "crt", "runtime_dependency")
    $lines = $text.TrimEnd("`n").Split("`n")
    if ($lines.Count -ne $names.Count) { throw "native TLS manifest has the wrong field count" }
    $fields = @{}
    for ($index = 0; $index -lt $names.Count; $index++) {
        $parts = $lines[$index].Split('=', 2)
        if ($parts.Count -ne 2 -or $parts[0] -cne $names[$index] -or $fields.ContainsKey($parts[0])) { throw "native TLS manifest fields are unknown, duplicate, or out of order" }
        $fields[$parts[0]] = $parts[1]
    }
    $expected = @{ format = "ku-native-tls-pack-v1"; target = $Contract.Target; flavor = $Contract.Flavor; object_format = $Contract.ObjectFormat; abi_version = "1"; panic = "unwind"; build_id = $BuildId; link_contract = $Contract.LinkContract; crt = $Contract.Crt; runtime_dependency = "none" }
    foreach ($name in $expected.Keys) { if ($fields[$name] -cne $expected[$name]) { throw "native TLS manifest field '$name' violates the release contract" } }
    foreach ($name in @("archive_size", "header_size")) { if ($fields[$name] -notmatch '^[1-9][0-9]*$') { throw "native TLS manifest field '$name' is not canonical" } }
    foreach ($name in @("archive_sha256", "header_sha256")) { if ($fields[$name] -cnotmatch '^[0-9a-f]{64}$') { throw "native TLS manifest field '$name' is not lowercase SHA-256" } }
    $archive = Get-Item -LiteralPath $archivePath; $header = Get-Item -LiteralPath $headerPath
    if ($archive.Length -gt 128MB -or $header.Length -gt 64KB -or [uint64]$fields.archive_size -ne $archive.Length -or [uint64]$fields.header_size -ne $header.Length) { throw "native TLS pack sizes violate its manifest" }
    if ((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant() -cne $fields.archive_sha256 -or (Get-FileHash -LiteralPath $headerPath -Algorithm SHA256).Hash.ToLowerInvariant() -cne $fields.header_sha256) { throw "native TLS pack hashes violate its manifest" }
    if ([string]::IsNullOrEmpty([string]$script:expectedTlsHeaderHash) -or (Get-FileHash -LiteralPath $headerPath -Algorithm SHA256).Hash.ToLowerInvariant() -cne [string]$script:expectedTlsHeaderHash) { throw "native TLS header differs from the repository ABI header" }
    $archiveStream = [IO.File]::OpenRead($archivePath)
    try { if ([Text.Encoding]::ASCII.GetString((Read-Bytes $archiveStream 0 8)) -cne "!<arch>`n") { throw "native TLS library is not a complete archive" } } finally { $archiveStream.Dispose() }
    Invoke-NativeTlsArchiveValidation $archivePath ([string]$Contract.ObjectFormat) $BuildId
}

function Test-PackagedTlsConsumer([string]$Bundle, [hashtable]$Contract, [string]$WorkRoot, [string]$Wrapper) {
    $ku = Join-Path $Bundle $Contract.Executable
    $pack = Join-Path $Bundle 'native-tls'; $retiredPack = Join-Path $WorkRoot 'retired-native-tls'
    $consumerRoot = Join-Path $WorkRoot 'packaged-tls-consumer'; Ensure-PlainDirectory $consumerRoot
    $source = Join-Path $consumerRoot 'main.ku'
    $program = @'
import net from "std.net"
fn main(): null! {
    try {
        client = net.client({ host: "127.0.0.1", port: 9, tls: true, tls_server_name: "localhost", connect_timeout_ms: 100, read_timeout_ms: 100, write_timeout_ms: 100, max_read_bytes: 32 })?
        client.close()
        println("packaged tls consumer ok")
    } catch(err) {
        println("packaged tls consumer ok")
    }
    return ok(null)
}
'@
    [IO.File]::WriteAllText($source, $program, [Text.UTF8Encoding]::new($false))
    $executable = Join-Path $consumerRoot $(if ($IsWindows) { 'consumer.exe' } else { 'consumer' })
    $hadPackEnvironment = Test-Path -LiteralPath 'Env:\KU_NATIVE_TLS_PACK'; $savedPack = $env:KU_NATIVE_TLS_PACK
    $hadRequiredEnvironment = Test-Path -LiteralPath 'Env:\KU_NATIVE_TLS_LINK_REQUIRED'; $savedRequired = $env:KU_NATIVE_TLS_LINK_REQUIRED
    $packMoved = $false
    try {
        $env:KU_NATIVE_TLS_PACK = $null; $env:KU_NATIVE_TLS_LINK_REQUIRED = $null
        [void](Invoke-CheckedTool $ku @('build', '--native', '--offline', $source, '-o', $executable) $consumerRoot $WorkRoot 'packaged-consumer-build' 180000 $Wrapper)
        Assert-ExecutableArchitecture $executable $Contract
        [IO.File]::Delete($source)
        $generated = Join-Path $consumerRoot '.ku'
        if (Test-Path -LiteralPath $generated) { if (-not (Test-PlainDirectory $generated)) { throw "packaged consumer generated path is not a plain directory" }; [IO.Directory]::Move($generated, (Join-Path $consumerRoot 'retired-generated-source')) }
        if (-not (Test-PlainDirectory $pack) -or (Test-Path -LiteralPath $retiredPack)) { throw "packaged consumer could not isolate its native TLS pack before execution" }
        [IO.Directory]::Move($pack, $retiredPack); $packMoved = $true
        $output = Invoke-CheckedTool $executable @() $consumerRoot $WorkRoot 'packaged-consumer-run' 30000 $Wrapper
        if ($output.Replace("`r", '').TrimEnd("`n") -cne 'packaged tls consumer ok') { throw "packaged TLS consumer returned unexpected output: '$output'" }
    }
    finally {
        if ($hadPackEnvironment) { $env:KU_NATIVE_TLS_PACK = $savedPack } else { $env:KU_NATIVE_TLS_PACK = $null }
        if ($hadRequiredEnvironment) { $env:KU_NATIVE_TLS_LINK_REQUIRED = $savedRequired } else { $env:KU_NATIVE_TLS_LINK_REQUIRED = $null }
        if ($packMoved -and -not (Test-Path -LiteralPath $pack) -and (Test-PlainDirectory $retiredPack)) { [IO.Directory]::Move($retiredPack, $pack) }
    }
    if (-not (Test-PlainDirectory $pack)) { throw "packaged TLS consumer failed to restore the candidate pack after its source-free run" }
}

function Copy-PlainFile([string]$Source, [string]$Destination, [long]$Limit) {
    if (-not (Test-PlainFile $Source)) { throw "release input must be a plain file: '$Source'" }
    $sourceStream = [IO.File]::Open($Source, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($sourceStream.Length -eq 0 -or $sourceStream.Length -gt $Limit) { throw "release input has an invalid size: '$Source'" }
        Ensure-PlainDirectory (Split-Path -Parent $Destination)
        $destinationStream = [IO.FileStream]::new($Destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $sourceStream.CopyTo($destinationStream, 65536); $destinationStream.Flush($true) } finally { $destinationStream.Dispose() }
        if (-not $IsWindows) {
            $fileMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::GroupRead -bor [IO.UnixFileMode]::OtherRead
            [IO.File]::SetUnixFileMode($Destination, $fileMode)
            if (-not (Test-PlainFile $Destination) -or [IO.File]::GetUnixFileMode($Destination) -ne $fileMode) { throw "release data file did not receive the fixed 0644 mode" }
        }
    }
    finally { $sourceStream.Dispose() }
}

function Copy-ExecutableFile([string]$Source, [string]$Destination, [long]$Limit) {
    if (-not $IsWindows) {
        $sourceMode = [IO.File]::GetUnixFileMode($Source)
        $executeMask = [IO.UnixFileMode]::UserExecute -bor [IO.UnixFileMode]::GroupExecute -bor [IO.UnixFileMode]::OtherExecute
        if (($sourceMode -band $executeMask) -eq 0) { throw "release executable source has no execute bit: '$Source'" }
    }
    Copy-PlainFile $Source $Destination $Limit
    if (-not $IsWindows) {
        $releaseMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute -bor [IO.UnixFileMode]::GroupRead -bor [IO.UnixFileMode]::GroupExecute -bor [IO.UnixFileMode]::OtherRead -bor [IO.UnixFileMode]::OtherExecute
        [IO.File]::SetUnixFileMode($Destination, $releaseMode)
        if (-not (Test-PlainFile $Destination) -or [IO.File]::GetUnixFileMode($Destination) -ne $releaseMode) { throw "release executable did not receive the fixed 0755 mode" }
    }
}

function Copy-ExtensionSource([string]$Source, [string]$Destination) {
    if (-not (Test-PlainDirectory $Source)) { throw "VS Code extension source must be a plain directory" }
    Ensure-PlainDirectory $Destination
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Source); $entries = 0; $total = 0L
    while ($pending.Count -ne 0) {
        $directory = $pending.Pop()
        foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($directory)) {
            $entries++; if ($entries -gt 10000) { throw "VS Code extension source has too many entries" }
            $relative = [IO.Path]::GetRelativePath($Source, $entry); $first = $relative.Split([IO.Path]::DirectorySeparatorChar, 2)[0]
            if ($first -in @("node_modules", "out") -or $relative.EndsWith(".vsix", [StringComparison]::OrdinalIgnoreCase)) { continue }
            $attributes = [IO.File]::GetAttributes($entry)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "VS Code extension source contains a symlink/reparse point: '$entry'" }
            $target = Join-Path $Destination $relative
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { Ensure-PlainDirectory $target; $pending.Push($entry) }
            else { $length = (Get-Item -LiteralPath $entry).Length; $total += $length; if ($total -gt 64MB) { throw "VS Code extension source exceeds 64 MiB" }; Copy-PlainFile $entry $target 64MB }
        }
    }
}

function Copy-TlsPack([string]$Source, [string]$Destination, [hashtable]$Contract, [string]$BuildId) {
    Assert-TlsPack $Source $Contract $BuildId
    Ensure-PlainDirectory $Destination; Ensure-PlainDirectory (Join-Path $Destination "include"); Ensure-PlainDirectory (Join-Path $Destination "lib")
    Copy-PlainFile (Join-Path $Source "manifest.kutls") (Join-Path $Destination "manifest.kutls") 64KB
    Copy-PlainFile (Join-Path $Source "include/ku_native_tls.h") (Join-Path $Destination "include/ku_native_tls.h") 64KB
    Copy-PlainFile (Join-Path $Source ("lib/" + $Contract.Archive)) (Join-Path $Destination ("lib/" + $Contract.Archive)) 128MB
    Assert-TlsPack $Destination $Contract $BuildId
}

function Read-VsixEntryUtf8([object]$Entry, [long]$Limit, [string]$Label) {
    if ($Entry.Length -eq 0 -or $Entry.Length -gt $Limit) { throw "VSIX $Label has an invalid uncompressed size" }
    $stream = $Entry.Open()
    try {
        $bytes = [byte[]]::new([int]$Entry.Length); $offset = 0
        while ($offset -lt $bytes.Length) { $read = $stream.Read($bytes, $offset, $bytes.Length - $offset); if ($read -eq 0) { throw "VSIX $Label is truncated" }; $offset += $read }
        if ($stream.ReadByte() -ne -1) { throw "VSIX $Label exceeds its declared size" }
        return $utf8.GetString($bytes)
    }
    finally { $stream.Dispose() }
}

function Resolve-VsixRelativeModule([string]$From, [string]$Specifier, [Collections.Generic.Dictionary[string, object]]$Entries) {
    if ($Specifier -notmatch '^\.\.?/' -or $Specifier.Contains('\') -or $Specifier.Contains(':') -or $Specifier.Contains([char]0)) { throw "VSIX contains an unsafe relative require '$Specifier'" }
    $slash = $From.LastIndexOf('/'); if ($slash -lt 0) { throw "VSIX module has no package directory" }
    $segments = [Collections.Generic.List[string]]::new()
    foreach ($segment in $From.Substring(0, $slash).Split('/')) { [void]$segments.Add($segment) }
    foreach ($segment in $Specifier.Split('/')) {
        if ($segment -eq '.' -or $segment.Length -eq 0) { continue }
        if ($segment -eq '..') { if ($segments.Count -le 1) { throw "VSIX relative require escapes the extension root" }; $segments.RemoveAt($segments.Count - 1); continue }
        [void]$segments.Add($segment)
    }
    $base = [string]::Join('/', $segments)
    foreach ($candidate in @($base, "$base.js", "$base.json", "$base/index.js")) {
        if ($Entries.ContainsKey($candidate) -and -not $candidate.EndsWith('/', [StringComparison]::Ordinal)) { return $candidate }
    }
    throw "VSIX relative require '$Specifier' from '$From' has no packaged module"
}

function Assert-Vsix([string]$Path, [string]$Version) {
    if (-not (Test-PlainFile $Path)) { throw "VS Code extension must be a plain VSIX file" }
    $info = Get-Item -LiteralPath $Path
    if ($info.Length -eq 0 -or $info.Length -gt 128MB) { throw "VS Code extension has an invalid compressed size" }
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        $archive = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Read, $false)
        try {
            $entries = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::Ordinal)
            $folded = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase); $count = 0; $total = 0L
            foreach ($entry in $archive.Entries) {
                $count++; if ($count -gt 10000) { throw "VSIX contains too many ZIP entries" }
                $name = [string]$entry.FullName
                if ($name.Length -eq 0 -or $name.Length -gt 512 -or $name.Contains('\') -or $name.StartsWith('/', [StringComparison]::Ordinal) -or $name.Contains(':') -or $name.Contains([char]0) -or $name.Contains('//')) { throw "VSIX contains an unsafe ZIP entry name" }
                $parts = $name.Split('/'); $last = $parts.Count - 1
                for ($index = 0; $index -lt $parts.Count; $index++) { if (($parts[$index].Length -eq 0 -and $index -ne $last) -or $parts[$index] -in @('.', '..')) { throw "VSIX ZIP entry attempts path traversal" } }
                if (-not $entries.TryAdd($name, $entry) -or -not $folded.Add($name)) { throw "VSIX contains a duplicate or case-colliding ZIP entry" }
                if ($entry.Length -lt 0 -or $entry.Length -gt 64MB -or $entry.CompressedLength -lt 0 -or $entry.CompressedLength -gt 128MB) { throw "VSIX ZIP entry exceeds its size limit" }
                $total += $entry.Length; if ($total -gt 256MB) { throw "VSIX exceeds its total uncompressed size limit" }
                if ($name.EndsWith('/', [StringComparison]::Ordinal) -and $entry.Length -ne 0) { throw "VSIX directory entry contains data" }
            }
            if (-not $entries.ContainsKey('extension/package.json')) { throw "VSIX is missing extension/package.json" }
            $packageText = Read-VsixEntryUtf8 $entries['extension/package.json'] 1MB 'package.json'
            try { $package = $packageText | ConvertFrom-Json -AsHashtable }
            catch { throw "VSIX extension/package.json is invalid JSON: $($_.Exception.Message)" }
            if ([string]$package['version'] -cne $Version -or [string]$package['main'] -cne './out/language.js') { throw "VSIX package version or main entry violates the release contract" }
            $main = 'extension/out/language.js'
            if (-not $entries.ContainsKey($main)) { throw "VSIX package main file is missing" }
            $pending = [Collections.Generic.Queue[string]]::new(); $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal); $pending.Enqueue($main)
            $requirePattern = @'
\brequire\s*\(\s*["'](?<path>\.{1,2}/[^"']+)["']\s*\)
'@
            while ($pending.Count -ne 0) {
                $module = $pending.Dequeue(); if (-not $seen.Add($module)) { continue }
                if ($seen.Count -gt 1000) { throw "VSIX JavaScript require closure is too large" }
                $moduleText = Read-VsixEntryUtf8 $entries[$module] 4MB "module '$module'"
                foreach ($match in [regex]::Matches($moduleText, $requirePattern, [Text.RegularExpressions.RegexOptions]::CultureInvariant)) {
                    $dependency = Resolve-VsixRelativeModule $module $match.Groups['path'].Value $entries
                    if ($dependency.EndsWith('.js', [StringComparison]::OrdinalIgnoreCase)) { $pending.Enqueue($dependency) }
                }
            }
        }
        finally { $archive.Dispose() }
    }
    catch [IO.InvalidDataException] { throw "VS Code extension is not a valid bounded ZIP package: $($_.Exception.Message)" }
    finally { $stream.Dispose() }
}

function Assert-Bundle([string]$Bundle, [hashtable]$Contract, [string]$Version, [string]$BuildId) {
    if (-not (Test-PlainDirectory $Bundle)) { throw "release bundle must be a plain directory" }
    $vsixName = "ku-language-$Version.vsix"
    $expectedFiles = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($relative in @($Contract.Executable, "libku.rlib", $vsixName, "native-tls/v1/$($Contract.Target)/manifest.kutls", "native-tls/v1/$($Contract.Target)/include/ku_native_tls.h", "native-tls/v1/$($Contract.Target)/lib/$($Contract.Archive)")) { [void]$expectedFiles.Add($relative.Replace('\', '/')) }
    if ($IsWindows -and (Test-PlainFile (Join-Path $Bundle "ku.pdb"))) { [void]$expectedFiles.Add("ku.pdb") }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal); $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Bundle); $entries = 0
    while ($pending.Count -ne 0) {
        $directory = $pending.Pop()
        foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($directory)) {
            $entries++; if ($entries -gt 1000) { throw "release bundle has too many entries" }
            $attributes = [IO.File]::GetAttributes($entry)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "release bundle contains a symlink/reparse point: '$entry'" }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { $pending.Push($entry) } else { [void]$seen.Add([IO.Path]::GetRelativePath($Bundle, $entry).Replace('\', '/')) }
        }
    }
    if ($seen.Count -ne $expectedFiles.Count -or @($seen | Where-Object { -not $expectedFiles.Contains($_) }).Count -ne 0) { throw "release bundle contains missing or unexpected files" }
    Assert-ExecutableArchitecture (Join-Path $Bundle $Contract.Executable) $Contract
    Assert-RlibArchitecture (Join-Path $Bundle "libku.rlib") $Contract
    Assert-Vsix (Join-Path $Bundle $vsixName) $Version
    Assert-TlsPack (Join-Path $Bundle "native-tls/v1/$($Contract.Target)") $Contract $BuildId
}

function Copy-BundleTree([string]$Source, [string]$Destination, [hashtable]$Contract) {
    Ensure-PlainDirectory $Destination
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Source); $entries = 0; $total = 0L
    while ($pending.Count -ne 0) {
        $directory = $pending.Pop()
        foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($directory)) {
            $entries++; if ($entries -gt 1000) { throw "release bundle source has too many entries" }
            $relative = [IO.Path]::GetRelativePath($Source, $entry); $target = Join-Path $Destination $relative; $attributes = [IO.File]::GetAttributes($entry)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "release bundle source contains a symlink/reparse point" }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { Ensure-PlainDirectory $target; $pending.Push($entry) }
            elseif ($relative.Replace('\', '/') -ceq [string]$Contract.Executable) { Copy-ExecutableFile $entry $target 512MB }
            else { Copy-PlainFile $entry $target 512MB }
            if (($attributes -band [IO.FileAttributes]::Directory) -eq 0) { $total += (Get-Item -LiteralPath $entry).Length; if ($total -gt 1GB) { throw "release bundle source exceeds 1 GiB" } }
        }
    }
}

function Assert-PlainBoundedTree([string]$Root) {
    if (-not (Test-PlainDirectory $Root)) { throw "existing release target is not a plain directory" }
    $pending = [Collections.Generic.Stack[string]]::new(); $pending.Push($Root); $entries = 0
    while ($pending.Count -ne 0) {
        foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($pending.Pop())) {
            $entries++; if ($entries -gt 1000) { throw "existing release target has too many entries" }
            $attributes = [IO.File]::GetAttributes($entry)
            if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "existing release target contains a symlink/reparse point" }
            if (($attributes -band [IO.FileAttributes]::Directory) -ne 0) { $pending.Push($entry) }
            elseif ((Get-Item -LiteralPath $entry).Length -gt 512MB) { throw "existing release target contains an oversized file" }
        }
    }
}

function Enter-ExclusivePublishLock([string]$Root, [string]$Name) {
    if ($Name -notmatch '^[0-9A-Za-z._+-]{1,192}$') { throw "release lock name is not path-safe" }
    Ensure-PlainDirectory $Root
    $path = Join-Path $Root $Name
    if ((Test-Path -LiteralPath $path) -and -not (Test-PlainFile $path)) { throw "release lock is not a plain file: '$path'" }
    try { $stream = [IO.FileStream]::new($path, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
    catch [IO.IOException] { throw "another publisher already holds the '$Name' release lock" }
    try {
        if (-not (Test-PlainFile $path)) { throw "release lock became a symlink/reparse point" }
        if (-not $IsWindows) {
            $lockMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::GroupRead -bor [IO.UnixFileMode]::OtherRead
            [IO.File]::SetUnixFileMode($path, $lockMode)
            if ([IO.File]::GetUnixFileMode($path) -ne $lockMode) { throw "release lock did not receive the fixed 0644 mode" }
        }
        $stream.SetLength(0); $bytes = [Text.Encoding]::ASCII.GetBytes("ku-release-lock-v1 pid=$PID`n"); $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true)
        return $stream
    }
    catch { $stream.Dispose(); throw }
}

function Write-PublishJournal([string]$Path, [string]$TransactionRoot, [string]$Target, [string]$Owner, [string]$StageLeaf, [string]$BackupLeaf, [bool]$HadCurrent, [string]$State) {
    if ($Owner -cnotmatch '^[0-9a-f]{32}$' -or $StageLeaf -cnotmatch '^\.ku-release-stage-[0-9]+-[0-9a-f]{32}$' -or ($BackupLeaf -cne '-' -and $BackupLeaf -cnotmatch '^\.ku-release-backup-[0-9]+-[0-9a-f]{32}$') -or $State -notin @('prepared', 'old_moved', 'new_moved')) { throw "refusing to write an invalid release transaction journal" }
    $had = if ($HadCurrent) { '1' } else { '0' }
    $text = @("format=ku-release-transaction-v1", "target=$Target", "owner=$Owner", "stage=$StageLeaf", "backup=$BackupLeaf", "had_current=$had", "state=$State") -join "`n"; $text += "`n"
    $temporary = Join-Path $TransactionRoot (".ku-release-journal-$([Guid]::NewGuid().ToString('N')).tmp")
    try {
        $stream = [IO.FileStream]::new($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $bytes = [Text.Encoding]::ASCII.GetBytes($text); $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
        if (-not $IsWindows) {
            $journalMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite
            [IO.File]::SetUnixFileMode($temporary, $journalMode)
            if ([IO.File]::GetUnixFileMode($temporary) -ne $journalMode) { throw "release transaction journal did not receive the fixed 0600 mode" }
        }
        if ((Test-Path -LiteralPath $Path) -and -not (Test-PlainFile $Path)) { throw "release transaction journal is not a plain file" }
        [IO.File]::Move($temporary, $Path, $true)
    }
    catch { if (Test-PlainFile $temporary) { [IO.File]::Delete($temporary) }; throw }
}

function Read-PublishJournal([string]$Path, [string]$Target) {
    if (-not (Test-PlainFile $Path)) { throw "release transaction journal is missing or not plain" }
    $info = Get-Item -LiteralPath $Path
    if ($info.Length -eq 0 -or $info.Length -gt 4096) { throw "release transaction journal has an invalid size" }
    $bytes = [IO.File]::ReadAllBytes($Path)
    if (@($bytes | Where-Object { $_ -gt 127 -or $_ -eq 0 -or $_ -eq 13 }).Count -ne 0) { throw "release transaction journal is not canonical ASCII" }
    $text = [Text.Encoding]::ASCII.GetString($bytes)
    if (-not $text.EndsWith("`n") -or $text.EndsWith("`n`n")) { throw "release transaction journal is not canonically terminated" }
    $names = @('format', 'target', 'owner', 'stage', 'backup', 'had_current', 'state'); $lines = $text.TrimEnd("`n").Split("`n")
    if ($lines.Count -ne $names.Count) { throw "release transaction journal has the wrong field count" }
    $fields = @{}
    for ($index = 0; $index -lt $names.Count; $index++) { $parts = $lines[$index].Split('=', 2); if ($parts.Count -ne 2 -or $parts[0] -cne $names[$index] -or $fields.ContainsKey($parts[0])) { throw "release transaction journal fields are invalid" }; $fields[$parts[0]] = $parts[1] }
    if ($fields.format -cne 'ku-release-transaction-v1' -or $fields.target -cne $Target -or $fields.owner -cnotmatch '^[0-9a-f]{32}$' -or $fields.stage -cnotmatch '^\.ku-release-stage-[0-9]+-[0-9a-f]{32}$' -or ($fields.backup -cne '-' -and $fields.backup -cnotmatch '^\.ku-release-backup-[0-9]+-[0-9a-f]{32}$') -or $fields.had_current -notin @('0', '1') -or $fields.state -notin @('prepared', 'old_moved', 'new_moved')) { throw "release transaction journal violates its contract" }
    return $fields
}

function Get-OwnedMarkerToken([string]$Path) {
    return Read-OwnedMarkerToken (Join-Path $Path ".ku-release-owner")
}

function Clear-OrphanedTransactionArtifacts([string]$TransactionRoot) {
    if (-not (Test-PlainDirectory $TransactionRoot)) { throw "release transaction root is not a plain directory" }
    $entries = 0
    foreach ($entry in [IO.Directory]::EnumerateFileSystemEntries($TransactionRoot)) {
        $entries++; if ($entries -gt 128) { throw "release transaction root has too many entries" }
        $leaf = [IO.Path]::GetFileName($entry)
        if ($leaf -cmatch '^\.ku-release-journal-[0-9a-f]{32}\.tmp$') { if (Test-PlainFile $entry) { [IO.File]::Delete($entry) }; continue }
        $prefix = if ($leaf -cmatch '^\.ku-release-stage-[0-9]+-[0-9a-f]{32}$') { '.ku-release-stage-' } elseif ($leaf -cmatch '^\.ku-release-backup-[0-9]+-[0-9a-f]{32}$') { '.ku-release-backup-' } elseif ($leaf -cmatch '^\.ku-history-stage-[0-9]+-[0-9a-f]{32}$') { '.ku-history-stage-' } else { $null }
        if ($null -ne $prefix -and (Test-PlainDirectory $entry)) { $owner = Get-OwnedMarkerToken $entry; if ($null -ne $owner) { Remove-OwnedDirectory $entry $TransactionRoot $prefix $owner } }
    }
}

function Recover-PublishTransaction([string]$JournalPath, [string]$TransactionRoot, [string]$Destination, [string]$Target) {
    if (-not (Test-Path -LiteralPath $JournalPath)) { Clear-OrphanedTransactionArtifacts $TransactionRoot; return }
    $journal = Read-PublishJournal $JournalPath $Target; $owner = [string]$journal.owner
    $stage = Join-Path $TransactionRoot ([string]$journal.stage); $backup = if ($journal.backup -eq '-') { $null } else { Join-Path $TransactionRoot ([string]$journal.backup) }
    if (Test-Path -LiteralPath $stage) { Assert-OwnedDirectory $stage $TransactionRoot '.ku-release-stage-' $owner }
    if ($null -ne $backup -and (Test-Path -LiteralPath $backup)) { Assert-OwnedDirectory $backup $TransactionRoot '.ku-release-backup-' $owner }
    if ((Test-Path -LiteralPath $Destination) -and -not (Test-PlainDirectory $Destination)) { throw "release destination is not a plain directory during recovery" }
    $backupPayload = if ($null -eq $backup) { $null } else { Join-Path $backup 'payload' }
    $hasBackup = $null -ne $backupPayload -and (Test-PlainDirectory $backupPayload); $hasDestination = Test-PlainDirectory $Destination
    if ($journal.state -eq 'new_moved') {
        if (-not $hasDestination) {
            if (-not $hasBackup) { throw "release recovery found neither the committed bundle nor its backup" }
            [IO.Directory]::Move($backupPayload, $Destination); $hasDestination = $true; $hasBackup = $false
        }
    }
    elseif ($journal.had_current -eq '1') {
        if ($hasBackup) {
            if ($hasDestination) {
                if (-not (Test-PlainDirectory $stage)) { throw "release recovery cannot preserve the interrupted candidate" }
                $rejected = Join-Path $stage 'rejected'; if (Test-Path -LiteralPath $rejected) { throw "release recovery staging path is already occupied" }
                Assert-PlainBoundedTree $Destination; [IO.Directory]::Move($Destination, $rejected); $hasDestination = $false
            }
            [IO.Directory]::Move($backupPayload, $Destination); $hasDestination = $true; $hasBackup = $false
        }
        elseif (-not $hasDestination) { throw "release recovery lost both the prior and candidate bundles" }
    }
    # With no prior bundle, a present destination means the candidate directory
    # move completed before its journal update. Keeping it cannot lose old state.
    if ($null -ne $backup -and (Test-Path -LiteralPath $backup)) { Remove-OwnedDirectory $backup $TransactionRoot '.ku-release-backup-' $owner }
    if (Test-Path -LiteralPath $stage) { Remove-OwnedDirectory $stage $TransactionRoot '.ku-release-stage-' $owner }
    if (-not (Test-PlainFile $JournalPath)) { throw "release transaction journal changed during recovery" }
    [IO.File]::Delete($JournalPath)
    Clear-OrphanedTransactionArtifacts $TransactionRoot
}

function Publish-Bundle([string]$Source, [string]$ReleaseRoot, [hashtable]$Contract, [string]$Version, [string]$BuildId) {
    Assert-Bundle $Source $Contract $Version $BuildId; Ensure-PlainDirectory $ReleaseRoot
    $lockRoot = Join-Path $ReleaseRoot '.locks'; $transactionBase = Join-Path $ReleaseRoot '.transactions'; Ensure-PlainDirectory $lockRoot; Ensure-PlainDirectory $transactionBase
    $transactionRoot = Join-Path $transactionBase $Contract.Target; Ensure-PlainDirectory $transactionRoot
    $lock = Enter-ExclusivePublishLock $lockRoot ("$($Contract.Target).lock"); $destination = Join-Path $ReleaseRoot $Contract.Target; $journalPath = Join-Path $transactionRoot 'journal'
    try {
        Recover-PublishTransaction $journalPath $transactionRoot $destination $Contract.Target
        $owner = [Guid]::NewGuid().ToString('N'); $stage = New-OwnedDirectory $transactionRoot '.ku-release-stage-' $owner; $backup = $null
        try {
            $payload = Join-Path $stage 'payload'; Copy-BundleTree $Source $payload $Contract; Assert-Bundle $payload $Contract $Version $BuildId
            $hadCurrent = Test-Path -LiteralPath $destination
            if ($hadCurrent) { Assert-PlainBoundedTree $destination; $backup = New-OwnedDirectory $transactionRoot '.ku-release-backup-' $owner }
            $backupLeaf = if ($null -eq $backup) { '-' } else { [IO.Path]::GetFileName($backup) }
            Write-PublishJournal $journalPath $transactionRoot $Contract.Target $owner ([IO.Path]::GetFileName($stage)) $backupLeaf $hadCurrent 'prepared'
            if ($hadCurrent) { [IO.Directory]::Move($destination, (Join-Path $backup 'payload')); Write-PublishJournal $journalPath $transactionRoot $Contract.Target $owner ([IO.Path]::GetFileName($stage)) $backupLeaf $true 'old_moved' }
            [IO.Directory]::Move($payload, $destination)
            Write-PublishJournal $journalPath $transactionRoot $Contract.Target $owner ([IO.Path]::GetFileName($stage)) $backupLeaf $hadCurrent 'new_moved'
            if ($null -ne $backup) { Remove-OwnedDirectory $backup $transactionRoot '.ku-release-backup-' $owner }
            Remove-OwnedDirectory $stage $transactionRoot '.ku-release-stage-' $owner
            [IO.File]::Delete($journalPath)
        }
        catch {
            $failure = $_
            try { Recover-PublishTransaction $journalPath $transactionRoot $destination $Contract.Target }
            catch { throw "release publish failed ('$($failure.Exception.Message)') and crash recovery also failed ('$($_.Exception.Message)'); a complete bundle remains at the destination or transaction backup" }
            throw $failure
        }
        return $destination
    }
    finally { $lock.Dispose() }
}

function Publish-ReleaseAndHistory([string]$Source, [string]$Repo, [hashtable]$Contract, [string]$Version, [string]$BuildId) {
    Assert-Bundle $Source $Contract $Version $BuildId
    $historyRoot = Join-Path $Repo 'history'; Ensure-PlainDirectory $historyRoot
    $locks = Join-Path $historyRoot '.locks'; $transactions = Join-Path $historyRoot '.transactions'; Ensure-PlainDirectory $locks; Ensure-PlainDirectory $transactions
    $versionLeaf = "v$Version"; $versionRoot = Join-Path $historyRoot $versionLeaf; Ensure-PlainDirectory $versionRoot
    $transactionVersion = Join-Path $transactions $versionLeaf; Ensure-PlainDirectory $transactionVersion
    $transactionRoot = Join-Path $transactionVersion $Contract.Target; Ensure-PlainDirectory $transactionRoot
    $lock = Enter-ExclusivePublishLock $locks ("$versionLeaf--$($Contract.Target).lock"); $historyDestination = Join-Path $versionRoot $Contract.Target; $stage = $null; $historyPublished = $false
    try {
        Clear-OrphanedTransactionArtifacts $transactionRoot
        if (Test-Path -LiteralPath $historyDestination) { throw "immutable history bundle already exists: '$historyDestination'" }
        $owner = [Guid]::NewGuid().ToString('N'); $stage = New-OwnedDirectory $transactionRoot '.ku-history-stage-' $owner; $payload = Join-Path $stage 'payload'
        Copy-BundleTree $Source $payload $Contract; Assert-Bundle $payload $Contract $Version $BuildId
        $releaseDestination = Publish-Bundle $Source (Join-Path $Repo 'release') $Contract $Version $BuildId
        [IO.Directory]::Move($payload, $historyDestination); $historyPublished = $true
        try { Remove-OwnedDirectory $stage $transactionRoot '.ku-history-stage-' $owner; $stage = $null } catch { Write-Warning "history bundle is complete, but its private staging directory could not be cleaned: $($_.Exception.Message)" }
        return [pscustomobject]@{ Release = $releaseDestination; History = $historyDestination }
    }
    catch {
        if (-not $historyPublished -and $null -ne $stage -and (Test-Path -LiteralPath $stage)) { Remove-OwnedDirectory $stage $transactionRoot '.ku-history-stage-' $owner }
        throw
    }
    finally { $lock.Dispose() }
}

function Invoke-ReleaseSelfTest([string]$Repo, [string]$BuilderPath, [string]$HeaderSource) {
    $contract = Get-HostContract; $buildId = Get-NativeTlsBuildId $BuilderPath; $temporaryRoot = [IO.Path]::GetTempPath()
    if (-not (Test-PlainDirectory $temporaryRoot)) { throw "release self-test requires a plain system temporary directory" }
    foreach ($validVersion in @('0.0.16', '1.0.0-0', '1.0.0-alpha.1', '1.0.0-alpha-01+build.01')) { if (-not (Test-PathSafeSemVer $validVersion)) { throw "SemVer self-test rejected '$validVersion'" } }
    foreach ($invalidVersion in @('1.0.0-01', '1.0.0-alpha.01', '01.0.0', '1.0.0-', '1.0.0+bad/path')) { if (Test-PathSafeSemVer $invalidVersion) { throw "SemVer self-test accepted '$invalidVersion'" } }

    $python = Get-Command 'python3' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $python) { $python = Get-Command 'python' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1 }
    if ($null -eq $python) { throw "release self-test requires Python to exercise orphan process-tree cleanup" }

    $processOwner = [Guid]::NewGuid().ToString('N'); $processRoot = New-OwnedDirectory $temporaryRoot '.ku-release-process-selftest-' $processOwner
    try {
        function Assert-SelfTestChildGone([string]$PidPath, [string]$Case) {
            if (-not (Test-PlainFile $PidPath)) { throw "$Case self-test did not record its child" }
            $childPid = [int]([IO.File]::ReadAllText($PidPath)); $deadline = [DateTime]::UtcNow.AddSeconds(2); $alive = $true
            do {
                try { $child = [Diagnostics.Process]::GetProcessById($childPid); $alive = -not $child.HasExited; $child.Dispose() } catch [ArgumentException] { $alive = $false }
                if ($alive) { Start-Sleep -Milliseconds 20 }
            } while ($alive -and [DateTime]::UtcNow -lt $deadline)
            if ($alive) { throw "$Case self-test left child PID $childPid alive" }
        }

        $wrapper = New-ToolWrapper $processRoot; $pidFile = Join-Path $processRoot 'child.pid'
        $program = 'import subprocess,sys; p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]); open(sys.argv[1],"w").write(str(p.pid))'
        $clock = [Diagnostics.Stopwatch]::StartNew(); [void](Invoke-CheckedTool ([string]$python.Source) @('-c', $program, $pidFile) $processRoot $processRoot 'orphan-tree' 10000 $wrapper); $clock.Stop()
        Assert-SelfTestChildGone $pidFile 'orphan process'
        if ($clock.ElapsedMilliseconds -gt 15000) { throw "orphan process self-test exceeded its fixed cleanup bound" }

        $outputPidFile = Join-Path $processRoot 'output-child.pid'
        $outputProgram = 'import subprocess,sys; p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]); open(sys.argv[1],"w").write(str(p.pid)); sys.stdout.buffer.write((b"x"*8191+b"\n")*640); sys.stdout.flush()'
        $outputRejected = $false
        $outputFailure = $null
        try { [void](Invoke-BoundedTool ([string]$python.Source) @('-c', $outputProgram, $outputPidFile) $processRoot $processRoot 'output-budget' 10000 $wrapper) } catch { $outputFailure = $_.Exception.Message; $outputRejected = $outputFailure -like '*output exceeded*' }
        if (-not $outputRejected) {
            $observed = if ($null -eq $outputFailure) { 'no rejection' } else { "unexpected rejection: $outputFailure" }
            throw "release output-budget self-test did not report the bounded-output contract ($observed)"
        }
        Assert-SelfTestChildGone $outputPidFile 'output-budget process'
        $captured = 0L; foreach ($log in Get-ChildItem -LiteralPath $processRoot -File | Where-Object Name -Like 'output-budget-*.std*') { $captured += $log.Length }
        if ($captured -gt $processOutputLimit) { throw "release output-budget self-test captured more than its configured limit" }

        $timeoutPidFile = Join-Path $processRoot 'timeout-child.pid'
        $timeoutProgram = 'import subprocess,sys,time; p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]); open(sys.argv[1],"w").write(str(p.pid)); time.sleep(60)'
        $timeoutRejected = $false
        try { [void](Invoke-BoundedTool ([string]$python.Source) @('-c', $timeoutProgram, $timeoutPidFile) $processRoot $processRoot 'direct-timeout' 2000 $wrapper) } catch { $timeoutRejected = $_.Exception.Message -like '*exceeded its 2000-ms deadline*' }
        if (-not $timeoutRejected) { throw "release direct-timeout self-test was not rejected" }
        Assert-SelfTestChildGone $timeoutPidFile 'timeout process'
    }
    finally { Remove-OwnedDirectory $processRoot $temporaryRoot '.ku-release-process-selftest-' $processOwner }
    if (Test-Path -LiteralPath $processRoot) { throw "orphan process self-test work root could not be removed immediately" }

    if ($IsWindows) {
        $lockedOwner = [Guid]::NewGuid().ToString('N'); $lockedRoot = New-OwnedDirectory $temporaryRoot '.ku-release-cleanup-selftest-' $lockedOwner; $lockedStream = $null
        try {
            $lockedStream = [IO.FileStream]::new((Join-Path $lockedRoot 'locked'), [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::Read)
            $cleanupFailed = $false; try { Remove-OwnedDirectory $lockedRoot $temporaryRoot '.ku-release-cleanup-selftest-' $lockedOwner } catch { $cleanupFailed = $true }
            if (-not $cleanupFailed) { throw "release cleanup self-test unexpectedly deleted a locked file" }
            Assert-OwnedDirectory $lockedRoot $temporaryRoot '.ku-release-cleanup-selftest-' $lockedOwner
        }
        finally { if ($null -ne $lockedStream) { $lockedStream.Dispose() } }
        Remove-OwnedDirectory $lockedRoot $temporaryRoot '.ku-release-cleanup-selftest-' $lockedOwner
        if (Test-Path -LiteralPath $lockedRoot) { throw "release cleanup self-test could not retry an authenticated tree" }
    }

    $limitOwner = [Guid]::NewGuid().ToString('N'); $limitRoot = New-OwnedDirectory $temporaryRoot '.ku-release-limit-selftest-' $limitOwner
    [IO.File]::WriteAllText((Join-Path $limitRoot 'first'), 'first'); [IO.File]::WriteAllText((Join-Path $limitRoot 'second'), 'second')
    $savedCleanupLimit = $script:cleanupEntryLimit; $limitFailed = $false
    try { $script:cleanupEntryLimit = 1; try { Remove-OwnedDirectory $limitRoot $temporaryRoot '.ku-release-limit-selftest-' $limitOwner } catch { $limitFailed = $true } }
    finally { $script:cleanupEntryLimit = $savedCleanupLimit }
    if (-not $limitFailed) { throw "release cleanup entry-limit self-test was not rejected" }
    Assert-OwnedDirectory $limitRoot $temporaryRoot '.ku-release-limit-selftest-' $limitOwner
    Remove-OwnedDirectory $limitRoot $temporaryRoot '.ku-release-limit-selftest-' $limitOwner
    if (Test-Path -LiteralPath $limitRoot) { throw "release entry-limit cleanup self-test could not retry an authenticated tree" }

    $owner = [Guid]::NewGuid().ToString('N'); $workRoot = New-OwnedDirectory $temporaryRoot '.ku-release-selftest-' $owner
    try {
        $wrapper = New-ToolWrapper $workRoot; $script:releaseWorkRoot = $workRoot; $script:toolWrapper = $wrapper
        [void](Invoke-CheckedTool 'pwsh' @('-NoLogo', '-NoProfile', '-NonInteractive', '-File', $BuilderPath, '-Target', $contract.Target, '-OutputRoot', $workRoot, '-SelfTest') $Repo $workRoot 'builder-selftest' 60000 $wrapper)
        if (-not $IsWindows) {
            $modeDirectory = Join-Path $workRoot 'mode-directory'; Ensure-PlainDirectory $modeDirectory
            $expectedDirectoryMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute -bor [IO.UnixFileMode]::GroupRead -bor [IO.UnixFileMode]::GroupExecute -bor [IO.UnixFileMode]::OtherRead -bor [IO.UnixFileMode]::OtherExecute
            if ([IO.File]::GetUnixFileMode($modeDirectory) -ne $expectedDirectoryMode) { throw "release directory mode self-test did not preserve the fixed 0755 contract" }
            $plainSource = Join-Path $workRoot 'plain-source'; $plainCopy = Join-Path $workRoot 'plain-copy'; [IO.File]::WriteAllText($plainSource, 'plain mode probe', [Text.Encoding]::ASCII); Copy-PlainFile $plainSource $plainCopy 1024
            $expectedPlainMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::GroupRead -bor [IO.UnixFileMode]::OtherRead
            if ([IO.File]::GetUnixFileMode($plainCopy) -ne $expectedPlainMode) { throw "release data mode self-test did not preserve the fixed 0644 contract" }
            $modeSource = Join-Path $workRoot 'mode-source'; $modeCopy = Join-Path $workRoot 'mode-copy'; [IO.File]::WriteAllText($modeSource, 'mode probe', [Text.Encoding]::ASCII)
            [IO.File]::SetUnixFileMode($modeSource, [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute)
            Copy-ExecutableFile $modeSource $modeCopy 1024
            $expectedMode = [IO.UnixFileMode]::UserRead -bor [IO.UnixFileMode]::UserWrite -bor [IO.UnixFileMode]::UserExecute -bor [IO.UnixFileMode]::GroupRead -bor [IO.UnixFileMode]::GroupExecute -bor [IO.UnixFileMode]::OtherRead -bor [IO.UnixFileMode]::OtherExecute
            if ([IO.File]::GetUnixFileMode($modeCopy) -ne $expectedMode) { throw "release executable mode self-test did not preserve the fixed 0755 contract" }
        }
        $script:expectedTlsHeaderHash = (Get-FileHash -LiteralPath $HeaderSource -Algorithm SHA256).Hash.ToLowerInvariant()
        $script:archiveVerifier = New-NativeTlsArchiveVerifier $Repo $workRoot $wrapper

        $module = Join-Path $workRoot 'native_tls_archive.rs'; $moduleTests = Join-Path $workRoot $(if ($IsWindows) { 'native_tls_archive_tests.exe' } else { 'native_tls_archive_tests' })
        [void](Invoke-CheckedTool 'rustc' @('+1.89.0', '--edition=2021', '--test', '-o', $moduleTests, $module) $workRoot $workRoot 'archive-validator-tests-build' 30000 $wrapper)
        [void](Invoke-CheckedTool $moduleTests @('--nocapture') $workRoot $workRoot 'archive-validator-tests-run' 30000 $wrapper)

        function Add-SelfTestVsixEntry([IO.Compression.ZipArchive]$Archive, [string]$Name, [string]$Content) {
            $entry = $Archive.CreateEntry($Name, [IO.Compression.CompressionLevel]::Fastest); $entryStream = $entry.Open()
            try { $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Content); $entryStream.Write($bytes, 0, $bytes.Length) } finally { $entryStream.Dispose() }
        }
        function New-SelfTestVsix([string]$Path, [bool]$IncludeDependency, [bool]$IncludeTraversal, [bool]$IncludeDuplicate) {
            $file = [IO.FileStream]::new($Path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
            try {
                $zip = [IO.Compression.ZipArchive]::new($file, [IO.Compression.ZipArchiveMode]::Create, $true)
                try {
                    Add-SelfTestVsixEntry $zip 'extension/package.json' '{"version":"0.0.16","main":"./out/language.js"}'
                    Add-SelfTestVsixEntry $zip 'extension/out/language.js' 'require("./executableModel");'
                    if ($IncludeDependency) { Add-SelfTestVsixEntry $zip 'extension/out/executableModel.js' 'exports.ok = true;' }
                    if ($IncludeTraversal) { Add-SelfTestVsixEntry $zip 'extension/../escape.js' 'escape' }
                    if ($IncludeDuplicate) { Add-SelfTestVsixEntry $zip 'extension/out/language.js' 'duplicate' }
                }
                finally { $zip.Dispose() }
            }
            finally { $file.Dispose() }
        }
        $positiveVsix = Join-Path $workRoot 'positive.vsix'; New-SelfTestVsix $positiveVsix $true $false $false; Assert-Vsix $positiveVsix '0.0.16'
        foreach ($negative in @(
            @{ Name = 'missing-module'; Dependency = $false; Traversal = $false; Duplicate = $false; Message = '*no packaged module*' },
            @{ Name = 'traversal'; Dependency = $true; Traversal = $true; Duplicate = $false; Message = '*path traversal*' },
            @{ Name = 'duplicate'; Dependency = $true; Traversal = $false; Duplicate = $true; Message = '*duplicate or case-colliding*' }
        )) {
            $negativePath = Join-Path $workRoot ("$($negative.Name).vsix"); New-SelfTestVsix $negativePath ([bool]$negative.Dependency) ([bool]$negative.Traversal) ([bool]$negative.Duplicate); $rejected = $false
            try { Assert-Vsix $negativePath '0.0.16' } catch { $rejected = $_.Exception.Message -like [string]$negative.Message }
            if (-not $rejected) { throw "VSIX self-test did not reject '$($negative.Name)' with the expected contract error" }
        }

        function New-SelfTestTlsPack([string]$Leaf, [bool]$UseRepositoryHeader) {
            Ensure-PlainDirectory $Leaf; Ensure-PlainDirectory (Join-Path $Leaf 'include'); Ensure-PlainDirectory (Join-Path $Leaf 'lib')
            $header = Join-Path $Leaf 'include/ku_native_tls.h'
            if ($UseRepositoryHeader) { Copy-PlainFile $HeaderSource $header 64KB } else { [IO.File]::WriteAllText($header, 'not a Ku TLS ABI header', [Text.UTF8Encoding]::new($false)) }
            $archive = Join-Path $Leaf ("lib/$($contract.Archive)"); [IO.File]::WriteAllBytes($archive, [Text.Encoding]::ASCII.GetBytes("!<arch>`n"))
            $archiveInfo = Get-Item -LiteralPath $archive; $headerInfo = Get-Item -LiteralPath $header
            $manifest = @(
                'format=ku-native-tls-pack-v1', "target=$($contract.Target)", "flavor=$($contract.Flavor)", "object_format=$($contract.ObjectFormat)", 'abi_version=1', 'panic=unwind', "build_id=$buildId",
                "archive_size=$($archiveInfo.Length)", "archive_sha256=$((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant())",
                "header_size=$($headerInfo.Length)", "header_sha256=$((Get-FileHash -LiteralPath $header -Algorithm SHA256).Hash.ToLowerInvariant())",
                "link_contract=$($contract.LinkContract)", "crt=$($contract.Crt)", 'runtime_dependency=none'
            ) -join "`n"
            [IO.File]::WriteAllText((Join-Path $Leaf 'manifest.kutls'), $manifest + "`n", [Text.Encoding]::ASCII)
        }

        $fakeArchive = Join-Path $workRoot 'fake-archive'; New-SelfTestTlsPack $fakeArchive $true; $rejected = $false
        try { Assert-TlsPack $fakeArchive $contract $buildId } catch { $rejected = $true }
        if (-not $rejected) { throw "release validator accepted an empty self-describing ar archive" }
        $fakeHeader = Join-Path $workRoot 'fake-header'; New-SelfTestTlsPack $fakeHeader $false; $rejected = $false
        try { Assert-TlsPack $fakeHeader $contract $buildId } catch { $rejected = $true }
        if (-not $rejected) { throw "release validator accepted a fake ABI header" }

        $lockRoot = Join-Path $workRoot 'lock-probe'; Ensure-PlainDirectory $lockRoot; $firstLock = Enter-ExclusivePublishLock $lockRoot 'target.lock'
        try {
            $secondWasRejected = $false
            try { $secondLock = Enter-ExclusivePublishLock $lockRoot 'target.lock'; $secondLock.Dispose() } catch { $secondWasRejected = $_.Exception.Message -like '*already holds*' }
            if (-not $secondWasRejected) { throw "release lock admitted two writers for one target" }
        }
        finally { $firstLock.Dispose() }
        $afterRelease = Enter-ExclusivePublishLock $lockRoot 'target.lock'; $afterRelease.Dispose()

        foreach ($case in @(
            @{ Name = 'prepared_old_intact'; State = 'prepared'; Had = $true; Destination = $true; BackupPayload = $false; Expected = 'old' },
            @{ Name = 'old_moved'; State = 'old_moved'; Had = $true; Destination = $false; Expected = 'old' },
            @{ Name = 'move_before_state'; State = 'old_moved'; Had = $true; Destination = $true; Expected = 'old' },
            @{ Name = 'new_moved'; State = 'new_moved'; Had = $true; Destination = $true; Expected = 'new' },
            @{ Name = 'first_publish'; State = 'prepared'; Had = $false; Destination = $true; Expected = 'new' },
            @{ Name = 'first_before_move'; State = 'prepared'; Had = $false; Destination = $false; Expected = $null }
        )) {
            $caseRoot = Join-Path $workRoot ("recovery-$($case.Name)"); $transactionRoot = Join-Path $caseRoot 'transactions'; Ensure-PlainDirectory $caseRoot; Ensure-PlainDirectory $transactionRoot
            $destination = Join-Path $caseRoot $contract.Target; $journal = Join-Path $transactionRoot 'journal'; $caseOwner = [Guid]::NewGuid().ToString('N')
            $stage = New-OwnedDirectory $transactionRoot '.ku-release-stage-' $caseOwner; $backup = $null; $backupLeaf = '-'
            if ($case.Had) {
                $backup = New-OwnedDirectory $transactionRoot '.ku-release-backup-' $caseOwner; $backupLeaf = [IO.Path]::GetFileName($backup)
                if ($case.Name -ne 'prepared_old_intact') { Ensure-PlainDirectory (Join-Path $backup 'payload'); [IO.File]::WriteAllText((Join-Path $backup 'payload/old'), 'old') }
            }
            if ($case.Name -eq 'prepared_old_intact') { Ensure-PlainDirectory $destination; [IO.File]::WriteAllText((Join-Path $destination 'old'), 'old'); Ensure-PlainDirectory (Join-Path $stage 'payload'); [IO.File]::WriteAllText((Join-Path $stage 'payload/new'), 'new') }
            elseif (-not $case.Destination) { Ensure-PlainDirectory (Join-Path $stage 'payload'); [IO.File]::WriteAllText((Join-Path $stage 'payload/new'), 'new') }
            else { Ensure-PlainDirectory $destination; [IO.File]::WriteAllText((Join-Path $destination 'new'), 'new') }
            Write-PublishJournal $journal $transactionRoot $contract.Target $caseOwner ([IO.Path]::GetFileName($stage)) $backupLeaf ([bool]$case.Had) ([string]$case.State)
            Recover-PublishTransaction $journal $transactionRoot $destination $contract.Target
            $expectedOk = if ($null -eq $case.Expected) { -not (Test-Path -LiteralPath $destination) } else { Test-PlainFile (Join-Path $destination ([string]$case.Expected)) }
            if (-not $expectedOk -or (Test-Path -LiteralPath $journal) -or (Test-Path -LiteralPath $stage) -or ($null -ne $backup -and (Test-Path -LiteralPath $backup))) { throw "release transaction recovery failed case '$($case.Name)'" }
        }
        Write-Output "release self-test ok: SemVer, orphan/timeout/output/retry cleanup, strict TLS ABI archive validation, single-writer locking, and crash recovery"
    }
    finally { Remove-OwnedDirectory $workRoot $temporaryRoot '.ku-release-selftest-' $owner }
}

$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..")); $cargoPath = Join-Path $repo "Cargo.toml"; $builderPath = Join-Path $PSScriptRoot "build-native-tls-pack.ps1"; $extensionSource = Join-Path $repo "editors/vscode-ku"; $tlsHeaderSource = Join-Path $repo "crates/ku-native-tls/include/ku_native_tls.h"
if (-not (Test-PlainDirectory $repo) -or -not (Test-PlainFile $cargoPath) -or -not (Test-PlainDirectory $extensionSource) -or -not (Test-PlainFile $tlsHeaderSource)) { throw "cannot resolve a plain Ku source tree" }
if ($SelfTest) { Invoke-ReleaseSelfTest $repo $builderPath $tlsHeaderSource; return }
$cargoText = [IO.File]::ReadAllText($cargoPath, $utf8); $versionMatches = [regex]::Matches($cargoText, '(?m)^version\s*=\s*"(?<value>[^"]+)"\s*$')
if ($versionMatches.Count -lt 1) { throw "cannot read Ku package version" }
$version = $versionMatches[0].Groups['value'].Value
if (-not (Test-PathSafeSemVer $version)) { throw "Ku version is not a path-safe SemVer value" }
$packageJson = Get-Content -LiteralPath (Join-Path $extensionSource "package.json") -Raw -Encoding UTF8 | ConvertFrom-Json -AsHashtable
$packageLock = Get-Content -LiteralPath (Join-Path $extensionSource "package-lock.json") -Raw -Encoding UTF8 | ConvertFrom-Json -AsHashtable
$lockRoot = $packageLock["packages"][""]
if ([string]$packageJson["version"] -cne $version -or [string]$packageLock["version"] -cne $version -or [string]$lockRoot["version"] -cne $version) { throw "Cargo, extension, and lockfile versions must match" }
$vsceVersion = [string]$packageJson["devDependencies"]["@vscode/vsce"]
if ($vsceVersion -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$' -or [string]$lockRoot["devDependencies"]["@vscode/vsce"] -cne $vsceVersion) { throw "@vscode/vsce must be an exact lockfile-backed devDependency" }
$lockedVsce = $packageLock["packages"]["node_modules/@vscode/vsce"]
if ($null -eq $lockedVsce -or [string]$lockedVsce["version"] -cne $vsceVersion -or [string]$lockedVsce["resolved"] -cnotmatch '^https://[^\s]+$' -or [string]$lockedVsce["integrity"] -cnotmatch '^sha512-[A-Za-z0-9+/]+={0,2}$') { throw "@vscode/vsce lock entry must pin the declared version to an HTTPS artifact with SHA-512 integrity" }

$contract = Get-HostContract; $buildId = Get-NativeTlsBuildId $builderPath; $workBase = [IO.Path]::GetTempPath()
if (-not (Test-PlainDirectory $workBase)) { throw "release packaging requires a plain system temporary directory" }
$workOwner = [Guid]::NewGuid().ToString("N"); $workRoot = New-OwnedDirectory $workBase ".kr-" $workOwner; $wrapper = New-ToolWrapper $workRoot
$script:releaseWorkRoot = $workRoot; $script:toolWrapper = $wrapper; $script:expectedTlsHeaderHash = (Get-FileHash -LiteralPath $tlsHeaderSource -Algorithm SHA256).Hash.ToLowerInvariant()
try {
    $rustcOutput = Invoke-CheckedTool "rustc" @("+1.89.0", "-vV") $repo $workRoot "rustc" 30000 $wrapper
    if ($rustcOutput -notmatch "(?m)^host: $([regex]::Escape($contract.Target))$") { throw "Rust 1.89.0 host does not match release target '$($contract.Target)'" }
    $script:archiveVerifier = New-NativeTlsArchiveVerifier $repo $workRoot $wrapper
    Write-Host "Building Ku $version for $($contract.Target)..."
    $cargoTarget = Join-Path $workRoot "cargo"
    [void](Invoke-CheckedTool "cargo" @("+1.89.0", "build", "--locked", "--release", "--target", $contract.Target, "--target-dir", $cargoTarget, "--color", "never") $repo $workRoot "cargo-build" 900000 $wrapper)
    $tlsOutputRoot = Join-Path $workRoot "tls-pack"
    [void](Invoke-CheckedTool "pwsh" @("-NoLogo", "-NoProfile", "-NonInteractive", "-File", $builderPath, "-Target", $contract.Target, "-OutputRoot", $tlsOutputRoot) $repo $workRoot "tls-pack" 600000 $wrapper)
    $tlsPack = Join-Path $tlsOutputRoot $contract.Target

    $extensionWork = Join-Path $workRoot "extension"; Copy-ExtensionSource $extensionSource $extensionWork
    $nodeVersion = Invoke-CheckedTool "node" @("--version") $extensionWork $workRoot "node-version" 30000 $wrapper
    if ($nodeVersion.Trim() -notmatch '^v(?<major>[0-9]+)\.[0-9]+\.[0-9]+$' -or [int]$Matches.major -lt 22) { throw "VS Code release packaging requires Node.js 22 or newer; found '$($nodeVersion.Trim())'" }
    [void](Invoke-CheckedTool "npm" @("ci", "--ignore-scripts", "--no-audit", "--no-fund") $extensionWork $workRoot "npm-ci" 300000 $wrapper)
    [void](Invoke-CheckedTool "npm" @("test") $extensionWork $workRoot "npm-test" 180000 $wrapper)
    $vsixName = "ku-language-$version.vsix"; $vsixPath = Join-Path $extensionWork $vsixName
    [void](Invoke-CheckedTool "npx" @("--no-install", "vsce", "package", "--out", $vsixPath) $extensionWork $workRoot "vsce-package" 300000 $wrapper)

    $targetRelease = Join-Path $cargoTarget "$($contract.Target)/release"; $candidate = Join-Path $workRoot "bundle"; Ensure-PlainDirectory $candidate
    Copy-ExecutableFile (Join-Path $targetRelease $contract.Executable) (Join-Path $candidate $contract.Executable) 512MB
    Copy-PlainFile (Join-Path $targetRelease "libku.rlib") (Join-Path $candidate "libku.rlib") 512MB
    $pdb = Join-Path $targetRelease "ku.pdb"; if ($IsWindows -and (Test-PlainFile $pdb)) { Copy-PlainFile $pdb (Join-Path $candidate "ku.pdb") 512MB }
    Copy-PlainFile $vsixPath (Join-Path $candidate $vsixName) 128MB
    Copy-TlsPack $tlsPack (Join-Path $candidate "native-tls/v1/$($contract.Target)") $contract $buildId
    Assert-Bundle $candidate $contract $version $buildId
    Test-PackagedTlsConsumer $candidate $contract $workRoot $wrapper
    Assert-Bundle $candidate $contract $version $buildId
    if ($CheckOnly) {
        $kind = if ($ArchiveInternal) { 'archive' } else { 'package' }
        Write-Output "$kind check ok: ku $version ($($contract.Target)); no release or history directory was created or published"
        return
    }
    if ($ArchiveInternal) {
        $published = Publish-ReleaseAndHistory $candidate $repo $contract $version $buildId
        Write-Output "release bundle switched with per-target locking and crash recovery: $($published.Release)"
        Write-Output "immutable history bundle published as one complete directory: $($published.History)"
    }
    else {
        $published = Publish-Bundle $candidate (Join-Path $repo "release") $contract $version $buildId
        if ($InstallExtension) { [void](Invoke-CheckedTool "code" @("--install-extension", (Join-Path $published $vsixName), "--force") $repo $workRoot "code-install" 180000 $wrapper) }
        Write-Output "release bundle switched with per-target locking and crash recovery: $published"
    }
}
finally { Remove-OwnedDirectory $workRoot $workBase ".kr-" $workOwner }
