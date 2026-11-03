#![cfg(unix)]
mod common;
use common::*;
use std::time::Instant;

#[test]
fn a_reporting_template_retries_until_its_output_settles() {
    let mut s = Session::new();
    s.env = vec![("HC_QUIET_MS".into(), "0".into()), ("HC_STARTED_MS".into(), "500".into())];
    let dir = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ticker.md"), r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; for i in $(seq 1 65); do echo tick; sleep 0.1; done; while read -r l; do printf "\033]7501;state=working\033\\"; echo "got:$l"; printf "\033]7501;state=done\033\\"; done'
---
"#).unwrap();
    let start = Instant::now();
    let r = s.json(&["run", "--bg", "ticker", "task85"]);
    let t = r["address"].as_str().unwrap();
    eprintln!("run after {:?}: {r}", start.elapsed());
    eprintln!("get: {}", s.json(&["get", t]));
    eprintln!("screen: {}", text(&s, t));
    assert_eq!(r["first_message"]["sent"], true, "{r}");
    wait_until("task delivered", || text(&s, t).contains("got:task85"));
}

#[test]
fn a_retried_first_message_stays_pending_until_the_delivered_task_finishes() {
    let mut s = Session::new();
    s.env = vec![("HC_QUIET_MS".into(), "0".into()), ("HC_STARTED_MS".into(), "100".into())];
    let dir = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&dir).unwrap();
    let finish = s.root.join("finish");
    std::fs::write(
        dir.join("ticker.md"),
        format!(
            r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=done\033\\"; for i in $(seq 1 65); do echo tick; sleep 0.1; done; read -r task; echo "got:$task"; while [ ! -f "{}" ]; do sleep 0.01; done; printf "\033]7501;state=done\033\\"; printf "\033]7501;state=idle\033\\"; sleep 100'
---
"#,
            finish.display()
        ),
    )
    .unwrap();
    let parent = s.json(&["run", "--bg", "sleep", "100"])["address"].as_str().unwrap().to_string();
    let start = Instant::now();
    let r = s.json(&["run", "--bg", "--parent", &parent, "ticker", "retried-task"]);
    assert_eq!(r["first_message"]["sent"], true, "{r}");
    assert!(start.elapsed().as_secs() >= 5, "must outlast the first settle attempt");
    let child = r["address"].as_str().unwrap();
    wait_until("retried task delivered", || text(&s, child).contains("got:retried-task"));
    let wait = |mode: &str, timeout: &str| {
        let out = s
            .cmd()
            .env("HC_TERMINAL_ID", &parent)
            .args(["--json", "wait", "--children", mode, "--timeout", timeout])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    for mode in ["all", "any"] {
        let r = wait(mode, "0.1");
        assert_eq!(r["reason"], "timeout", "{mode}: startup done is not completion: {r}");
        assert_eq!(r["children"][0]["ended"], false, "{r}");
    }
    std::fs::write(finish, "").unwrap();
    let r = wait("any", "5");
    assert_eq!(r["reason"], "children", "{r}");
    assert_eq!(r["children"][0]["ended"], true, "{r}");
    assert_eq!(wait("all", "0.1")["reason"], "children");
    assert_eq!(wait("any", "0.1")["reason"], "timeout", "completion is consumed once");
    assert_eq!(text(&s, child).matches("got:retried-task").count(), 1, "delivered once");
}

#[test]
fn an_unsent_template_message_stays_visible_in_get() {
    let mut s = Session::new();
    s.env.push(("HC_READY_MS".into(), "1200".into()));
    let dir = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("ticker.md"),
        r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; while :; do echo tick; sleep 0.1; done'
---
task85
"#,
    )
    .unwrap();
    // Enable the real settle guard for this session.
    s.env.retain(|(k, _)| k != "HC_SETTLE_MS");
    let r = s.json(&["run", "--bg", "ticker"]);
    let t = r["address"].as_str().unwrap();
    let got = s.json(&["get", t]);
    eprintln!("run: {r}\nget: {got}");
    assert_eq!(r["first_message"]["sent"], false, "{r}");
    assert_eq!(got["first_message"], r["first_message"], "{got}");
    assert!(!text(&s, t).contains("task85"));
    s.json(&["server", "restart"]);
    assert_eq!(s.json(&["get", t])["first_message"], r["first_message"]);
}

#[test]
fn a_continuously_ticking_template_uses_sends_existing_startup_age_limit() {
    let mut s = Session::new();
    s.env = vec![("HC_QUIET_MS".into(), "0".into()), ("HC_STARTED_MS".into(), "500".into())];
    let dir = s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("ticker.md"),
        r#"---
command: sh -c 'stty -echo; printf "\033]7501;state=idle\033\\"; (while :; do echo tick; sleep 0.1; done) & while read -r l; do printf "\033]7501;state=working\033\\"; echo "got:$l"; printf "\033]7501;state=done\033\\"; done'
---
"#,
    )
    .unwrap();
    let start = Instant::now();
    let r = s.json(&["run", "--bg", "ticker", "task85"]);
    eprintln!("continuous ticker after {:?}: {r}", start.elapsed());
    assert_eq!(r["first_message"]["sent"], true, "{r}");
    let t = r["address"].as_str().unwrap();
    wait_until("task delivered despite ticker", || text(&s, t).contains("got:task85"));
}
