use super::*;
use clap::CommandFactory;

fn parsed(args: &[&str]) -> Cmd {
    Cli::try_parse_from([&["hc"], args].concat()).unwrap().cmd.unwrap()
}

#[test]
fn the_picker_orders_by_last_change_by_what_needs_you_and_by_name() {
    let t = |address: &str, name: &str, state: Option<&str>, ms: u64| {
        let status = state.map(|s| serde_json::json!({"state": s}));
        serde_json::json!({"address": address, "name": name, "state": "live", "created_ms": 1,
            "status": status, "status_ms": state.map(|_| ms)})
    };
    let all = [
        t("t1", "web", Some("working"), 30),
        t("t2", "api", Some("done"), 20),
        t("t3", "shell", None, 0),
        t("t4", "Docs", Some("blocked"), 10),
        t("t5", "cli", Some("idle"), 40),
    ];
    let orders: Vec<(&str, String)> = orders(&all)
        .into_iter()
        .map(|(name, ts)| (name, ts.iter().map(|t| t["address"].as_str().unwrap()).collect::<Vec<_>>().join(" ")))
        .collect();
    assert_eq!(
        orders,
        [("recent", "t5 t1 t2 t4 t3".into()), ("status", "t4 t1 t5 t2 t3".into()), ("name", "t2 t5 t4 t3 t1".into()),]
    );
}

#[test]
fn picker_rows_narrower_than_the_headings_list_without_a_header() {
    let t = serde_json::json!({"address": "t1", "argv": ["pi"], "cwd": "/a"});
    assert_eq!(items(&[t], false).len(), 1);
}

#[test]
fn rows_say_where_a_terminal_lives_else_its_folder_and_no_other_labels() {
    let t = |extra: Value| {
        let mut t = serde_json::json!({"address": "t1", "argv": ["pi"], "cwd": "/a/bliss", "state": "live"});
        t.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let item = items(&[t], false).remove(0);
        (picker::plain(&item.rest), item.short)
    };
    let labels = serde_json::json!({"where": "herdr-core / review", "herdr.tab": "tab3", "ws": "x9"});
    let (rest, short) = t(serde_json::json!({"labels": labels, "name": "rev"}));
    assert_eq!(short, "rev \u{b7} herdr-core / review");
    assert!(
        rest.contains("herdr-core / review")
            && !rest.contains("bliss")
            && !rest.contains("x9")
            && !rest.contains("tab3"),
        "{rest}"
    );
    assert_eq!(t(serde_json::json!({"labels": {"ws": "x9"}})).1, "pi \u{b7} bliss");
}

#[test]
fn human_folders_follow_the_foreground_but_json_keeps_the_launch_folder() {
    for foreground in [json!({"cwd": "/current/work"}), json!({"cwd": null}), Value::Null] {
        let t = json!({"address": "t1", "argv": ["bash"], "cwd": "/launch/start", "foreground": foreground});
        let original = t.clone();
        let cwd = if foreground["cwd"].is_string() { "/current/work" } else { "/launch/start" };
        assert_eq!(folder(&t), cwd.rsplit('/').next().unwrap());
        assert!(picker::plain(&items(std::slice::from_ref(&t), false)[0].rest).contains(&folder(&t)));
        assert!(describe(&t).contains(&format!("  cwd: {cwd}\n")));
        assert_eq!(short(&t), json!({"address": "t1", "cwd": "/launch/start", "agent": "bash"}));
        assert_eq!(t, original, "formatting must not change the full get record");
    }
}

#[test]
fn get_keeps_an_unsent_first_message_visible_to_people() {
    let mut t = json!({"address": "t1", "argv": ["pi"], "state": "live"});
    assert!(!describe(&t).contains("first message"));
    t["first_message"] = json!({"sent": false, "reason": "output did not settle"});
    assert!(describe(&t).contains("first message NOT sent: output did not settle"));
}

#[test]
fn every_api_method_has_a_command_or_is_api_only() {
    Cli::command().debug_assert();
    for method in crate::api::METHOD_NAMES {
        let covered = COMMANDS.iter().any(|(_, m)| m == method) || API_ONLY.contains(method);
        assert!(covered, "{method} has no command and is not marked API-only");
    }
    for (command, _) in COMMANDS {
        let mut cmd = Cli::command();
        for word in command.split(' ') {
            cmd = cmd.find_subcommand(word).unwrap_or_else(|| panic!("no command `{command}`")).clone();
        }
    }
}

#[test]
fn a_command_line_becomes_the_api_request() {
    let Cmd::Send(p) = parsed(&["send", "t3", "hi there", "--if", "idle", "--wait", "--timeout", "2.5"]) else {
        panic!()
    };
    let want = json!({"terminal": "t3", "text": "hi there", "if": "idle", "force": false, "wait": true, "timeout": 2.5, "caller": null});
    assert_eq!(to(&p), want);
    assert!(serde_json::from_value::<SendParams>(want).is_ok(), "and the API reads it back");

    let Cmd::Wait(p) = parsed(&["wait", "t3", "--output", "ready"]) else { panic!() };
    assert_eq!(to(&p)["output"], "ready");

    let Cmd::Run(p) = parsed(&["run", "--label", "a=1", "--label", "b=x=y", "--label", "a=2", "sh", "-c", "true"])
    else {
        panic!()
    };
    assert_eq!(to(&p)["labels"], json!({"a": "2", "b": "x=y"}), "an object; values may hold =; the last repeat wins");
    assert_eq!(to(&p)["argv"], json!(["sh", "-c", "true"]));
}

#[test]
fn a_wait_prints_what_happened() {
    let one = |reason: &str| json!({"terminal": "t1", "ended": reason != "timeout", "reason": reason, "status": "working", "turn": 2, "exit_code": 42, "children": []});
    assert!(describe_wait(&one("timeout")).starts_with("t1 still running: working"));
    assert!(describe_wait(&one("exit")).starts_with("t1 exited (exit 42"));
    let all = json!({"terminal": "t0", "reason": "children", "children": [one("exit")]});
    assert!(describe_wait(&all).starts_with("t1 exited"));
    let none = json!({"terminal": "t0", "reason": "children", "children": []});
    assert_eq!(describe_wait(&none), "no children are working");
    let message = json!({"terminal": "t0", "reason": "message", "from": "t2", "children": []});
    assert!(describe_wait(&message).contains("t2"), "{}", describe_wait(&message));
    let mut interrupted = one("message");
    interrupted["ended"] = false.into();
    interrupted["from"] = "t1".into();
    assert_eq!(
        describe_wait(&interrupted),
        "a message from t1 (already in your input)\nt1 still running: working, turn 2 (wait again to keep waiting)"
    );
}

#[test]
fn older_compares_versions_as_numbers() {
    assert!(older("hc 0.1.9", "hc 0.2.0") && older("hc 0.2.0", "hc 0.10.0"));
    assert!(!older("hc 0.2.0", "hc 0.2.0") && !older("hc 0.10.0", "hc 0.9.9") && !older("hc dev", "hc 0.2.0"));
}

#[test]
fn labels_need_a_key() {
    assert!(Cli::try_parse_from(["hc", "label", "t1", "nokey"]).is_err());
    assert!(Cli::try_parse_from(["hc", "label", "t1", "=v"]).is_err());
    let Cmd::Label(p) = parsed(&["label", "t1", "gone="]) else { panic!() };
    assert_eq!(to(&p)["labels"], json!({"gone": ""}), "an empty value removes it");
}
