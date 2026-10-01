<#
.SYNOPSIS
    Builds AudioBridge for Windows and installs it for the current user.

.DESCRIPTION
    The release exe installs itself: started from anywhere other than
    %LOCALAPPDATA%\Programs\AudioBridge\AudioBridge.exe it stops a running older copy, copies
    itself there, registers autostart (HKCU\...\Run\AudioBridge = "<exe>" --background), starts
    the installed copy and exits. On another PC, copying AudioBridge.exe and double-clicking it
    is all that is needed.

    This script:
    1. cargo build -p audiobridge-desktop --release (skip with -SkipBuild)
    2. Optionally adds an inbound Windows Firewall allow rule for the installed exe (all
       profiles). Only this step needs admin rights; the script elevates itself just for it.
       Without the rule Windows asks once when AudioBridge first listens.
    3. Runs the built exe with --background, which self-installs and starts in the tray.

.PARAMETER SkipBuild
    Use the already-built target\release\AudioBridge.exe.

.PARAMETER NoFirewall
    Do not add the firewall rule.
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$NoFirewall,
    # Internal: used by the elevated child process that only adds the firewall rule.
    [string]$FirewallOnly
)

$ErrorActionPreference = 'Stop'
$RuleName = 'AudioBridge'

function Set-FirewallRule([string]$Exe) {
    Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue | Remove-NetFirewallRule
    New-NetFirewallRule -DisplayName $RuleName -Direction Inbound -Action Allow `
        -Program $Exe -Profile Any -Protocol Any -Description 'AudioBridge (PC audio / phone mic over QUIC)' | Out-Null
}

if ($FirewallOnly) {
    Set-FirewallRule $FirewallOnly
    exit 0
}

$RepoRoot = Split-Path -Parent $PSScriptRoot
$Target = Join-Path $env:LOCALAPPDATA 'Programs\AudioBridge\AudioBridge.exe'
$Built = Join-Path $RepoRoot 'target\release\AudioBridge.exe'

if (-not $SkipBuild) {
    Write-Host '==> cargo build -p audiobridge-desktop --release'
    Push-Location $RepoRoot
    try {
        cargo build -p audiobridge-desktop --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}
if (-not (Test-Path $Built)) { throw "Not found: $Built" }

if (-not $NoFirewall) {
    Write-Host '==> Firewall rule (inbound allow, all profiles)'
    $existing = Get-NetFirewallRule -DisplayName $RuleName -ErrorAction SilentlyContinue |
        Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |
        Where-Object { $_.Program -eq $Target }
    $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($existing) {
        Write-Host '    already present'
    } elseif ($isAdmin) {
        Set-FirewallRule $Target
    } else {
        $shell = (Get-Process -Id $PID).Path
        try {
            $p = Start-Process -FilePath $shell -Verb RunAs -Wait -PassThru -ArgumentList @(
                '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-FirewallOnly', "`"$Target`"")
            if ($p.ExitCode -ne 0) { Write-Warning "Firewall rule was not added (exit $($p.ExitCode))." }
        } catch {
            Write-Warning "Firewall rule was not added (elevation declined): $($_.Exception.Message)"
        }
    }
}

Write-Host '==> Starting the built exe (it installs itself and moves to the tray)'
# Not -Wait: that would also wait for the installed copy it launches.
(Start-Process -FilePath $Built -ArgumentList '--background' -PassThru).WaitForExit()
$deadline = (Get-Date).AddSeconds(20)
do {
    Start-Sleep -Milliseconds 300
    $running = Get-Process -Name 'AudioBridge' -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Target }
} until ($running -or (Get-Date) -gt $deadline)
if (-not $running) { throw "AudioBridge did not start from $Target" }
Write-Host "Done. Installed at $Target (pid $($running.Id)); AudioBridge is in the notification area."
