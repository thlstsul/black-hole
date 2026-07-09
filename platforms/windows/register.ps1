#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Registers the blackhole TSF IME DLL so it appears in Windows Settings.

.DESCRIPTION
    Builds the blackhole-platform crate as a cdylib (if needed) and registers
    the resulting DLL with regsvr32. Registration writes COM and CTF/TIP
    keys to the registry under HKEY_CLASSES_ROOT and HKEY_LOCAL_MACHINE.
    Must be run as Administrator.
#>

param(
    [switch]$Release,
    [switch]$Rebuild
)

$ErrorActionPreference = "Stop"

# Verify admin privileges
$currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error "This script must be run as Administrator. Right-click PowerShell and select 'Run as administrator'."
    exit 1
}

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$profile = if ($Release) { "release" } else { "debug" }
$dllPath = Join-Path $repoRoot "target" $profile "blackhole_platform.dll"

# Build if missing or -Rebuild specified
if ($Rebuild -or -not (Test-Path $dllPath)) {
    if ($Rebuild -and (Test-Path $dllPath)) {
        Write-Host "-Rebuild specified. Rebuilding DLL..."
    } else {
        Write-Host "DLL not found at $dllPath. Building now..."
    }
    Push-Location $repoRoot
    $buildArgs = @("build", "-p", "blackhole-platform")
    if ($Release) { $buildArgs += "--release" }
    & cargo @buildArgs
    Pop-Location
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Build failed. If the error says 'failed to remove file', the DLL is locked by a running process (ctfmon/TextInputHost). Run .\unregister.ps1 first, then retry."
        exit 1
    }
}

Write-Host "Registering blackhole IME DLL: $dllPath"

# Use non-silent mode first to capture any error dialog text
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = "regsvr32"
$psi.Arguments = "/s `"$dllPath`""
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$proc = [System.Diagnostics.Process]::Start($psi)
$proc.WaitForExit()

if ($proc.ExitCode -ne 0) {
    Write-Error "regsvr32 failed with exit code $($proc.ExitCode)."
    Write-Error "Stdout: $($proc.StandardOutput.ReadToEnd())"
    Write-Error "Stderr: $($proc.StandardError.ReadToEnd())"
    Write-Error "Check that the DLL exports DllRegisterServer correctly and that you are running as Administrator."
    exit 1
}

Write-Host "regsvr32 reported success."
Write-Host ""

# ---------------------------------------------------------------------------
# Verify registry keys were actually written
# ---------------------------------------------------------------------------
$clsid = "{A1B2C3D4-E5F6-7890-1234-567890ABCDEF}"
$keysToCheck = @(
    "Registry::HKEY_CLASSES_ROOT\CLSID\$clsid",
    "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\CTF\TIP\$clsid",
    "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\CTF\TIP\$clsid\LanguageProfile\0804\{B2C3D4E5-F678-9012-3456-7890ABCDEF01}",
    "Registry::HKEY_LOCAL_MACHINE\SOFTWARE\WOW6432Node\Microsoft\CTF\TIP\$clsid"
)

$allOk = $true
foreach ($key in $keysToCheck) {
    if (Test-Path $key) {
        Write-Host "  [OK] $key"
    } else {
        Write-Warning "  [MISSING] $key"
        $allOk = $false
    }
}

if (-not $allOk) {
    Write-Warning "Some expected registry keys are missing. The IME may not appear in Settings."
    Write-Warning "Try running regsvr32 manually without /s to see error dialogs:"
    Write-Warning "  regsvr32 `"$dllPath`""
}

Write-Host ""
Write-Host "Restarting TSF / input-related processes..."

# Windows 11: TextInputHost manages the input list
$processesToRestart = @("ctfmon", "TextInputHost")
foreach ($procName in $processesToRestart) {
    $procs = Get-Process -Name $procName -ErrorAction SilentlyContinue
    if ($procs) {
        Write-Host "  Stopping $procName..."
        Stop-Process -Name $procName -Force -ErrorAction SilentlyContinue
        Start-Sleep -Milliseconds 500
    }
}

# Restart ctfmon (it will auto-restart on next input focus, but we can nudge it)
$ctfmonPath = Join-Path $env:SystemRoot "System32" "ctfmon.exe"
if (Test-Path $ctfmonPath) {
    Write-Host "  Starting ctfmon..."
    Start-Process $ctfmonPath
}

Write-Host ""
Write-Host "Registration complete."
Write-Host ""
Write-Host "Next steps:"
Write-Host "  1. Open Settings > Time & language > Language & region"
Write-Host "  2. Click your language (e.g. 'Chinese (Simplified)') → 'Language options'"
Write-Host "  3. Under 'Keyboards', click 'Add a keyboard'"
Write-Host "  4. Look for 'blackhole IME' in the list"
Write-Host ""
Write-Host "If it still does not appear:"
Write-Host "  - Restart your PC (Windows 11 caches the input list)"
Write-Host "  - Or run this script again after reboot"
Write-Host ""
Write-Host "To unregister, run: .\unregister.ps1"
