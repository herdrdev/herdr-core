# Puts Microsoft's ConPTY beside herdr-core.exe: <Dest>\conpty\conpty.dll and
# <Dest>\conpty\<arch>\OpenConsole.exe. Windows' own ConPTY swallows the answers
# hc gives programs (7501, DA), so hc ships this one. Pinned and checked.
# conpty.dll starts the OpenConsole of the machine's architecture, not the
# process's: the x64 hc on an ARM64 machine needs arm64\, or it falls back to
# Windows' own.
param([Parameter(Mandatory)][string]$Dest)
$ErrorActionPreference = "Stop"
$version = "1.24.260710001"
$url = "https://github.com/microsoft/terminal/releases/download/v1.24.11911.0/Microsoft.Windows.Console.ConPTY.$version.nupkg"
$sha = @{
    nupkg = "9382ad7becb7e4d84e300578d8e4f4df28f43d979d9055d978c42913c47e0e9d"
    "runtimes/win-x64/native/conpty.dll" = "39fba2713e2495117b1591ae8c32a3b904bea7aa66069cf7815e2844c76d75d8"
    "build/native/runtimes/x64/OpenConsole.exe" = "b7fd936c2668b87b9ecf7b3366dc6568afc1c6f981874cba3e955a1c35cf8160"
    "build/native/runtimes/arm64/OpenConsole.exe" = "ed7622fd0d3bedc9ab9f122f5e58edf0def9e7999224f52dd395ba9f54edbe09"
}
function Check($path, $want) {
    # .NET, not Get-FileHash: that one fails when PowerShell 7 starts 5.1.
    $stream = [IO.File]::OpenRead($path)
    try { $got = -join ([Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString("x2") }) } finally { $stream.Close() }
    if ($got -ne $want) { throw "$path`: sha256 $got, expected $want" }
}
$work = Join-Path ([IO.Path]::GetTempPath()) "hc-conpty-$version"
$zip = "$work.zip"
if (-not (Test-Path $zip)) {
    Invoke-WebRequest -UseBasicParsing $url -OutFile "$zip.part"
    Move-Item -Force "$zip.part" $zip
}
Check $zip $sha.nupkg
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
Expand-Archive $zip $work
New-Item -ItemType Directory -Force "$Dest\conpty\x64", "$Dest\conpty\arm64" | Out-Null
foreach ($pair in @(
        ("runtimes/win-x64/native/conpty.dll", "conpty\conpty.dll"),
        ("build/native/runtimes/x64/OpenConsole.exe", "conpty\x64\OpenConsole.exe"),
        ("build/native/runtimes/arm64/OpenConsole.exe", "conpty\arm64\OpenConsole.exe"))) {
    Check (Join-Path $work $pair[0]) $sha[$pair[0]]
    Copy-Item -Force (Join-Path $work $pair[0]) (Join-Path $Dest $pair[1])
}
Remove-Item -Recurse -Force $work
"ConPTY $version in $Dest\conpty"
