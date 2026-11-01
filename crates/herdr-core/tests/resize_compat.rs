#![cfg(unix)]

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
#[ignore = "set HC_OLD_CORE to the released 0.1.16 herdr-core binary"]
fn capable_stream_closes_if_the_remote_is_downgraded_on_reconnect() {
    let mut far = Session::new();
    let t = run(&far, &["sleep", "100"]);
    let mut near = Session::new();
    near.env.push(fake_ssh(&near, &[("far", &far)]));
    near.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    near.json(&["machine", "add", "far"]);
    let mut conn = Framed::open(&near);
    let reply = conn.call(
        "terminal.attach",
        json!({
            "terminal": format!("far/{t}"), "cols": 80, "rows": 24,
            "observe": true, "resize_notifications": true
        }),
    );
    assert_eq!(reply["result"]["resize_notifications"], true);
    let channel = reply["result"]["channel"].as_u64().unwrap() as u32;
    while conn.read(Duration::from_millis(100)).is_some() {}
    std::fs::write(near.root.join("down"), "").unwrap();
    far.bin = std::env::var("HC_OLD_CORE").expect("HC_OLD_CORE").into();
    far.json(&["server", "restart"]);
    wait_until("disconnected", || near.json(&["machine", "ls"])["machines"][0]["state"] == "unreachable");
    // Drain the line-loss notice before reconnecting: no stream data is valid after this point.
    while conn.read(Duration::from_millis(100)).is_some() {}
    let hello = far.connect().call(1, "hello", json!({}));
    assert!(!hello["result"]["features"].as_array().unwrap().contains(&json!("resize_notifications")));
    let _ = fake_ssh(&near, &[("far", &far)]);
    std::fs::remove_file(near.root.join("down")).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((ch, kind, _)) = conn.read(Duration::from_millis(100))
            && ch == channel
        {
            assert_ne!(kind, wire::DATA, "downgraded path must not deliver fallback paint before CLOSE");
            if kind == wire::CLOSE {
                break;
            }
        }
        assert!(Instant::now() < until, "downgraded path must close its opted-in stream");
    }
    let mut leader = Viewer::attach(&far, &t, 80, 24, json!({}));
    fallback(&near, &format!("far/{t}"), &mut leader);
}

fn fallback(s: &Session, terminal: &str, leader: &mut Viewer) {
    let mut conn = Framed::open(s);
    let reply = conn.call(
        "terminal.attach",
        json!({
            "terminal": terminal, "cols": 40, "rows": 10,
            "observe": true, "resize_notifications": true
        }),
    );
    assert_eq!(reply["result"]["resize_notifications"], false, "{reply}");
    let channel = reply["result"]["channel"].as_u64().unwrap() as u32;
    // Drain the first paint before changing an otherwise idle terminal's grid.
    while conn.read(Duration::from_millis(100)).is_some() {}
    let cols = if get(s, terminal)["cols"] == 90 { 100 } else { 90 };
    leader.conn.send(leader.channel, wire::RESIZE, &[cols, 0, 30, 0]);
    wait_until("leader resized", || get(s, terminal)["cols"] == cols);
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        if let Some((ch, kind, _)) = conn.read(Duration::from_millis(50)) {
            assert!(!(ch == channel && matches!(kind, wire::DATA | wire::RESIZE)), "fallback must not inject paints");
        }
    }
}

#[test]
#[ignore = "set HC_OLD_CORE to the released 0.1.16 herdr-core binary"]
fn mixed_version_observers_fall_back_without_replacement_paints() {
    let old = std::env::var("HC_OLD_CORE").expect("HC_OLD_CORE");
    // New local -> old owner.
    let mut far = Session::new();
    far.bin = old.clone().into();
    let t = run(&far, &["sleep", "100"]);
    let mut leader = Viewer::attach(&far, &t, 80, 24, json!({}));
    let mut near = Session::new();
    near.env.push(fake_ssh(&near, &[("far", &far)]));
    near.json(&["machine", "add", "far"]);
    fallback(&near, &format!("far/{t}"), &mut leader);

    // New local -> new relay -> old owner: fallback propagates end-to-end.
    let mut front = Session::new();
    front.env.push(fake_ssh(&front, &[("near", &near)]));
    front.json(&["machine", "add", "near"]);
    fallback(&front, &format!("near/far/{t}"), &mut leader);

    // New local -> old relay -> new owner: don't let the old relay lose RESIZE
    // while forwarding replacement paints from the new owner.
    let owner = Session::new();
    let t = run(&owner, &["sleep", "100"]);
    let mut leader = Viewer::attach(&owner, &t, 80, 24, json!({}));
    let mut relay = Session::new();
    relay.bin = old.into();
    relay.env.push(fake_ssh(&relay, &[("owner", &owner)]));
    relay.json(&["machine", "add", "owner"]);
    fallback(&relay, &format!("owner/{t}"), &mut leader);
    let mut client = Session::new();
    client.env.push(fake_ssh(&client, &[("relay", &relay)]));
    client.json(&["machine", "add", "relay"]);
    fallback(&client, &format!("relay/owner/{t}"), &mut leader);
}
