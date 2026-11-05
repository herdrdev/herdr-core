#[test]
fn a_saved_absence_removes_an_inherited_manager_value() {
    let line = if cfg!(windows) {
        "if defined HC_RESTART_ABSENT (echo leaked) else (echo %HC_RESTART_VALUE%)"
    } else {
        "if [ \"${HC_RESTART_ABSENT+x}\" ]; then echo leaked; else echo \"$HC_RESTART_VALUE\"; fi"
    };
    let argv = super::shell_line(line);
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).env("HC_RESTART_ABSENT", "manager").env("HC_RESTART_VALUE", "caller");
    super::replace_env(
        &mut cmd,
        [("HC_RESTART_VALUE".into(), Some("saved".into())), ("HC_RESTART_ABSENT".into(), None)],
    );
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "saved");
}
