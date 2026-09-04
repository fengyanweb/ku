[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet(
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "x86_64-pc-windows-gnu",
        "aarch64-apple-darwin"
    )]
    [string]$Target,

    [Parameter(Mandatory = $true)]
    [string]$OutputRoot,

    [Parameter(DontShow = $true)]
    [switch]$SelfTest
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$kuTlsBuildId = "ku-native-tls/0.1.0;abi=1;rustls=0.23.40;ring=0.17.14;webpki-roots=1.0.7;buffer=65536;handshake=1048576;record-staging=65540;resumption=disabled"
$kuTlsManifestLimit = 64KB
$kuTlsHeaderLimit = 64KB
$kuTlsArchiveLimit = 128MB
$kuTlsProcessOutputLimit = 1MB
$kuTlsToolProbeTimeoutMs = 30000
$kuTlsCargoTimeoutMs = 300000
$kuTlsCleanupEntryLimit = 250000

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "build-native-tls-pack.ps1 requires PowerShell 7 or newer for bounded process-tree cleanup"
}

$kuTlsPathComparison = if ($IsWindows) {
    [System.StringComparison]::OrdinalIgnoreCase
}
else {
    [System.StringComparison]::Ordinal
}

$kuTlsContracts = @{
    "x86_64-unknown-linux-gnu" = @{
        Flavor = "gnu"
        ObjectFormat = "elf-x86_64"
        Archive = "libku_native_tls.a"
        LinkContract = "rust-1.89.0-linux-gnu-v1"
        Crt = "system-dynamic"
        NativeStaticLibs = "-lc -lm -lrt -lpthread -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc"
    }
    "x86_64-pc-windows-msvc" = @{
        Flavor = "msvc"
        ObjectFormat = "coff-x86_64"
        Archive = "ku_native_tls.lib"
        LinkContract = "rust-1.89.0-windows-msvc-v1"
        Crt = "msvc-dynamic"
        NativeStaticLibs = "bcrypt.lib advapi32.lib kernel32.lib ntdll.lib userenv.lib ws2_32.lib dbghelp.lib /defaultlib:msvcrt"
    }
    "x86_64-pc-windows-gnu" = @{
        Flavor = "gnu"
        ObjectFormat = "coff-x86_64"
        Archive = "libku_native_tls.a"
        LinkContract = "rust-1.89.0-windows-gnu-v1"
        Crt = "mingw-dynamic"
        NativeStaticLibs = "-lbcrypt -ladvapi32 -lkernel32 -lntdll -luserenv -lws2_32 -ldbghelp"
    }
    "aarch64-apple-darwin" = @{
        Flavor = "apple"
        ObjectFormat = "macho-arm64"
        Archive = "libku_native_tls.a"
        LinkContract = "rust-1.89.0-darwin-v1"
        Crt = "system-dynamic"
        NativeStaticLibs = "-lc -lm -liconv -lSystem -lc -lm"
    }
}

function Get-KuTlsFullPath([string]$Path) {
    return [System.IO.Path]::GetFullPath($Path)
}

function Set-KuTlsPublicDirectoryMode([string]$Path) {
    if ($IsWindows) { return }
    $mode = [System.IO.UnixFileMode]::UserRead -bor [System.IO.UnixFileMode]::UserWrite -bor [System.IO.UnixFileMode]::UserExecute -bor [System.IO.UnixFileMode]::GroupRead -bor [System.IO.UnixFileMode]::GroupExecute -bor [System.IO.UnixFileMode]::OtherRead -bor [System.IO.UnixFileMode]::OtherExecute
    [System.IO.File]::SetUnixFileMode($Path, $mode)
    $attributes = [System.IO.File]::GetAttributes($Path)
    if (($attributes -band [System.IO.FileAttributes]::Directory) -eq 0 -or
        ($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        [System.IO.File]::GetUnixFileMode($Path) -ne $mode) {
        throw "native TLS public directory did not receive the fixed 0755 mode: '$Path'"
    }
}

function Set-KuTlsPublicFileMode([string]$Path) {
    if ($IsWindows) { return }
    $mode = [System.IO.UnixFileMode]::UserRead -bor [System.IO.UnixFileMode]::UserWrite -bor [System.IO.UnixFileMode]::GroupRead -bor [System.IO.UnixFileMode]::OtherRead
    [System.IO.File]::SetUnixFileMode($Path, $mode)
    $attributes = [System.IO.File]::GetAttributes($Path)
    if (($attributes -band [System.IO.FileAttributes]::Directory) -ne 0 -or
        ($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        [System.IO.File]::GetUnixFileMode($Path) -ne $mode) {
        throw "native TLS public file did not receive the fixed 0644 mode: '$Path'"
    }
}

function Read-KuTlsUtf8Source([string]$Path) {
    $info = Get-Item -LiteralPath $Path -Force
    if ($info.PSIsContainer -or
        ($info.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $info.Length -eq 0 -or
        $info.Length -gt 8MB) {
        throw "native TLS build-id source must be a bounded plain file: '$Path'"
    }
    return [System.IO.File]::ReadAllText(
        $Path,
        [System.Text.UTF8Encoding]::new($false, $true)
    )
}

function Assert-KuTlsBuildIdSources(
    [string]$CrateSource,
    [string]$CliSource,
    [string]$BackendSource,
    [string]$Expected
) {
    $crateText = Read-KuTlsUtf8Source $CrateSource
    $crateMatches = [regex]::Matches(
        $crateText,
        'const\s+BUILD_ID\s*:\s*&\[u8\]\s*=\s*b"(?<value>(?:[^"\\]|\\.)*)"\s*;',
        [Text.RegularExpressions.RegexOptions]::Singleline
    )
    if ($crateMatches.Count -ne 1) {
        throw "cannot read exactly one native TLS crate BUILD_ID"
    }
    $crateMatch = $crateMatches[0]
    $crateId = [regex]::Replace($crateMatch.Groups['value'].Value, '\\\r?\n\s*', '')
    if ($crateId.Contains('\')) {
        throw "native TLS crate BUILD_ID uses an unsupported escape"
    }

    $cliText = Read-KuTlsUtf8Source $CliSource
    $cliMatches = [regex]::Matches(
        $cliText,
        '(?m)^const NATIVE_TLS_BUILD_ID: &str = "(?<value>[^"\r\n\\]+)";\r?$'
    )
    if ($cliMatches.Count -ne 1) {
        throw "cannot read exactly one CLI NATIVE_TLS_BUILD_ID"
    }
    $cliId = $cliMatches[0].Groups['value'].Value

    $backendText = Read-KuTlsUtf8Source $BackendSource
    $backendMatches = [regex]::Matches(
        $backendText,
        'static const uint8_t ku_net_tls_expected_build_id\[\]\s*=\s*(?<segments>(?:"[^"\r\n]*"\s*)+);',
        [Text.RegularExpressions.RegexOptions]::Singleline
    )
    if ($backendMatches.Count -ne 1) {
        throw "cannot read exactly one C runtime native TLS expected build id"
    }
    $backendMatch = $backendMatches[0]
    $backendSegments = [regex]::Matches($backendMatch.Groups['segments'].Value, '"(?<value>[^"\r\n\\]*)"')
    if ($backendSegments.Count -eq 0) {
        throw "C runtime native TLS expected build id has no canonical segments"
    }
    $backendId = [string]::Concat(@($backendSegments | ForEach-Object { $_.Groups['value'].Value }))
    if (-not [string]::Equals($crateId, $Expected, [StringComparison]::Ordinal) -or
        -not [string]::Equals($cliId, $Expected, [StringComparison]::Ordinal) -or
        -not [string]::Equals($backendId, $Expected, [StringComparison]::Ordinal)) {
        throw "native TLS build-id drift: builder, TLS crate, CLI, and C runtime must declare the same identifier"
    }
}

if (-not ("Ku.NativeTlsPack.BoundedProcess" -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Threading;
using System.Threading.Tasks;

namespace Ku.NativeTlsPack {
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

        private sealed class OutputBudget {
            internal readonly Process Process;
            internal readonly long Maximum;
            internal readonly string GroupReadyPath;
            internal long Total;
            internal int Exceeded;
            internal int StopPumps;

            internal OutputBudget(Process process, long maximum, string groupReadyPath) {
                Process = process;
                Maximum = maximum;
                GroupReadyPath = groupReadyPath;
            }
        }

        private static void KillLeader(Process process) {
            try {
                if (!process.HasExited) {
                    process.Kill();
                }
            } catch (InvalidOperationException) {
            }
        }

        private static IntPtr AssignKillOnCloseJob(Process process) {
            IntPtr job = CreateJobObject(IntPtr.Zero, null);
            if (job == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error(), "CreateJobObject failed");
            var info = new JOBOBJECT_EXTENDED_LIMIT_INFORMATION();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if (!SetInformationJobObject(job, JobObjectExtendedLimitInformation, ref info, (uint)Marshal.SizeOf<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()) || !AssignProcessToJobObject(job, process.Handle)) {
                int error = Marshal.GetLastWin32Error(); CloseHandle(job); KillLeader(process);
                throw new Win32Exception(error, "failed to place native TLS subprocess in its kill-on-close Job Object");
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
                if (error != 3) throw new Win32Exception(error, "failed to terminate native TLS subprocess group");
            }
        }

        private static void StopForOutputLimit(OutputBudget budget) {
            try {
                if (!OperatingSystem.IsWindows() && File.Exists(budget.GroupReadyPath)) {
                    KillUnixProcessGroup(budget.Process.Id, budget.GroupReadyPath);
                } else {
                    KillLeader(budget.Process);
                }
            } catch {
                // The bounded wait below remains authoritative and retries cleanup.
                try { KillLeader(budget.Process); } catch { }
            }
        }

        private static async Task Pump(
            Stream input,
            string outputPath,
            OutputBudget budget
        ) {
            using (var output = new FileStream(
                outputPath,
                FileMode.CreateNew,
                FileAccess.Write,
                FileShare.Read,
                8192,
                true
            )) {
                var buffer = new byte[8192];
                try {
                    while (true) {
                        int read = await input.ReadAsync(buffer, 0, buffer.Length).ConfigureAwait(false);
                        if (read == 0) {
                            break;
                        }
                        long after = Interlocked.Add(ref budget.Total, read);
                        long before = after - read;
                        int permitted = before >= budget.Maximum
                            ? 0
                            : (int)Math.Min(read, budget.Maximum - before);
                        if (permitted != 0) {
                            await output.WriteAsync(buffer, 0, permitted).ConfigureAwait(false);
                        }
                        if (after > budget.Maximum &&
                            Interlocked.CompareExchange(ref budget.Exceeded, 1, 0) == 0) {
                            Interlocked.Exchange(ref budget.StopPumps, 1);
                            StopForOutputLimit(budget);
                        }
                    }
                } catch (ObjectDisposedException) when (
                    Volatile.Read(ref budget.StopPumps) != 0 ||
                    Volatile.Read(ref budget.Exceeded) != 0) {
                } catch (IOException) when (
                    Volatile.Read(ref budget.StopPumps) != 0 ||
                    Volatile.Read(ref budget.Exceeded) != 0) {
                }
                await output.FlushAsync().ConfigureAwait(false);
            }
        }

        public static BoundedProcessResult Run(
            string fileName,
            string[] arguments,
            string workingDirectory,
            string stdoutPath,
            string stderrPath,
            string startGatePath,
            string groupReadyPath,
            int timeoutMilliseconds,
            long maximumOutputBytes
        ) {
            if (timeoutMilliseconds <= 0 || maximumOutputBytes <= 0) {
                throw new ArgumentOutOfRangeException();
            }
            var start = new ProcessStartInfo {
                FileName = fileName,
                WorkingDirectory = workingDirectory,
                UseShellExecute = false,
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                CreateNoWindow = true,
                WindowStyle = ProcessWindowStyle.Hidden,
            };
            foreach (string argument in arguments) {
                start.ArgumentList.Add(argument);
            }
            using (var process = new Process { StartInfo = start }) {
                if (!process.Start()) {
                    throw new InvalidOperationException("failed to start build subprocess");
                }
                IntPtr job = IntPtr.Zero;
                try {
                    if (OperatingSystem.IsWindows()) { job = AssignKillOnCloseJob(process); SignalStart(startGatePath); }
                    var budget = new OutputBudget(process, maximumOutputBytes, groupReadyPath);
                    Task stdout = Pump(process.StandardOutput.BaseStream, stdoutPath, budget);
                    Task stderr = Pump(process.StandardError.BaseStream, stderrPath, budget);
                    bool timedOut = !process.WaitForExit(timeoutMilliseconds);
                    if (OperatingSystem.IsWindows()) { if (job != IntPtr.Zero) { CloseHandle(job); job = IntPtr.Zero; } }
                    else { KillUnixProcessGroup(process.Id, groupReadyPath); }
                    if (timedOut) KillLeader(process);
                    if (!process.WaitForExit(10000)) {
                        KillLeader(process);
                        throw new TimeoutException("build subprocess tree cleanup was not confirmed");
                    }
                    Task allOutput = Task.WhenAll(stdout, stderr);
                    bool descendantHeldOutputPipe = !allOutput.Wait(10000);
                    if (descendantHeldOutputPipe) {
                        Interlocked.Exchange(ref budget.StopPumps, 1);
                        KillLeader(process);
                        process.StandardOutput.Close();
                        process.StandardError.Close();
                        if (!allOutput.Wait(10000)) throw new TimeoutException("build subprocess output cleanup was not confirmed");
                    }
                    if (allOutput.IsFaulted) throw new IOException("build subprocess output capture failed", allOutput.Exception);
                    return new BoundedProcessResult {
                        ExitCode = process.ExitCode,
                        TimedOut = timedOut,
                        OutputLimitExceeded = Volatile.Read(ref budget.Exceeded) != 0,
                        DescendantHeldOutputPipe = descendantHeldOutputPipe,
                    };
                } finally {
                    if (job != IntPtr.Zero) CloseHandle(job);
                }
            }
        }
    }
}
'@
}

function Invoke-KuTlsBoundedProcess(
    [string]$FileName,
    [string[]]$ArgumentList,
    [string]$WorkingDirectory,
    [string]$LogRoot,
    [string]$LogName,
    [int]$TimeoutMilliseconds
) {
    $wrapperPath = Join-Path $LogRoot ".ku-invoke-tool.ps1"
    if (-not (Test-Path -LiteralPath $wrapperPath)) {
        $wrapperSource = @'
param([Parameter(Mandatory = $true)][string]$Spec)
$ErrorActionPreference = "Stop"
$item = Get-Content -LiteralPath $Spec -Raw -Encoding UTF8 | ConvertFrom-Json
try {
    if ($IsWindows) {
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        while (-not (Test-Path -LiteralPath ([string]$item.startGate) -PathType Leaf)) {
            if ([DateTime]::UtcNow -ge $deadline) { throw "native TLS Job Object start gate timed out" }
            Start-Sleep -Milliseconds 10
        }
    }
    else {
        Add-Type -TypeDefinition 'using System.Runtime.InteropServices; public static class KuTlsUnixGroup { [DllImport("libc", SetLastError=true)] public static extern int setpgid(int pid, int pgid); }'
        if ([KuTlsUnixGroup]::setpgid(0, 0) -ne 0) { throw "cannot create an isolated native TLS process group" }
        $ready = [IO.FileStream]::new([string]$item.groupReady, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
        try { $ready.WriteByte(1); $ready.Flush($true) } finally { $ready.Dispose() }
    }
    & ([string]$item.tool) @([string[]]$item.arguments)
    if ($LASTEXITCODE -is [int]) { exit $LASTEXITCODE }
    exit 0
}
catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
'@
        [System.IO.File]::WriteAllText(
            $wrapperPath,
            $wrapperSource,
            [System.Text.UTF8Encoding]::new($false)
        )
    }
    $command = Get-Command $FileName -ErrorAction Stop | Select-Object -First 1
    if ($command.CommandType -ne [Management.Automation.CommandTypes]::Application -and
        $command.CommandType -ne [Management.Automation.CommandTypes]::ExternalScript) {
        throw "native TLS tool '$FileName' must resolve to an application or external script"
    }
    $pwsh = Get-Command "pwsh" -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $runId = [Guid]::NewGuid().ToString("N")
    $specPath = Join-Path $LogRoot "$LogName-$runId.json"
    $startGatePath = Join-Path $LogRoot "$runId.start"
    $groupReadyPath = Join-Path $LogRoot "$runId.group"
    $spec = [ordered]@{
        tool = [string]$command.Source
        arguments = @($ArgumentList)
        startGate = $startGatePath
        groupReady = $groupReadyPath
    } | ConvertTo-Json -Compress
    [System.IO.File]::WriteAllText($specPath, $spec, [System.Text.UTF8Encoding]::new($false))
    $stdoutPath = Join-Path $LogRoot "$LogName-$runId.stdout"
    $stderrPath = Join-Path $LogRoot "$LogName-$runId.stderr"
    $result = [Ku.NativeTlsPack.BoundedProcess]::Run(
        [string]$pwsh.Source,
        @("-NoLogo", "-NoProfile", "-NonInteractive", "-File", $wrapperPath, "-Spec", $specPath),
        $WorkingDirectory,
        $stdoutPath,
        $stderrPath,
        $startGatePath,
        $groupReadyPath,
        $TimeoutMilliseconds,
        $kuTlsProcessOutputLimit
    )
    $stdout = [System.IO.File]::ReadAllText($stdoutPath)
    $stderr = [System.IO.File]::ReadAllText($stderrPath)
    $output = if ($stdout.Length -eq 0) {
        $stderr
    }
    elseif ($stderr.Length -eq 0) {
        $stdout
    }
    else {
        "$stdout`n$stderr"
    }
    if ($result.OutputLimitExceeded) {
        throw "$FileName output exceeded the $kuTlsProcessOutputLimit-byte limit; its process tree was terminated"
    }
    if ($result.TimedOut) {
        throw "$FileName exceeded its $TimeoutMilliseconds-ms deadline; its process tree was terminated"
    }
    if ($result.DescendantHeldOutputPipe) {
        throw "$FileName left an inherited output pipe open after exit; the bounded build was rejected"
    }
    return [pscustomobject]@{
        ExitCode = $result.ExitCode
        Output = $output
    }
}

function New-KuTlsPrivateRoot(
    [string]$Root,
    [string]$Prefix,
    [string]$OwnerToken
) {
    if ($OwnerToken -cnotmatch '^[0-9a-f]{32}$') {
        throw "private native TLS owner token is invalid"
    }
    $name = "$Prefix$PID-$([Guid]::NewGuid().ToString('N'))"
    $path = Join-Path $Root $name
    if (Test-Path -LiteralPath $path) {
        throw "private native TLS path unexpectedly already exists: '$path'"
    }
    $marker = Join-Path $path ".ku-private-owner"
    $markerBytes = [System.Text.Encoding]::ASCII.GetBytes($OwnerToken)
    try {
        [System.IO.Directory]::CreateDirectory($path) | Out-Null
        if (-not $IsWindows) {
            [System.IO.File]::SetUnixFileMode(
                $path,
                [System.IO.UnixFileMode]::UserRead -bor
                    [System.IO.UnixFileMode]::UserWrite -bor
                    [System.IO.UnixFileMode]::UserExecute
            )
        }
        $attributes = [System.IO.File]::GetAttributes($path)
        if (($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
            ($attributes -band [System.IO.FileAttributes]::Directory) -eq 0) {
            throw "private native TLS path became a file or reparse point: '$path'"
        }
        $markerStream = [System.IO.FileStream]::new(
            $marker,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        try {
            $markerStream.Write($markerBytes, 0, $markerBytes.Length)
            $markerStream.Flush($true)
        }
        finally {
            $markerStream.Dispose()
        }
    }
    catch {
        $failure = $_
        # Clean only the exact marker and directory allocated above. This also
        # covers a marker write/flush failure, which otherwise leaves a partial
        # owner marker that no caller can authenticate and clean later.
        try {
            if ((Test-Path -LiteralPath $path) -and
                ([System.IO.File]::GetAttributes($path) -band [System.IO.FileAttributes]::ReparsePoint) -eq 0 -and
                (Test-Path -LiteralPath $marker)) {
                $markerAttributes = [System.IO.File]::GetAttributes($marker)
                if (($markerAttributes -band [System.IO.FileAttributes]::ReparsePoint) -eq 0 -and
                    ($markerAttributes -band [System.IO.FileAttributes]::Directory) -eq 0) {
                    [System.IO.File]::Delete($marker)
                }
            }
            if (Test-Path -LiteralPath $path) {
                $pathAttributes = [System.IO.File]::GetAttributes($path)
                if (($pathAttributes -band [System.IO.FileAttributes]::ReparsePoint) -eq 0 -and
                    ($pathAttributes -band [System.IO.FileAttributes]::Directory) -ne 0 -and
                    [System.IO.Directory]::GetFileSystemEntries($path).Length -eq 0) {
                    [System.IO.Directory]::Delete($path, $false)
                }
            }
        }
        catch { }
        throw $failure
    }
    return $path
}

function Assert-KuTlsOwnedTree(
    [string]$Path,
    [string]$Root,
    [string]$Prefix,
    [string]$OwnerToken
) {
    if ($OwnerToken -cnotmatch '^[0-9a-f]{32}$') {
        throw "private native TLS owner token is invalid"
    }
    $pathFull = Get-KuTlsFullPath $Path
    $rootFull = (Get-KuTlsFullPath $Root).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $parent = [System.IO.Path]::GetDirectoryName($pathFull).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $leaf = [System.IO.Path]::GetFileName($pathFull)
    if (-not [string]::Equals($parent, $rootFull, $kuTlsPathComparison) -or
        -not $leaf.StartsWith($Prefix, [System.StringComparison]::Ordinal)) {
        throw "refusing to operate on an unverified native TLS private path '$pathFull'"
    }
    $rootAttributes = [System.IO.File]::GetAttributes($rootFull)
    $pathAttributes = [System.IO.File]::GetAttributes($pathFull)
    if (($rootAttributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        ($pathAttributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        ($pathAttributes -band [System.IO.FileAttributes]::Directory) -eq 0) {
        throw "refusing to operate on a replaced native TLS private directory '$pathFull'"
    }
    $marker = Join-Path $pathFull ".ku-private-owner"
    $markerInfo = Get-Item -LiteralPath $marker -Force
    if (($markerInfo.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        $markerInfo.PSIsContainer -or
        $markerInfo.Length -ne $OwnerToken.Length) {
        throw "native TLS private ownership marker changed at '$marker'"
    }
    $markerStream = [System.IO.FileStream]::new(
        $marker,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::Read
    )
    try {
        if ($markerStream.Length -ne $OwnerToken.Length) {
            throw "native TLS private ownership marker changed at '$marker'"
        }
        $markerBytes = [byte[]]::new($OwnerToken.Length)
        $markerRead = 0
        while ($markerRead -lt $markerBytes.Length) {
            $read = $markerStream.Read($markerBytes, $markerRead, $markerBytes.Length - $markerRead)
            if ($read -eq 0) { throw "native TLS private ownership marker changed at '$marker'" }
            $markerRead += $read
        }
        if ($markerStream.ReadByte() -ne -1 -or
            -not [string]::Equals(
                [System.Text.Encoding]::ASCII.GetString($markerBytes),
                $OwnerToken,
                [System.StringComparison]::Ordinal
            )) {
            throw "native TLS private ownership marker changed at '$marker'"
        }
    }
    finally {
        $markerStream.Dispose()
    }
    $pending = [System.Collections.Generic.Stack[string]]::new()
    $pending.Push($pathFull)
    $entries = 0
    while ($pending.Count -ne 0) {
        $directory = $pending.Pop()
        foreach ($entry in [System.IO.Directory]::EnumerateFileSystemEntries($directory)) {
            $entries++
            if ($entries -gt $kuTlsCleanupEntryLimit) {
                throw "native TLS private tree exceeds the $kuTlsCleanupEntryLimit-entry cleanup limit"
            }
            $attributes = [System.IO.File]::GetAttributes($entry)
            if (($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "native TLS private tree contains a forbidden reparse point: '$entry'"
            }
            if (($attributes -band [System.IO.FileAttributes]::Directory) -ne 0) {
                $pending.Push($entry)
            }
        }
    }
    return $marker
}

function Remove-KuTlsPrivateRoot(
    [string]$Path,
    [string]$Root,
    [string]$Prefix,
    [string]$OwnerToken
) {
    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }
    $pathFull = Get-KuTlsFullPath $Path
    $marker = Assert-KuTlsOwnedTree $pathFull $Root $Prefix $OwnerToken
    $pending = [System.Collections.Generic.Stack[string]]::new()
    $postorder = [System.Collections.Generic.Stack[string]]::new()
    $pending.Push($pathFull)
    $entries = 0
    while ($pending.Count -ne 0) {
        $directory = $pending.Pop()
        $postorder.Push($directory)
        foreach ($entry in [System.IO.Directory]::EnumerateFileSystemEntries($directory)) {
            if ([string]::Equals((Get-KuTlsFullPath $entry), $marker, $kuTlsPathComparison)) { continue }
            $entries++
            if ($entries -gt $kuTlsCleanupEntryLimit) {
                throw "native TLS private tree exceeds the $kuTlsCleanupEntryLimit-entry cleanup limit"
            }
            $attributes = [System.IO.File]::GetAttributes($entry)
            if (($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "native TLS private tree contains a forbidden reparse point during cleanup: '$entry'"
            }
            if (($attributes -band [System.IO.FileAttributes]::Directory) -ne 0) {
                $pending.Push($entry)
            }
            else {
                [System.IO.File]::Delete($entry)
            }
        }
    }
    while ($postorder.Count -ne 0) {
        $directory = $postorder.Pop()
        if (-not [string]::Equals($directory, $pathFull, $kuTlsPathComparison)) {
            [System.IO.Directory]::Delete($directory, $false)
            continue
        }
        $marker = Assert-KuTlsOwnedTree $pathFull $Root $Prefix $OwnerToken
        $remaining = @([System.IO.Directory]::EnumerateFileSystemEntries($pathFull))
        if ($remaining.Count -ne 1 -or
            -not [string]::Equals((Get-KuTlsFullPath $remaining[0]), $marker, $kuTlsPathComparison)) {
            throw "native TLS private root changed during cleanup"
        }
        [System.IO.File]::Delete($marker)
        try {
            [System.IO.Directory]::Delete($pathFull, $false)
        }
        catch {
            $failure = $_
            if ((Test-Path -LiteralPath $pathFull -PathType Container) -and
                ([System.IO.File]::GetAttributes($pathFull) -band [System.IO.FileAttributes]::ReparsePoint) -eq 0 -and
                -not (Test-Path -LiteralPath $marker)) {
                $markerStream = [System.IO.FileStream]::new(
                    $marker,
                    [System.IO.FileMode]::CreateNew,
                    [System.IO.FileAccess]::Write,
                    [System.IO.FileShare]::None
                )
                try {
                    $markerBytes = [System.Text.Encoding]::ASCII.GetBytes($OwnerToken)
                    $markerStream.Write($markerBytes, 0, $markerBytes.Length)
                    $markerStream.Flush($true)
                }
                finally {
                    $markerStream.Dispose()
                }
            }
            throw $failure
        }
    }
}

function Copy-KuTlsPinnedFile(
    [string]$Source,
    [string]$Destination,
    [long]$Limit,
    [bool]$RequireArchive,
    [bool]$RequireHeaderAbi
) {
    $sourceInfo = Get-Item -LiteralPath $Source -Force
    if ($sourceInfo.PSIsContainer -or
        ($sourceInfo.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "native TLS input must be a plain regular file: '$Source'"
    }
    $sourceStream = [System.IO.FileStream]::new(
        $Source,
        [System.IO.FileMode]::Open,
        [System.IO.FileAccess]::Read,
        [System.IO.FileShare]::Read
    )
    $destinationStream = $null
    try {
        $length = $sourceStream.Length
        if ($length -eq 0 -or $length -gt $Limit) {
            throw "native TLS input '$Source' is empty or exceeds its $Limit-byte limit"
        }
        $destinationStream = [System.IO.FileStream]::new(
            $Destination,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
        $hash = [System.Security.Cryptography.SHA256]::Create()
        try {
            $buffer = [byte[]]::new(65536)
            $copied = 0L
            $first = [byte[]]::new(8)
            while (($read = $sourceStream.Read($buffer, 0, $buffer.Length)) -ne 0) {
                if ($copied + $read -gt $length) {
                    throw "native TLS input '$Source' grew while it was copied"
                }
                if ($copied -lt 8) {
                    $take = [Math]::Min($read, 8 - [int]$copied)
                    [Array]::Copy($buffer, 0, $first, [int]$copied, $take)
                }
                $destinationStream.Write($buffer, 0, $read)
                [void]$hash.TransformBlock($buffer, 0, $read, $null, 0)
                $copied += $read
            }
            [void]$hash.TransformFinalBlock([byte[]]::new(0), 0, 0)
            $copiedHash = [Convert]::ToHexString($hash.Hash).ToLowerInvariant()
        }
        finally {
            $hash.Dispose()
        }
        if ($copied -ne $length -or $sourceStream.Length -ne $length) {
            throw "native TLS input '$Source' changed size while it was copied"
        }
        if ($RequireArchive) {
            $magic = [System.Text.Encoding]::ASCII.GetString($first)
            if ($magic -eq "!<thin>`n") {
                throw "thin native TLS archives are forbidden"
            }
            if ($magic -ne "!<arch>`n") {
                throw "native TLS output is not a complete static archive"
            }
        }
        $destinationStream.Flush($true)
        if ($destinationStream.Length -ne $length) {
            throw "native TLS private copy has the wrong size"
        }
        $sourceStream.Position = 0
        $sourceHashAlgorithm = [System.Security.Cryptography.SHA256]::Create()
        try {
            $sourceHash = [Convert]::ToHexString(
                $sourceHashAlgorithm.ComputeHash($sourceStream)
            ).ToLowerInvariant()
        }
        finally {
            $sourceHashAlgorithm.Dispose()
        }
        $destinationStream.Position = 0
        $destinationHashAlgorithm = [System.Security.Cryptography.SHA256]::Create()
        try {
            $destinationHash = [Convert]::ToHexString(
                $destinationHashAlgorithm.ComputeHash($destinationStream)
            ).ToLowerInvariant()
        }
        finally {
            $destinationHashAlgorithm.Dispose()
        }
        if ($sourceStream.Length -ne $length -or
            $sourceHash -ne $copiedHash -or
            $destinationHash -ne $copiedHash) {
            throw "native TLS input '$Source' changed or its private copy failed hash verification"
        }
        if ($RequireHeaderAbi) {
            $destinationStream.Position = 0
            $reader = [System.IO.StreamReader]::new(
                $destinationStream,
                [System.Text.UTF8Encoding]::new($false, $true),
                $false,
                4096,
                $true
            )
            try {
                $headerText = $reader.ReadToEnd()
            }
            finally {
                $reader.Dispose()
            }
            if ($headerText -notmatch '(?m)^#define KU_TLS_ABI_VERSION 1u\r?$') {
                throw "ku_native_tls.h does not declare the required ABI version 1"
            }
            $requiredHeaderSymbols = @(
                "ku_tls_abi_version", "ku_tls_v1_build_id", "ku_tls_v1_config_new",
                "ku_tls_v1_config_drop", "ku_tls_v1_client_new", "ku_tls_v1_client_drop",
                "ku_tls_v1_client_wants_read", "ku_tls_v1_client_wants_write",
                "ku_tls_v1_client_is_handshaking", "ku_tls_v1_client_peer_closed",
                "ku_tls_v1_client_feed_ciphertext", "ku_tls_v1_client_process",
                "ku_tls_v1_client_drain_ciphertext", "ku_tls_v1_client_write_plaintext",
                "ku_tls_v1_client_read_plaintext", "ku_tls_v1_client_send_close_notify",
                "ku_tls_v1_client_notify_eof"
            )
            foreach ($symbol in $requiredHeaderSymbols) {
                if ([regex]::Matches($headerText, "(?m)\b$([regex]::Escape($symbol))\s*\(").Count -ne 1) {
                    throw "ku_native_tls.h must declare exactly one '$symbol' ABI function"
                }
            }
        }
        Set-KuTlsPublicFileMode $Destination
        return [pscustomobject]@{
            Length = $length
            Sha256 = $copiedHash
        }
    }
    finally {
        if ($null -ne $destinationStream) {
            $destinationStream.Dispose()
        }
        $sourceStream.Dispose()
    }
}

function Invoke-KuTlsProcessSelfTest {
    $temporaryRoot = [System.IO.Path]::GetTempPath()
    $temporaryAttributes = [System.IO.File]::GetAttributes($temporaryRoot)
    if (($temporaryAttributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        ($temporaryAttributes -band [System.IO.FileAttributes]::Directory) -eq 0) {
        throw "native TLS self-test requires a plain system temporary directory"
    }
    $python = Get-Command "python3" -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $python) {
        $python = Get-Command "python" -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    }
    if ($null -eq $python) {
        throw "native TLS self-test requires Python to exercise process-tree cleanup"
    }

    $owner = [Guid]::NewGuid().ToString("N")
    $workRoot = New-KuTlsPrivateRoot $temporaryRoot ".ku-tls-process-selftest-" $owner
    try {
        if (-not $IsWindows) {
            $modeDirectory = Join-Path $workRoot "public-mode-directory"; [System.IO.Directory]::CreateDirectory($modeDirectory) | Out-Null; Set-KuTlsPublicDirectoryMode $modeDirectory
            $modeFile = Join-Path $modeDirectory "public-mode-file"; [System.IO.File]::WriteAllText($modeFile, "mode probe", [Text.Encoding]::ASCII); Set-KuTlsPublicFileMode $modeFile
        }
        function Assert-KuTlsSelfTestChildGone([string]$PidPath, [string]$Case) {
            $attributes = if (Test-Path -LiteralPath $PidPath) { [System.IO.File]::GetAttributes($PidPath) } else { [System.IO.FileAttributes]::Directory }
            if (($attributes -band [System.IO.FileAttributes]::Directory) -ne 0 -or
                ($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "$Case self-test did not record a plain child PID file"
            }
            $childPid = [int]([System.IO.File]::ReadAllText($PidPath))
            $deadline = [DateTime]::UtcNow.AddSeconds(2)
            $alive = $true
            do {
                try {
                    $child = [Diagnostics.Process]::GetProcessById($childPid)
                    $alive = -not $child.HasExited
                    $child.Dispose()
                }
                catch [ArgumentException] {
                    $alive = $false
                }
                if ($alive) { Start-Sleep -Milliseconds 20 }
            } while ($alive -and [DateTime]::UtcNow -lt $deadline)
            if ($alive) { throw "$Case self-test left child PID $childPid alive" }
        }

        $orphanPid = Join-Path $workRoot "orphan-child.pid"
        $orphanProgram = 'import subprocess,sys; p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]); open(sys.argv[1],"w").write(str(p.pid))'
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $orphan = Invoke-KuTlsBoundedProcess ([string]$python.Source) @("-c", $orphanProgram, $orphanPid) $workRoot $workRoot "orphan-tree" 10000
        $clock.Stop()
        if ($orphan.ExitCode -ne 0) { throw "native TLS orphan self-test parent failed" }
        Assert-KuTlsSelfTestChildGone $orphanPid "native TLS orphan"
        if ($clock.ElapsedMilliseconds -gt 15000) { throw "native TLS orphan self-test exceeded its cleanup bound" }

        $outputPid = Join-Path $workRoot "output-child.pid"
        $outputProgram = 'import subprocess,sys; p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]); open(sys.argv[1],"w").write(str(p.pid)); sys.stdout.buffer.write((b"x"*4095+b"\n")*512); sys.stdout.flush()'
        $outputRejected = $false
        $outputFailure = $null
        try { [void](Invoke-KuTlsBoundedProcess ([string]$python.Source) @("-c", $outputProgram, $outputPid) $workRoot $workRoot "output-budget" 10000) }
        catch { $outputFailure = $_.Exception.Message; $outputRejected = $outputFailure -like "*output exceeded*" }
        if (-not $outputRejected) {
            $observed = if ($null -eq $outputFailure) { "no rejection" } else { "unexpected rejection: $outputFailure" }
            throw "native TLS output-budget self-test did not report the bounded-output contract ($observed)"
        }
        Assert-KuTlsSelfTestChildGone $outputPid "native TLS output-budget"
        $captured = 0L
        foreach ($log in Get-ChildItem -LiteralPath $workRoot -File | Where-Object Name -Like "output-budget-*.std*") { $captured += $log.Length }
        if ($captured -gt $kuTlsProcessOutputLimit) { throw "native TLS output-budget self-test captured more than its configured limit" }

        $timeoutPid = Join-Path $workRoot "timeout-child.pid"
        $timeoutProgram = 'import subprocess,sys,time; p=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"]); open(sys.argv[1],"w").write(str(p.pid)); time.sleep(60)'
        $timeoutRejected = $false
        try { [void](Invoke-KuTlsBoundedProcess ([string]$python.Source) @("-c", $timeoutProgram, $timeoutPid) $workRoot $workRoot "direct-timeout" 2000) }
        catch { $timeoutRejected = $_.Exception.Message -like "*exceeded its 2000-ms deadline*" }
        if (-not $timeoutRejected) { throw "native TLS direct-timeout self-test was not rejected" }
        Assert-KuTlsSelfTestChildGone $timeoutPid "native TLS timeout"
    }
    finally {
        Remove-KuTlsPrivateRoot $workRoot $temporaryRoot ".ku-tls-process-selftest-" $owner
    }
    if (Test-Path -LiteralPath $workRoot) {
        throw "native TLS process self-test root could not be removed immediately"
    }
    if ($IsWindows) {
        $cleanupOwner = [Guid]::NewGuid().ToString("N")
        $cleanupRoot = New-KuTlsPrivateRoot $temporaryRoot ".ku-tls-cleanup-selftest-" $cleanupOwner
        $lockedStream = $null
        try {
            $lockedStream = [System.IO.FileStream]::new(
                (Join-Path $cleanupRoot "locked"),
                [System.IO.FileMode]::CreateNew,
                [System.IO.FileAccess]::ReadWrite,
                [System.IO.FileShare]::Read
            )
            $cleanupFailed = $false
            try {
                Remove-KuTlsPrivateRoot $cleanupRoot $temporaryRoot ".ku-tls-cleanup-selftest-" $cleanupOwner
            }
            catch {
                $cleanupFailed = $true
            }
            if (-not $cleanupFailed) {
                throw "native TLS cleanup self-test unexpectedly deleted a locked file"
            }
            Assert-KuTlsOwnedTree $cleanupRoot $temporaryRoot ".ku-tls-cleanup-selftest-" $cleanupOwner | Out-Null
        }
        finally {
            if ($null -ne $lockedStream) { $lockedStream.Dispose() }
        }
        Remove-KuTlsPrivateRoot $cleanupRoot $temporaryRoot ".ku-tls-cleanup-selftest-" $cleanupOwner
        if (Test-Path -LiteralPath $cleanupRoot) {
            throw "native TLS cleanup self-test could not retry an authenticated tree"
        }
    }
    Write-Output "native TLS process self-test ok: orphan, output-budget, timeout, and retryable cleanup"
}

$kuTlsScriptRoot = Split-Path -Parent $PSCommandPath
$kuTlsRepositoryRoot = Get-KuTlsFullPath (Join-Path $kuTlsScriptRoot "..")
$kuTlsCargoManifest = Join-Path $kuTlsRepositoryRoot "Cargo.toml"
$kuTlsHeaderSource = Join-Path $kuTlsRepositoryRoot "crates/ku-native-tls/include/ku_native_tls.h"
$kuTlsCrateSource = Join-Path $kuTlsRepositoryRoot "crates/ku-native-tls/src/lib.rs"
$kuTlsCliSource = Join-Path $kuTlsRepositoryRoot "src/cli.rs"
$kuTlsBackendSource = Join-Path $kuTlsRepositoryRoot "src/backend/c.rs"
$kuTlsArchiveValidatorSource = Join-Path $kuTlsRepositoryRoot "src/native_tls_archive.rs"
if (-not (Test-Path -LiteralPath $kuTlsCargoManifest -PathType Leaf) -or
    -not (Test-Path -LiteralPath $kuTlsHeaderSource -PathType Leaf) -or
    -not (Test-Path -LiteralPath $kuTlsCrateSource -PathType Leaf) -or
    -not (Test-Path -LiteralPath $kuTlsCliSource -PathType Leaf) -or
    -not (Test-Path -LiteralPath $kuTlsBackendSource -PathType Leaf) -or
    -not (Test-Path -LiteralPath $kuTlsArchiveValidatorSource -PathType Leaf)) {
    throw "build-native-tls-pack.ps1 must run from the Ku source tree"
}
Assert-KuTlsBuildIdSources $kuTlsCrateSource $kuTlsCliSource $kuTlsBackendSource $kuTlsBuildId
if ($SelfTest) {
    Invoke-KuTlsProcessSelfTest
    return
}
if (-not [System.IO.Path]::IsPathRooted($OutputRoot)) {
    throw "OutputRoot must be an absolute directory"
}

$kuTlsOutputRoot = Get-KuTlsFullPath $OutputRoot
New-Item -ItemType Directory -Path $kuTlsOutputRoot -Force | Out-Null
$kuTlsOutputAttributes = [System.IO.File]::GetAttributes($kuTlsOutputRoot)
if (($kuTlsOutputAttributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
    ($kuTlsOutputAttributes -band [System.IO.FileAttributes]::Directory) -eq 0) {
    throw "OutputRoot must be a plain directory, not a file, symlink, or reparse point"
}
$kuTlsDestination = Join-Path $kuTlsOutputRoot $Target
if (Test-Path -LiteralPath $kuTlsDestination) {
    throw "target-pack destination already exists: '$kuTlsDestination'"
}

$kuTlsContract = $kuTlsContracts[$Target]
$kuTlsOwnerToken = [Guid]::NewGuid().ToString("N")
$kuTlsStagePrefix = ".ktp-"
$kuTlsBuildPrefix = ".ktb-"
$kuTlsStage = $null
$kuTlsBuildRoot = $null
$kuTlsPayload = $null

try {
    $kuTlsStage = New-KuTlsPrivateRoot `
        $kuTlsOutputRoot `
        $kuTlsStagePrefix `
        $kuTlsOwnerToken
    $kuTlsBuildRoot = New-KuTlsPrivateRoot `
        $kuTlsOutputRoot `
        $kuTlsBuildPrefix `
        $kuTlsOwnerToken
    $kuTlsPayload = Join-Path $kuTlsStage "payload"
    [System.IO.Directory]::CreateDirectory($kuTlsPayload) | Out-Null
    [System.IO.Directory]::CreateDirectory((Join-Path $kuTlsPayload "include")) | Out-Null
    [System.IO.Directory]::CreateDirectory((Join-Path $kuTlsPayload "lib")) | Out-Null
    Set-KuTlsPublicDirectoryMode $kuTlsPayload
    Set-KuTlsPublicDirectoryMode (Join-Path $kuTlsPayload "include")
    Set-KuTlsPublicDirectoryMode (Join-Path $kuTlsPayload "lib")
    $kuTlsCargoTarget = Join-Path $kuTlsBuildRoot "target"

    $kuTlsRustc = Invoke-KuTlsBoundedProcess `
        "rustc" `
        @("+1.89.0", "--version") `
        $kuTlsRepositoryRoot `
        $kuTlsBuildRoot `
        "rustc-version" `
        $kuTlsToolProbeTimeoutMs
    if ($kuTlsRustc.ExitCode -ne 0 -or
        $kuTlsRustc.Output -notmatch '^rustc 1\.89\.0 \(') {
        throw "native TLS packs require the exact Rust 1.89.0 toolchain"
    }

    $kuTlsCargo = Invoke-KuTlsBoundedProcess `
        "cargo" `
        @(
            "+1.89.0",
            "rustc",
            "--locked",
            "--release",
            "--color",
            "never",
            "--manifest-path",
            $kuTlsCargoManifest,
            "--target-dir",
            $kuTlsCargoTarget,
            "-p",
            "ku-native-tls",
            "--target",
            $Target,
            "--",
            "--print",
            "native-static-libs"
        ) `
        $kuTlsRepositoryRoot `
        $kuTlsBuildRoot `
        "cargo-build" `
        $kuTlsCargoTimeoutMs
    $kuTlsCargoText = $kuTlsCargo.Output
    if ($kuTlsCargo.ExitCode -ne 0) {
        throw "ku-native-tls staticlib build failed for '$Target':`n$kuTlsCargoText"
    }
    $kuTlsNativeMatches = [regex]::Matches(
        $kuTlsCargoText,
        '(?m)^\s*(?:note:\s*)?native-static-libs:\s*(?<libs>[^\r\n]+)\s*$'
    )
    if ($kuTlsNativeMatches.Count -ne 1) {
        throw "rustc did not report exactly one bounded native-static-libs contract for '$Target'"
    }
    $kuTlsActualNativeLibs = $kuTlsNativeMatches[0].Groups['libs'].Value.Trim()
    $kuTlsExpectedNativeLibs = [string]$kuTlsContract.NativeStaticLibs
    if (-not [string]::Equals(
        $kuTlsActualNativeLibs,
        $kuTlsExpectedNativeLibs,
        [System.StringComparison]::Ordinal
    )) {
        throw "native-static-libs contract mismatch for '$Target'. expected: '$kuTlsExpectedNativeLibs'; actual: '$kuTlsActualNativeLibs'"
    }

    $kuTlsArchiveSource = Join-Path `
        (Join-Path (Join-Path $kuTlsCargoTarget $Target) "release") `
        ([string]$kuTlsContract.Archive)
    if (-not (Test-Path -LiteralPath $kuTlsArchiveSource -PathType Leaf)) {
        throw "cargo succeeded but the expected static archive is missing: '$kuTlsArchiveSource'"
    }

    $kuTlsArchiveDestination = Join-Path (Join-Path $kuTlsPayload "lib") ([string]$kuTlsContract.Archive)
    $kuTlsHeaderDestination = Join-Path (Join-Path $kuTlsPayload "include") "ku_native_tls.h"
    $kuTlsArchiveSnapshot = Copy-KuTlsPinnedFile `
        $kuTlsArchiveSource `
        $kuTlsArchiveDestination `
        $kuTlsArchiveLimit `
        $true `
        $false
    $kuTlsHeaderSnapshot = Copy-KuTlsPinnedFile `
        $kuTlsHeaderSource `
        $kuTlsHeaderDestination `
        $kuTlsHeaderLimit `
        $false `
        $true

    $kuTlsValidatorModule = Join-Path $kuTlsBuildRoot "native_tls_archive.rs"
    [void](Copy-KuTlsPinnedFile `
        $kuTlsArchiveValidatorSource `
        $kuTlsValidatorModule `
        1MB `
        $false `
        $false)
    $kuTlsValidatorSource = Join-Path $kuTlsBuildRoot "validate_tls_archive.rs"
    $kuTlsValidatorText = @'
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
    [IO.File]::WriteAllText($kuTlsValidatorSource, $kuTlsValidatorText, [Text.UTF8Encoding]::new($false))
    $kuTlsValidatorBinary = Join-Path $kuTlsBuildRoot $(if ($IsWindows) { "validate_tls_archive.exe" } else { "validate_tls_archive" })
    $kuTlsValidatorBuild = Invoke-KuTlsBoundedProcess `
        "rustc" `
        @("+1.89.0", "--edition=2021", "-C", "panic=abort", "-o", $kuTlsValidatorBinary, $kuTlsValidatorSource) `
        $kuTlsBuildRoot `
        $kuTlsBuildRoot `
        "archive-validator-build" `
        $kuTlsToolProbeTimeoutMs
    if ($kuTlsValidatorBuild.ExitCode -ne 0) {
        throw "failed to build the bounded native TLS archive validator:`n$($kuTlsValidatorBuild.Output)"
    }
    $kuTlsValidation = Invoke-KuTlsBoundedProcess `
        $kuTlsValidatorBinary `
        @($kuTlsArchiveDestination, [string]$kuTlsContract.ObjectFormat, $kuTlsBuildId) `
        $kuTlsBuildRoot `
        $kuTlsBuildRoot `
        "archive-validator-run" `
        $kuTlsToolProbeTimeoutMs
    if ($kuTlsValidation.ExitCode -ne 0) {
        throw "native TLS archive failed structural ABI validation:`n$($kuTlsValidation.Output)"
    }
    $kuTlsManifest = @(
        "format=ku-native-tls-pack-v1"
        "target=$Target"
        "flavor=$($kuTlsContract.Flavor)"
        "object_format=$($kuTlsContract.ObjectFormat)"
        "abi_version=1"
        "panic=unwind"
        "build_id=$kuTlsBuildId"
        "archive_size=$($kuTlsArchiveSnapshot.Length)"
        "archive_sha256=$($kuTlsArchiveSnapshot.Sha256)"
        "header_size=$($kuTlsHeaderSnapshot.Length)"
        "header_sha256=$($kuTlsHeaderSnapshot.Sha256)"
        "link_contract=$($kuTlsContract.LinkContract)"
        "crt=$($kuTlsContract.Crt)"
        "runtime_dependency=none"
    ) -join "`n"
    $kuTlsManifest += "`n"
    $kuTlsManifestBytes = [System.Text.Encoding]::ASCII.GetBytes($kuTlsManifest)
    if ($kuTlsManifestBytes.LongLength -gt $kuTlsManifestLimit) {
        throw "generated native TLS manifest exceeds 64 KiB"
    }
    $kuTlsManifestPath = Join-Path $kuTlsPayload "manifest.kutls"
    $kuTlsManifestStream = [System.IO.FileStream]::new(
        $kuTlsManifestPath,
        [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        $kuTlsManifestStream.Write($kuTlsManifestBytes, 0, $kuTlsManifestBytes.Length)
        $kuTlsManifestStream.Flush($true)
    }
    finally {
        $kuTlsManifestStream.Dispose()
    }
    Set-KuTlsPublicFileMode $kuTlsManifestPath

    # The ownership marker remains in the private parent. Only the marker-free
    # payload is published, so a failed move never requires recreating cleanup
    # authority and a successful pack cannot leak an internal owner marker.
    Assert-KuTlsOwnedTree `
        $kuTlsStage `
        $kuTlsOutputRoot `
        $kuTlsStagePrefix `
        $kuTlsOwnerToken | Out-Null
    # Directory.Move fails atomically when another pack builder won the same
    # destination race. Move-Item could instead nest the payload.
    [System.IO.Directory]::Move($kuTlsPayload, $kuTlsDestination)
    Write-Output "created native TLS target pack: $kuTlsDestination"
}
finally {
    try {
        if ($null -ne $kuTlsStage) {
            Remove-KuTlsPrivateRoot `
                $kuTlsStage `
                $kuTlsOutputRoot `
                $kuTlsStagePrefix `
                $kuTlsOwnerToken
        }
    }
    finally {
        if ($null -ne $kuTlsBuildRoot) {
            Remove-KuTlsPrivateRoot `
                $kuTlsBuildRoot `
                $kuTlsOutputRoot `
                $kuTlsBuildPrefix `
                $kuTlsOwnerToken
        }
    }
}
