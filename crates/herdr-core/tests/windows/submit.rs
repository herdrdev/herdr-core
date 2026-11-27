use super::*;

// These templates still use sh; give the terminal Git's shell explicitly.
fn template_with_git(s: &Session, template: &str) -> String {
    let mut conn = cmd(&s.name, &s.root)
        .arg("connect")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let request = serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"terminal.run", "params":{
        "argv":[template], "env":{"PATH":format!("C:\\Program Files\\Git\\bin;{}", std::env::var("PATH").unwrap_or_default())}
    }});
    let mut input = conn.stdin.take().unwrap();
    std::io::Write::write_all(&mut input, format!("{request}\n").as_bytes()).unwrap();
    let mut reply = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(conn.stdout.take().unwrap()), &mut reply).unwrap();
    drop(input);
    let _ = conn.wait();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    reply["result"]["address"].as_str().unwrap_or_else(|| panic!("{reply}")).to_string()
}

#[test]
fn a_template_submits_queued_text_with_an_ordered_key_sequence() {
    let s = Session::new();
    let probe = s.file(
        "queued-input.ps1",
        r#"[Console]::WriteLine('READER_READY')
while (!(Test-Path (Join-Path $PSScriptRoot 'release'))) { Start-Sleep -Milliseconds 10 }
$keys = @()
do {
    $key = [Console]::ReadKey($true)
    $keys += $key.Key.ToString()
} until ($key.Key -eq 'Enter')
Start-Sleep -Milliseconds 100
[IO.File]::WriteAllText((Join-Path $PSScriptRoot 'keys.json'), (ConvertTo-Json @{keys=$keys; extra=[Console]::KeyAvailable}))
"#,
    );
    let templates = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("queued.md"),
        format!(
            "---\nsubmit: right enter\npaste_gap_ms: 0\ncommand: powershell.exe -NoProfile -File \"{probe}\"\n---\n"
        ),
    )
    .unwrap();
    let t = template_with_git(&s, "queued");
    s.shows(&t, "READER_READY");
    let (code, _) = s.out(&["send", &t, "abc"]);
    assert_eq!(code, 0);
    s.file("release", "");
    let result = s.root.join("keys.json");
    wait_until("queued input consumed", || s.json(&["get", &t])["state"] == "finished");
    let keys: Value = serde_json::from_slice(&std::fs::read(result).unwrap()).unwrap();
    assert_eq!(keys["keys"], serde_json::json!(["A", "B", "C", "RightArrow", "Enter"]));
    assert_eq!(keys["extra"], false, "submission must not be retried");
}

#[test]
fn child_notices_use_the_parent_templates_submit_keys() {
    let s = Session::new();
    let probe = s.file(
        "notice-input.ps1",
        r#"$e = [char]27
[Console]::Out.Write("$e]7501;state=working$e\")
[Console]::Out.Flush()
$text = ''
$keys = @()
do {
    $key = [Console]::ReadKey($true)
    if ($key.KeyChar -ge ' ') { $text += $key.KeyChar } else { $keys += $key.Key.ToString() }
} until ($key.Key -eq 'Enter')
Start-Sleep -Milliseconds 100
[IO.File]::WriteAllText((Join-Path $PSScriptRoot 'keys.json'), (ConvertTo-Json @{text=$text; keys=$keys; extra=[Console]::KeyAvailable}))
"#,
    );
    let templates = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("notice.md"),
        format!(
            "---\nsubmit: right enter\npaste_gap_ms: 0\ncommand: powershell.exe -NoProfile -File \"{probe}\"\n---\n"
        ),
    )
    .unwrap();
    let parent = template_with_git(&s, "notice");
    wait_until("parent working", || s.json(&["get", &parent])["status"]["state"] == "working");
    let out =
        cmd(&s.name, &s.root).args(["run", "--bg", "--parent", &parent, "cmd", "/C", "exit", "0"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let child: Value = serde_json::from_slice(&out.stdout).unwrap();
    wait_until("notice consumed", || s.json(&["get", &parent])["state"] == "finished");
    let result: Value = serde_json::from_slice(&std::fs::read(s.root.join("keys.json")).unwrap()).unwrap();
    let child = child["address"].as_str().unwrap();
    assert_eq!(result["text"], format!("<hc>{child} exited (exit 0). Read: hc read {child}</hc>"));
    assert_eq!(result["keys"], serde_json::json!(["RightArrow", "Enter"]));
    assert_eq!(result["extra"], false, "submission must not be retried");
}

#[test]
fn template_escape_and_character_are_separate_keys() {
    let s = Session::new();
    let probe = s.file(
        "escape.ps1",
        r#"[Console]::WriteLine('READER_READY')
$keys = @()
do {
    $key = [Console]::ReadKey($true)
    $keys += "$($key.Key):$($key.Modifiers)"
} until ($key.Key -eq 'Enter')
[IO.File]::WriteAllText((Join-Path $PSScriptRoot 'keys.json'), (ConvertTo-Json $keys))
"#,
    );
    let templates = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("escape.md"),
        format!("---\nsubmit: esc x enter\ncommand: powershell.exe -NoProfile -File \"{probe}\"\n---\n"),
    )
    .unwrap();
    let t = template_with_git(&s, "escape");
    let t = t.as_str();
    s.shows(t, "READER_READY");
    assert_eq!(s.out(&["send", t, "abc"]).0, 0);
    wait_until("escape input consumed", || s.json(&["get", t])["state"] == "finished");
    let keys: Value = serde_json::from_slice(&std::fs::read(s.root.join("keys.json")).unwrap()).unwrap();
    assert_eq!(keys, serde_json::json!(["A:0", "B:0", "C:0", "Escape:0", "X:0", "Enter:0"]));
}

#[test]
fn invalid_template_submission_types_no_partial_text() {
    let s = Session::new();
    let probe = s.file(
        "invalid-input.ps1",
        r#"[Console]::WriteLine('READER_READY')
while (!(Test-Path (Join-Path $PSScriptRoot 'release'))) { Start-Sleep -Milliseconds 10 }
$keys = @()
do {
    $key = [Console]::ReadKey($true)
    $keys += $key.Key.ToString()
} until ($key.Key -eq 'Enter')
Start-Sleep -Milliseconds 100
[IO.File]::WriteAllText((Join-Path $PSScriptRoot 'keys.json'), (ConvertTo-Json @{keys=$keys; extra=[Console]::KeyAvailable}))
"#,
    );
    let templates = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&templates).unwrap();
    std::fs::write(
        templates.join("invalid.md"),
        format!(
            "---\nsubmit: right unknown-key enter\npaste_gap_ms: 0\ncommand: powershell.exe -NoProfile -File \"{probe}\"\n---\n"
        ),
    )
    .unwrap();
    let t = template_with_git(&s, "invalid");
    s.shows(&t, "READER_READY");
    let rejected = cmd(&s.name, &s.root).args(["send", &t, "abc"]).output().unwrap();
    assert!(!rejected.status.success(), "invalid submit unexpectedly succeeded: {rejected:?}");
    let error: Value = serde_json::from_slice(&rejected.stderr)
        .unwrap_or_else(|e| panic!("rejected send to {t}: {e}; output: {rejected:?}"));
    assert_eq!(error["error"]["kind"], "invalid_params", "{error}");
    assert_eq!(s.out(&["keys", &t, "enter"]).0, 0);
    s.file("release", "");
    wait_until("rejected input checked", || s.json(&["get", &t])["state"] == "finished");
    let result: Value = serde_json::from_slice(&std::fs::read(s.root.join("keys.json")).unwrap()).unwrap();
    assert_eq!(result["keys"], serde_json::json!(["Enter"]), "rejected send typed partial text or keys");
    assert_eq!(result["extra"], false, "rejected send left queued input");
}
