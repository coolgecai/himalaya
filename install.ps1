# Himalaya Code — Windows installer (build from source)
#
# Usage:
#   .\install.ps1                  # release build, adds to PATH
#   .\install.ps1 -Profile debug   # debug build
#   .\install.ps1 -NoVerify        # skip post-install check
#   .\install.ps1 -InstallDir C:\Tools\Himalaya
#
# Requirements: Rust toolchain (rustup), Git (optional)
# Install Rust: https://rustup.rs

[CmdletBinding()]
param(
    [ValidateSet('release','debug')]
    [string]$Profile = 'release',

    [string]$InstallDir = "$env:LOCALAPPDATA\Himalaya",

    [switch]$NoVerify,
    [switch]$Help
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$BuildJobs = if ($env:Himalaya_BUILD_JOBS) { $env:Himalaya_BUILD_JOBS } else { '1' }

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

function Write-Step([string]$msg) {
    Write-Host ""
    Write-Host "==> $msg" -ForegroundColor Cyan
}

function Write-Ok([string]$msg)   { Write-Host "  ok  $msg" -ForegroundColor Green }
function Write-Info([string]$msg) { Write-Host "  ->  $msg" -ForegroundColor Gray }
function Write-Warn([string]$msg) { Write-Host "  warn $msg" -ForegroundColor Yellow }
function Write-Err([string]$msg)  { Write-Host "  error $msg" -ForegroundColor Red }

function Require-Command([string]$cmd) {
    return $null -ne (Get-Command $cmd -ErrorAction SilentlyContinue)
}

# ---------------------------------------------------------------------------
# Banner / help
# ---------------------------------------------------------------------------

Write-Host @"

  _   _ _                 _
 | | | (_)_ __ ___   __ _| | __ _ _   _  __ _
 | |_| | | '_ ' _ \ / _' | |/ _' | | | |/ _' |
 |  _  | | | | | | | (_| | | (_| | |_| | (_| |
 |_| |_|_|_| |_| |_|\__,_|_|\__,_|\__, |\__,_|
                                   |___/

"@ -ForegroundColor Red
Write-Host "  Himalaya Code — Windows installer" -ForegroundColor DarkGray
Write-Host ""

if ($Help) {
    Write-Host @"
Usage: .\install.ps1 [options]

  -Profile <release|debug>   Build profile (default: release)
  -InstallDir <path>         Where to copy Himalaya.exe (default: %LOCALAPPDATA%\Himalaya)
  -NoVerify                  Skip post-install verification
  -Help                      Show this help

Requirements:
  Rust toolchain — https://rustup.rs
  (Git is optional but recommended)
"@
    exit 0
}

# ---------------------------------------------------------------------------
# Step 1: locate workspace
# ---------------------------------------------------------------------------

Write-Step "Locating Rust workspace"

$ScriptDir  = Split-Path -Parent $MyInvocation.MyCommand.Path
$RustDir    = Join-Path $ScriptDir "rust"

if (-not (Test-Path (Join-Path $RustDir "Cargo.toml"))) {
    Write-Err "Cannot find rust\Cargo.toml next to install.ps1"
    Write-Err "Expected: $RustDir\Cargo.toml"
    exit 1
}
Write-Ok "workspace at $RustDir"

# ---------------------------------------------------------------------------
# Step 2: prerequisites
# ---------------------------------------------------------------------------

Write-Step "Checking prerequisites"

$missing = $false

if (Require-Command 'cargo') {
    $cv = & cargo --version 2>&1
    Write-Ok "cargo: $cv"
} else {
    Write-Err "cargo not found. Install Rust from https://rustup.rs"
    $missing = $true
}

if (Require-Command 'rustc') {
    $rv = & rustc --version 2>&1
    Write-Ok "rustc: $rv"
} else {
    Write-Err "rustc not found."
    $missing = $true
}

if (Require-Command 'git') {
    $gv = & git --version 2>&1
    Write-Ok "git: $gv"
} else {
    Write-Warn "git not found — build metadata (SHA) will show 'unknown'"
}

if ($missing) {
    Write-Err "Missing required tools. Aborting."
    exit 1
}

# ---------------------------------------------------------------------------
# Step 3: build
# ---------------------------------------------------------------------------

Write-Step "Building Himalaya ($Profile profile)"
Write-Info "This may take several minutes on the first run."
Write-Info "Cargo build jobs: $BuildJobs"

Push-Location $RustDir
try {
    $cargoArgs = @('build', '-p', 'rusty-Himalaya-cli')
    if ($Profile -eq 'release') { $cargoArgs += '--release' }

    Write-Info "cargo $($cargoArgs -join ' ')"
    $env:CARGO_BUILD_JOBS = $BuildJobs
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Err "cargo build failed (exit $LASTEXITCODE)"
        exit 1
    }
} finally {
    Pop-Location
}

$BinSrc = Join-Path $RustDir "target\$Profile\Himalaya.exe"
if (-not (Test-Path $BinSrc)) {
    Write-Err "Expected binary not found: $BinSrc"
    exit 1
}
Write-Ok "built $BinSrc"

# ---------------------------------------------------------------------------
# Step 4: install
# ---------------------------------------------------------------------------

Write-Step "Installing to $InstallDir"

if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir | Out-Null
}

$BinDst = Join-Path $InstallDir "Himalaya.exe"
Copy-Item -Path $BinSrc -Destination $BinDst -Force
Write-Ok "copied Himalaya.exe -> $BinDst"

# Add to user PATH if not already present
$userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
if ($userPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable('PATH', "$userPath;$InstallDir", 'User')
    Write-Ok "added $InstallDir to user PATH"
    Write-Warn "Restart your terminal (or run: `$env:PATH += ';$InstallDir'`) to use 'Himalaya' immediately."
} else {
    Write-Ok "$InstallDir already in PATH"
}

# ---------------------------------------------------------------------------
# Step 5: verify
# ---------------------------------------------------------------------------

Write-Step "Verifying installation"

if ($NoVerify) {
    Write-Warn "Skipped (-NoVerify)"
} else {
    $ver = & $BinDst --version 2>&1
    if ($LASTEXITCODE -eq 0) {
        Write-Ok "Himalaya --version -> $ver"
    } else {
        Write-Err "Himalaya --version failed: $ver"
        exit 1
    }
}

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

Write-Host ""
Write-Host "Himalaya Code is installed!" -ForegroundColor Green
Write-Host ""
Write-Host "  Binary:  $BinDst"
Write-Host ""
Write-Host "Quick start:"
Write-Host "  Himalaya                              # interactive REPL"
Write-Host "  Himalaya -p `"summarise this repo`"     # one-shot prompt"
Write-Host "  Himalaya --model openai/qwen3-vl --file paper.pdf -p `"总结`""
Write-Host ""
Write-Host "Authentication:"
Write-Host "  `$env:ANTHROPIC_API_KEY = 'sk-ant-...'"
Write-Host "  # or for Ollama:"
Write-Host "  `$env:OPENAI_BASE_URL   = 'http://127.0.0.1:11434/v1'"
Write-Host ""
