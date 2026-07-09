#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Unregisters the blackhole TSF IME DLL from Windows.

.DESCRIPTION
    Unregisters the blackhole_platform.dll using regsvr32 /u.
    This removes COM and CTF/TIP registry keys (including WOW6432Node).
    Must be run as Administrator.
#>

param(
    [switch]$Release
)

$ErrorActionPreference = "Stop"

$currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error "This script must be run as Administrator."
    exit 1
}

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$profile = if ($Release) { "release" } else { "debug" }
$dllPath = Join-Path $repoRoot "target" $profile "blackhole_platform.dll"

if (-not (Test-Path $dllPath)) {
    Write-Error "DLL not found at $dllPath. Build the project first or specify -Release."
    exit 1
}

Write-Host "Unregistering blackhole IME DLL: $dllPath"
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = "regsvr32"
$psi.Arguments = "/u /s `"$dllPath`""
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$proc = [System.Diagnostics.Process]::Start($psi)
$proc.WaitForExit()

if ($proc.ExitCode -ne 0) {
    Write-Error "regsvr32 /u failed with exit code $($proc.ExitCode)."
    exit 1
}

# Also clean up any orphaned registry keys manually
$clsid = "{A1B2C3D4-E5F6-7890-1234-567890ABCDEF}"
$orphanedKeys = @(
    "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\CTF\TIP\$clsid",
    "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\CTF\TIP\$clsid",
    "Registry::HKEY_CLASSES_ROOT\CLSID\$clsid"
)
foreach ($key in $orphanedKeys) {
    if (Test-Path $key) {
        Write-Warning "Removing orphaned key: $key"
        Remove-Item -Path $key -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "Unregistration successful."
Write-Host "You may need to remove 'blackhole IME' manually from Settings > Time & language > Language & region > Keyboards if it is still listed."
Write-Host "A restart of Explorer or your PC may be required for the change to take full effect."
