param(
    [ValidateSet('win-x64', 'win-arm64')][string]$Runtime = 'win-x64',
    [string]$Output = ''
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent
if (-not $Output) { $Output = Join-Path $Root "dist/SpeedTracker-$Runtime" }
$Output = [System.IO.Path]::GetFullPath($Output)

# A PC prepared by setup_windows_gnu.ps1 has no Visual Studio linker and builds x64 with Rust's
# LLVM-based target. Everywhere else, CI included, the Visual Studio C++ tools are used.
if (Test-Path (Join-Path $Root 'Windows/.cargo/config.toml')) {
    if ($Runtime -ne 'win-x64') { throw 'ARM64 needs the Visual Studio C++ tools; this PC is set up to build x64 only.' }
    $Target = 'x86_64-pc-windows-gnullvm'
} else {
    $Target = if ($Runtime -eq 'win-arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
    rustup target add $Target
    if ($LASTEXITCODE -ne 0) { throw "rustup could not add $Target" }
    # The C runtime is linked in, so the exe needs no Visual C++ redistributable on the user's PC.
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
}

# Cargo reads .cargo/config.toml from the folder it is run in.
Push-Location (Join-Path $Root 'Windows')
try {
    cargo build --release --locked --target $Target
    if ($LASTEXITCODE -ne 0) { throw 'Windows build failed' }
} finally {
    Pop-Location
}

# The whole app is this one file: tray, windows, optional collector and proxy.
New-Item -ItemType Directory -Force $Output | Out-Null
Copy-Item (Join-Path $Root "Windows/target/$Target/release/SpeedTracker.exe") $Output -Force
$Archive = "$Output.zip"
Compress-Archive -Path (Join-Path $Output '*') -DestinationPath $Archive -Force
Write-Output ("Built {0} ({1:N0} bytes)" -f $Archive, (Get-Item $Archive).Length)
