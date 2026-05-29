$ErrorActionPreference = 'Stop'
$installDir = "$env:LOCALAPPDATA\Himalaya"

Write-Host ""
Write-Host "  Himalaya Code - Uninstaller" -ForegroundColor DarkGray
Write-Host ""

if (-not (Test-Path $installDir)) {
    Write-Host "  ->  $installDir not found, nothing to remove." -ForegroundColor Yellow
    Read-Host "Press Enter to close"
    exit 0
}

$confirm = Read-Host "  Remove Himalaya Code from $installDir ? [y/N]"
if ($confirm -notmatch '^[Yy]$') {
    Write-Host "  Cancelled." -ForegroundColor Yellow
    exit 0
}

# Remove files
Remove-Item -Recurse -Force $installDir
Write-Host "  ok  Removed $installDir" -ForegroundColor Green

# Remove from user PATH
$current = [Environment]::GetEnvironmentVariable('PATH', 'User')
$updated = ($current -split ';' | Where-Object { $_ -ne $installDir }) -join ';'
if ($updated -ne $current) {
    [Environment]::SetEnvironmentVariable('PATH', $updated, 'User')
    Write-Host "  ok  Removed from user PATH" -ForegroundColor Green
}

Write-Host ""
Write-Host "Himalaya Code has been uninstalled." -ForegroundColor Green
Write-Host "Restart your terminal to apply PATH changes."
Write-Host ""
Read-Host "Press Enter to close"
