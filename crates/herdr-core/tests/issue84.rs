#![cfg(unix)]
mod common;
use common::*;

fn wait_text(s: &Session, t: &str, needle: &str) {
    wait_until(&format!("{needle:?} in {t}"), || text(s, t).contains(needle));
}

// Copies from a program land on the machine the person views from, not the one
// hosting it — so a terminal whose viewer is at another machine says so in its
// environment, and programs route their copies through the terminal (OSC 52)
// to the viewer's clipboard instead of this host's (#84).
#[test]
fn a_remote_viewers_terminal_tells_programs_the_viewer_is_remote() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let marks = "v=[$HERDR_REMOTE_VIEWER] h=[$HERDR_ENV] ssh=[$SSH_CONNECTION] client=[$SSH_CLIENT]";
    // The call for the host arrives over the machine's line.
    let there = run(&s, &["--on", "rohan", "sh", "-c", &format!("echo {marks}; sleep 100")]);
    wait_text(&s, &there, "v=[1] h=[1] ssh=[0.0.0.0 0 0.0.0.0 0] client=[0.0.0.0 0 0]");

    // A terminal here, viewed here, keeps to the host's own claims.
    let here = run(&s, &["sh", "-c", &format!("echo {marks}; sleep 100")]);
    wait_text(&s, &here, "v=[] h=[] ssh=[] client=[]");
}

#[test]
fn a_caller_on_another_machine_marks_the_terminal_too() {
    let s = Session::new();
    let marks = "v=[$HERDR_REMOTE_VIEWER] ssh=[$SSH_CONNECTION]";
    let out = s
        .cmd()
        .args(["run", "--bg", "--no-parent", "sh", "-c", &format!("echo {marks}; sleep 100")])
        .env("HC_TERMINAL_ID", "desktop/t7")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let t = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["address"].as_str().unwrap().to_string();
    wait_text(&s, &t, "v=[1] ssh=[0.0.0.0 0 0.0.0.0 0]");
}
