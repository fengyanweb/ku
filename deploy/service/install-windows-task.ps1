param(
    [Parameter(Mandatory = $true)][string]$Config,
    [Parameter(Mandatory = $true)][string]$Python,
    [string]$Account = 'NT AUTHORITY\LOCAL SERVICE',
    [string]$TaskPrefix = 'Ku-Service',
    [switch]$Register
)
$ErrorActionPreference = 'Stop'
$configurationPath = (Resolve-Path -LiteralPath $Config).Path
$pythonPath = (Resolve-Path -LiteralPath $Python).Path
$managerPath = Join-Path $PSScriptRoot 'manage.py'
$configuration = Get-Content -LiteralPath $configurationPath -Raw -Encoding UTF8 | ConvertFrom-Json
if ($TaskPrefix -notmatch '^[A-Za-z0-9_-]{1,64}$') { throw 'Invalid task prefix' }

function Quote-WindowsArgument([string]$Argument) {
    # CommandLineToArgvW / C runtime escaping, including trailing backslashes.
    $escaped = [regex]::Replace($Argument, '(\\*)"', '$1$1\"')
    $escaped = [regex]::Replace($escaped, '(\\+)$', '$1$1')
    return '"' + $escaped + '"'
}

$plans = @()
foreach ($slot in $configuration.slots) {
    if ($slot.name -notmatch '^[A-Za-z0-9_]+$') { throw 'Invalid slot name' }
    $arguments = @('-X', 'utf8', '-B', $managerPath, 'run', $configurationPath, [string]$slot.name)
    $quoted = ($arguments | ForEach-Object { Quote-WindowsArgument $_ }) -join ' '
    $name = $TaskPrefix + '-' + $slot.name
    $plans += [pscustomobject]@{
        task = $name
        execute = $pythonPath
        arguments = $quoted
        restart_count = 3
        restart_interval_seconds = 60
        initial_active = $slot.name -in @('a', 'b')
    }
    if ($Register) {
        $action = New-ScheduledTaskAction -Execute $pythonPath -Argument $quoted
        # Task Scheduler's RestartOnFailure schema requires at least one minute.
        $settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
            -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew `
            -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        $principal = New-ScheduledTaskPrincipal -UserId $Account -LogonType ServiceAccount -RunLevel Limited
        $parameters = @{ Action = $action; Settings = $settings; Principal = $principal }
        if ($slot.name -in @('a', 'b')) { $parameters.Trigger = New-ScheduledTaskTrigger -AtStartup }
        $task = New-ScheduledTask @parameters
        Register-ScheduledTask -TaskName $name -InputObject $task -Force | Out-Null
    }
}
$plans | ConvertTo-Json -Depth 4
