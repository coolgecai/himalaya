$ErrorActionPreference = 'Stop'
$installDir = "$env:LOCALAPPDATA\Himalaya"

Write-Host ""
Write-Host "  _   _ _                 _               " -ForegroundColor Red
Write-Host " | | | (_)_ __ ___   __ _| | __ _ _   _  __ _ " -ForegroundColor Red
Write-Host " | |_| | | '_ ' _ \ / _' | |/ _' | | | |/ _' |" -ForegroundColor Red
Write-Host " |  _  | | | | | | | (_| | | (_| | |_| | (_| |" -ForegroundColor Red
Write-Host " |_| |_|_|_| |_| |_|\__,_|_|\__,_|\__, |\__,_|" -ForegroundColor Red
Write-Host "                                   |___/       " -ForegroundColor Red
Write-Host ""
Write-Host "  Himalaya Code - Windows Installer" -ForegroundColor DarkGray
Write-Host ""

# Install directory
if (-not (Test-Path $installDir)) {
    New-Item -ItemType Directory -Path $installDir | Out-Null
}

$src = Join-Path $PSScriptRoot "Himalaya.exe"
$dst = Join-Path $installDir "Himalaya.exe"
Copy-Item $src $dst -Force
Write-Host "  ok  Installed to $dst" -ForegroundColor Green

$uninstallSrc = Join-Path $PSScriptRoot "uninstall.ps1"
$uninstallDst = Join-Path $installDir "uninstall.ps1"
Copy-Item $uninstallSrc $uninstallDst -Force
Write-Host "  ok  Uninstaller at $uninstallDst" -ForegroundColor Green

# Add to user PATH
$current = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($current -notlike "*$installDir*") {
    $newPath = if ($current) { "$current;$installDir" } else { $installDir }
    [Environment]::SetEnvironmentVariable('PATH', $newPath, 'User')
    Write-Host "  ok  Added $installDir to user PATH" -ForegroundColor Green
    Write-Host "  ->  Restart your terminal to use 'Himalaya'" -ForegroundColor Yellow
} else {
    Write-Host "  ok  $installDir already in PATH" -ForegroundColor Green
}

Write-Host ""
Write-Host "Himalaya Code is installed!" -ForegroundColor Green
Write-Host ""
Write-Host "  Binary:  $dst"
Write-Host ""
Write-Host "Quick start:"
Write-Host "  Himalaya                          # interactive REPL"
Write-Host "  Himalaya -p `"summarise this repo`" # one-shot prompt"
Write-Host ""
Read-Host "Press Enter to close"
