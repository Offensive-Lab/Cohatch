# Optional verified portable toolchain installed in this workspace.
# Dot-source this file. It changes only the current PowerShell environment.
$cohatchRoot = Split-Path -Parent $PSScriptRoot
$cohatchPortable = Join-Path $cohatchRoot '.tools\storage-rust'
if (-not (Test-Path -LiteralPath (Join-Path $cohatchPortable 'bin\cargo.exe'))) {
    throw 'Portable Rust is not installed. Use a standard Rust/MSVC installation; see README.'
}
$env:CARGO_HOME = Join-Path $cohatchRoot '.tools\cargo'
$cohatchLinkers = Join-Path $cohatchPortable 'lib\rustlib\x86_64-pc-windows-gnu\bin'
$env:PATH = "$(Join-Path $cohatchPortable 'bin');$cohatchLinkers;$(Join-Path $cohatchLinkers 'self-contained');$env:PATH"
$env:CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER = Join-Path $cohatchLinkers 'self-contained\x86_64-w64-mingw32-gcc.exe'
# Native Windows TLS requires no external C compilation. Use the GNU linker
# bundled in the official Rust distribution, with no machine-wide settings.
