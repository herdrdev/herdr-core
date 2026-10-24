# Builds this checkout and installs it for the current Windows user.
function Install-HerdrCoreFiles($Build, $Destination) {
    $ErrorActionPreference = "Stop"
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $exe = Join-Path $Destination "herdr-core.exe"
    $installed = Test-Path -LiteralPath $exe
    if ($installed) {
        & $exe update --from (Join-Path $Build "herdr-core.exe")
        if ($LASTEXITCODE) { throw "Updating $exe failed (exit $LASTEXITCODE)." }
    } else {
        Copy-Item -LiteralPath (Join-Path $Build "herdr-core.exe"), (Join-Path $Build "conpty") -Destination $Destination -Recurse
    }
    $alias = Join-Path $Destination "hc.exe"
    if (-not (Test-Path -LiteralPath $alias)) {
        New-Item -ItemType HardLink -Path $alias -Target $exe | Out-Null
    }
    if (-not $installed) {
        & $exe server restart
        if ($LASTEXITCODE) { throw "Restarting $exe failed (exit $LASTEXITCODE)." }
    }
}

if ($MyInvocation.InvocationName -eq '.') { return }
$ErrorActionPreference = "Stop"
if ($env:OS -ne "Windows_NT") { throw "This installer requires Windows." }
Push-Location "$PSScriptRoot\.."
try {
    cargo build --release --locked --target-dir target
    if ($LASTEXITCODE) { throw "Core build failed (exit $LASTEXITCODE)." }
    & "$PSScriptRoot\fetch-conpty.ps1" -Dest "target\release"
    $dir = if ($env:HC_INSTALL_DIR) { $env:HC_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\herdr-core" }
    Install-HerdrCoreFiles "target\release" $dir
} finally {
    Pop-Location
}

# Keep literal %VARIABLES% and the existing registry value type in PATH.
$key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
try {
    $raw = [string]$key.GetValue("Path", "", [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $listed = $raw -split ";" | Where-Object { $_.Trim().TrimEnd("\") -ieq $dir.TrimEnd("\") }
    if (-not $listed) {
        $kind = if ($key.GetValueNames() -contains "Path") { $key.GetValueKind("Path") } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $key.SetValue("Path", $(if ($raw) { "$($raw.TrimEnd(';'));$dir" } else { $dir }), $kind)
        [Environment]::SetEnvironmentVariable("HC_INSTALLING", "1", "User")
        [Environment]::SetEnvironmentVariable("HC_INSTALLING", $null, "User")
    }
} finally {
    $key.Close()
}
if (($env:Path -split ";") -notcontains $dir) { $env:Path += ";$dir" }
