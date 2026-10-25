# Builds this checkout and installs it with distribution/install.ps1, from a
# zip like a release's: PATH set, running terminals kept.
$ErrorActionPreference = "Stop"
Set-Location "$PSScriptRoot\.."
cargo build --release --locked
if ($LASTEXITCODE) { exit $LASTEXITCODE }
& "$PSScriptRoot\fetch-conpty.ps1" -Dest target\release
$tmp = Join-Path ([IO.Path]::GetTempPath()) "hc-local-$PID"
New-Item -ItemType Directory -Force $tmp | Out-Null
# The installer deletes the zip and itself when done: hand it copies.
Copy-Item distribution\install.ps1 $tmp
& "$env:SystemRoot\System32\tar.exe" -a -cf "$tmp\hc.zip" -C target\release herdr-core.exe conpty
if ($LASTEXITCODE) { exit $LASTEXITCODE }
powershell -NoProfile -ExecutionPolicy Bypass -File "$tmp\install.ps1" "$tmp\hc.zip"
$code = $LASTEXITCODE
Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
exit $code
