//! Exercise the Windows installer's warning without writing the user's PATH.
#![cfg(windows)]

#[test]
fn the_installer_warns_using_the_supplied_fresh_terminal_path() {
    let root = std::env::temp_dir().join(format!("hc-install-path-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let script = r#"
$ErrorActionPreference = 'Stop'
$ast = [Management.Automation.Language.Parser]::ParseFile($env:INSTALL_SCRIPT, [ref]$null, [ref]$null)
foreach ($name in 'File-Sha256', 'Warn-PathShadowing') {
    $function = $ast.Find({ param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name }, $true)
    if (-not $function) { throw "missing $name function" }
    Invoke-Expression $function.Extent.Text
}
$dir = Join-Path $env:TEST_ROOT 'install bin'
$other = Join-Path $env:TEST_ROOT 'other bin'
New-Item -ItemType Directory -Force $dir, $other | Out-Null
# Windows runners can supply TEMP through an 8.3 path. Exercise that spelling when available.
$fs = New-Object -ComObject Scripting.FileSystemObject
$dir = $fs.GetFolder($dir).ShortPath
$other = $fs.GetFolder($other).ShortPath
foreach ($name in 'hc', 'herdr-core') {
    Copy-Item "$env:SystemRoot\System32\whoami.exe" (Join-Path $dir "$name.exe")
    Copy-Item "$env:SystemRoot\System32\whoami.exe" (Join-Path $other "$name.exe")
}
# PowerShell also resolves scripts on PATH before reaching the installed exe.
Remove-Item (Join-Path $other 'hc.exe')
Set-Content (Join-Path $other 'hc.ps1') 'exit 99'
$original = $env:Path
# A stale process PATH must not affect the supplied fresh-terminal PATH.
$env:Path = $other
try {
    foreach ($shadowed in $false, $true) {
        $path = if ($shadowed) { "$other;$dir" } else { "$dir;$other" }
        $text = (Warn-PathShadowing $path $dir 6>&1 | Out-String -Width 4096)
        foreach ($name in 'hc', 'herdr-core') {
            $extension = if ($name -eq 'hc') { 'ps1' } else { 'exe' }
            $expected = "another $name at $(Join-Path $other "$name.$extension")"
            if ($text.Contains($expected) -ne $shadowed) { throw "wrong warning: $text" }
        }
        if ($text.Contains('comes first on your PATH') -ne $shadowed) { throw "wrong warning: $text" }
        if ($env:Path -ne $other) { throw 'process PATH was not restored' }
    }
    # A preserved, unrelated alias in the install directory also needs a warning.
    Copy-Item -Force "$env:SystemRoot\System32\cmd.exe" (Join-Path $dir 'hc.exe')
    $text = (Warn-PathShadowing $dir $dir 6>&1 | Out-String -Width 4096)
    if (-not $text.Contains("another hc at $(Join-Path $dir 'hc.exe')")) { throw "missing local conflict: $text" }
    if ($text.Contains('another herdr-core')) { throw "wrong local conflict: $text" }
} finally { $env:Path = $original }
"#;
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .env("INSTALL_SCRIPT", concat!(env!("CARGO_MANIFEST_DIR"), "/../../distribution/install.ps1"))
        .env("TEST_ROOT", &root)
        .output()
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert!(out.status.success(), "{out:?}");
}
