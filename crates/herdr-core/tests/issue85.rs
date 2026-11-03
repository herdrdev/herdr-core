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
