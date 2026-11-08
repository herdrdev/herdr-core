# Installs herdr-core (hc) on Windows, or updates it:
#
#   powershell -c "irm https://releases.herdr.dev/core/install.ps1 | iex"
#
# Into %LOCALAPPDATA%\Programs\herdr-core (herdr-core.exe, Microsoft's
# ConPTY beside it), added to your PATH, with hc.exe linked beside it unless
# that path already exists. An hc that is already there is swapped the planned way (hc
# update --from): running terminals keep running. No admin needed.
# HC_VERSION=0.2.0 picks a version; HERDR_RELEASES_URL points elsewhere
# (tests); HC_INSTALL_DIR installs elsewhere.
#
# `-File install.ps1 <release zip> [session]` installs from a zip already
# here, checked by whoever put it here, and removes both files after
# (`hc machine add` from another machine does that).

# Its own scope: under `irm | iex` it would otherwise set the caller's variables.
& {
$zip, $session = $args
if ($session) { $env:HC_SESSION = $session }
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# throw, not exit: under `irm | iex`, exit would close the person's window.
function Fail($why) { throw "hc: $why" }

function Warn-PathShadowing($path, $dir) {
    $saved = $env:Path
    try {
        $env:Path = [Environment]::ExpandEnvironmentVariables($path)
        foreach ($name in "hc", "herdr-core") {
            $found = Get-Command $name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($found -and $found.Path -ine [IO.Path]::GetFullPath((Join-Path $dir "$name.exe"))) {
                Write-Host "another $name at $($found.Path) comes first on your PATH; remove it or put $dir ahead of it."
            }
        }
    } finally { $env:Path = $saved }
}

$releases = if ($env:HERDR_RELEASES_URL) { $env:HERDR_RELEASES_URL.TrimEnd("/") } else { "https://releases.herdr.dev" }
$dir = if ($env:HC_INSTALL_DIR) { $env:HC_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\herdr-core" }
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne "AMD64") { Fail "no build for $arch Windows yet (x64 only)" }
$target = "windows-x86_64"
# Windows' own tar opens zips; Git Bash's, often first on PATH, can't.
$tar = Join-Path $env:SystemRoot "System32\tar.exe"
if (-not (Get-Command curl.exe -ErrorAction SilentlyContinue) -or -not (Test-Path $tar)) {
    Fail "needs curl.exe and tar.exe (Windows 10 1803 or newer)"
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) "hc-install-$PID"
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
  if ($zip) {
    Copy-Item $zip "$tmp\hc.zip"
  } else {
    $manifest = if ($env:HC_VERSION) { "$releases/core/$($env:HC_VERSION)/manifest.json" } else { "$releases/core/latest.json" }
    curl.exe -fsSL $manifest -o "$tmp\manifest.json"
    if ($LASTEXITCODE) { Fail "cannot reach $manifest" }
    $m = Get-Content -Raw "$tmp\manifest.json" | ConvertFrom-Json
    $t = $m.targets.$target
    if (-not $t.url -or -not $t.sha256) { Fail "no build for $target in $manifest" }

    # Tells the release server's download counter a new install from an
    # update; the file is the same either way. Only plain URLs on the release
    # server in use are marked: one with a query or fragment (a mirror's
    # signed URL) is fetched exactly as the manifest gives it.
    $src = if (Test-Path (Join-Path $dir "herdr-core.exe")) { "update" } else { "install" }
    $plain = $t.url.StartsWith("$releases/", [StringComparison]::Ordinal) -and $t.url.IndexOfAny([char[]]"?#") -lt 0
    $download = if ($plain) { "$($t.url)?src=$src" } else { $t.url }
    curl.exe -fsSL $download -o "$tmp\hc.zip"
    if ($LASTEXITCODE) { Fail "download failed: $($t.url)" }
    # .NET, not Get-FileHash: that one fails when PowerShell 7 starts 5.1.
    $stream = [IO.File]::OpenRead("$tmp\hc.zip")
    try { $got = -join ([Security.Cryptography.SHA256]::Create().ComputeHash($stream) | ForEach-Object { $_.ToString("x2") }) } finally { $stream.Close() }
    if ($got -ne $t.sha256) { Fail "the download is damaged (checksum mismatch); try again" }
  }
    New-Item -ItemType Directory -Force "$tmp\hc" | Out-Null
    & $tar -xf "$tmp\hc.zip" -C "$tmp\hc"
    $new = "$tmp\hc\herdr-core.exe"
    if ($LASTEXITCODE -or -not (Test-Path "$tmp\hc\conpty\conpty.dll")) { Fail "the download holds no complete hc" }
    $version = (& $new --version) -replace "^hc ", ""
    if ($LASTEXITCODE) { Fail "the downloaded hc does not run on this machine" }

    New-Item -ItemType Directory -Force $dir | Out-Null
    $exe = Join-Path $dir "herdr-core.exe"
    if (Test-Path $exe) {
        # Swapped the planned way, so its terminals keep running.
        & $exe update --from $new | Out-Null
        if ($LASTEXITCODE) { Fail "updating $exe failed (see above)" }
        Write-Host "hc updated to $version."
    } else {
        Copy-Item -Recurse -Force "$tmp\hc\herdr-core.exe", "$tmp\hc\conpty" $dir
        Write-Host "hc $version installed into $dir."
    }
    Copy-Item -Recurse -Force "$tmp\hc\THIRD-PARTY-NOTICES" $dir -ErrorAction SilentlyContinue
    if (-not (Test-Path (Join-Path $dir "hc.exe"))) {
        New-Item -ItemType HardLink -Path (Join-Path $dir "hc.exe") -Target $exe | Out-Null
    }

    # The raw user PATH, so %VARIABLES% in it stay unexpanded and keep their type.
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
    $raw = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $listed = $raw -split ";" | Where-Object { $_.Trim().TrimEnd("\") -ieq $dir.TrimEnd("\") }
    if (-not $listed) {
        $kind = if ($key.GetValueNames() -contains "Path") { $key.GetValueKind("Path") } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $key.SetValue("Path", $(if ($raw) { "$($raw.TrimEnd(';'));$dir" } else { $dir }), $kind)
        # Setting any user variable tells running programs the environment changed.
        [Environment]::SetEnvironmentVariable("HC_INSTALLING", "1", "User")
        [Environment]::SetEnvironmentVariable("HC_INSTALLING", $null, "User")
        Write-Host "Added $dir to your PATH: new terminals have hc."
    }
    $key.Close()
    # Windows builds a fresh terminal's PATH with machine entries before user entries.
    $freshPath = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User")
    Warn-PathShadowing $freshPath $dir
    if (($env:Path -split ";") -notcontains $dir) { $env:Path += ";$dir" }
    Write-Host "Agents: hc --skill. People: hc --help."
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    if ($zip) { Remove-Item -Force $zip, $PSCommandPath -ErrorAction SilentlyContinue }
}
} @args
