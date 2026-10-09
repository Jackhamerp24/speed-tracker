# Prepares this PC to build the Rust Windows app without Visual Studio.
#
# CI and anyone with the Visual Studio C++ Build Tools need none of this: plain `cargo build` works there.
# Without them, Rust's LLVM-based target (x86_64-pc-windows-gnullvm) can still link the app, given
# three things the minimal toolchain leaves out. This script supplies them, per user, and writes
# Windows/.cargo/config.toml (not committed) so that `cargo build` picks them up:
#   1. an import-library tool: LLVM's archiver, which acts as dlltool when it has that name;
#   2. import libraries for system DLLs, borrowed from the GNU toolchain's bundled set;
#   3. one for shlwapi.dll, which that set lacks.
# The plain GNU target (x86_64-pc-windows-gnu) links too, but its exe crashes before main.
$ErrorActionPreference = 'Stop'
$Root = Split-Path $PSScriptRoot -Parent
if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) { throw 'Install Rust first: winget install Rustlang.Rustup' }

rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal
rustup default stable-x86_64-pc-windows-gnu
rustup component add llvm-tools rust-mingw-x86_64-pc-windows-gnullvm
rustup target add x86_64-pc-windows-gnullvm
if ($LASTEXITCODE -ne 0) { throw 'rustup could not install the toolchain pieces' }

$Sysroot = (rustc --print sysroot).Trim()
$Gnu = Join-Path $Sysroot 'lib/rustlib/x86_64-pc-windows-gnu'
$Extras = Join-Path $env:USERPROFILE '.cargo/gnu-extras'
New-Item -ItemType Directory -Force $Extras | Out-Null
Copy-Item (Join-Path $Gnu 'bin/llvm-ar.exe') (Join-Path $Extras 'dlltool.exe') -Force
foreach ($Name in 'advapi32', 'bcrypt', 'comctl32', 'comdlg32', 'credui', 'crypt32', 'gdi32', 'imagehlp', 'iphlpapi', 'msimg32', 'ole32', 'oleaut32', 'opengl32', 'psapi', 'rpcrt4', 'secur32', 'setupapi', 'shell32', 'synchronization', 'uuid', 'winhttp', 'winmm', 'winspool', 'wsock32') {
    Copy-Item (Join-Path $Gnu "lib/self-contained/lib$Name.a") $Extras -Force
}
$Definition = Join-Path $Extras 'shlwapi.def'
Set-Content -Path $Definition -Encoding ascii -Value "LIBRARY shlwapi.dll`nEXPORTS`nAssocQueryStringW"
& (Join-Path $Extras 'dlltool.exe') -d $Definition -l (Join-Path $Extras 'libshlwapi.a') -m i386:x86-64
if ($LASTEXITCODE -ne 0) { throw 'The shlwapi import library could not be created' }

$Path = $Extras -replace '\\', '/'
$Config = Join-Path $Root 'Windows/.cargo/config.toml'
New-Item -ItemType Directory -Force (Split-Path $Config) | Out-Null
Set-Content -Path $Config -Encoding ascii -Value @"
# Local build settings for building without Visual Studio. Written by Scripts/setup_windows_gnu.ps1; not committed.
[build]
target = "x86_64-pc-windows-gnullvm"

[target.x86_64-pc-windows-gnullvm]
linker = "rust-lld"
rustflags = ["-C", "linker-flavor=ld.lld", "-C", "link-self-contained=yes", "-C", "target-feature=+crt-static", "-C", "dlltool=$Path/dlltool.exe", "-L", "native=$Path"]
"@
Write-Output "Ready. Build with: cargo build --release --manifest-path Windows/Cargo.toml"
Write-Output "The exe is written to Windows/target/x86_64-pc-windows-gnullvm/release/SpeedTracker.exe"
