#![cfg(unix)]

mod common;

use common::*;
use serde_json::json;

#[test]
fn a_subscription_can_ask_for_documents_by_name() {
    let s = Session::new();
    s.json(&["server", "status"]);
    let mut c = s.connect();
    // One subscriber names a document, the other asks for everything.
    let mut named = s.connect();
    named.call(1, "events.subscribe", json!({"docs": ["app"]}));
    let mut all = s.connect();
    all.call(1, "events.subscribe", json!({}));
    c.call(2, "doc.set", json!({"name": "other", "value": 1, "version": 0}));
    c.call(3, "doc.set", json!({"name": "app", "value": 2, "version": 0}));
    // The named subscriber skipped `other`: its first document event is `app`.
    let named_event = next_event(&mut named, |m| m["params"]["type"] == "doc");
    assert_eq!(named_event["params"]["data"]["name"], "app");
    // The unfiltered one hears both, in order.
    let first = next_event(&mut all, |m| m["params"]["type"] == "doc");
    let second = next_event(&mut all, |m| m["params"]["type"] == "doc");
    assert_eq!(first["params"]["data"]["name"], "other");
    assert_eq!(second["params"]["data"]["name"], "app");
}

#[test]
fn documents_are_versioned_persisted_and_evented() {
    let s = Session::new();
    s.json(&["server", "status"]);
    let mut c = s.connect();
    assert_eq!(c.call(1, "doc.get", json!({"name": "app"}))["result"], json!({"value": null, "version": 0}));
    let mut events = s.connect();
    events.call(1, "events.subscribe", json!({}));
    let value = json!({"terminal": "t1", "nested": [1, true, null]});
    assert_eq!(
        c.call(2, "doc.set", json!({"name": "app", "value": value, "version": 0}))["result"],
        json!({"version": 1})
    );
    let event = next_event(&mut events, |m| m["params"]["type"] == "doc");
    assert_eq!(event["params"]["data"], json!({"name": "app", "version": 1}));
    assert!(event["params"].get("terminal").is_none());
    let stale = c.call(3, "doc.set", json!({"name": "app", "value": "stale", "version": 0}));
    assert_eq!(stale["error"]["data"]["kind"], "conflict");
    assert!(stale["error"]["message"].as_str().unwrap().contains('1'));
    s.hc(&["server", "restart"]);
    s.json(&["server", "status"]);
    let mut c = s.connect();
    assert_eq!(c.call(4, "doc.get", json!({"name": "app"}))["result"], json!({"value": value, "version": 1}));
    assert_eq!(
        c.call(5, "doc.set", json!({"name": "app", "value": null, "version": 1}))["result"],
        json!({"version": 2})
    );
    assert_eq!(c.call(6, "doc.get", json!({"name": "app"}))["result"], json!({"value": null, "version": 2}));
    s.hc(&["server", "stop"]);
    s.json(&["server", "status"]);
    assert_eq!(s.connect().call(7, "doc.get", json!({"name": "app"}))["result"]["version"], 2);
}

#[test]
fn corrupt_documents_are_quarantined_without_stopping_the_core() {
    let s = Session::new();
    let state = s.root.join("state/herdr-core/test");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("docs.json"), b"{broken").unwrap();
    s.json(&["server", "status"]);
    assert_eq!(std::fs::read(state.join("docs.json.bad")).unwrap(), b"{broken");
    let mut c = s.connect();
    assert_eq!(c.call(1, "doc.get", json!({"name": "app"}))["result"], json!({"value": null, "version": 0}));
    assert_eq!(c.call(2, "doc.set", json!({"name": "app", "value": true, "version": 0}))["result"]["version"], 1);
}

#[test]
fn exactly_one_client_can_write_a_version() {
    let s = Session::new();
    s.json(&["server", "status"]);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let writers: Vec<_> = (0..2)
        .map(|value| {
            let mut c = s.connect();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                c.call(1, "doc.set", json!({"name": "app", "value": value, "version": 0}))
            })
        })
        .collect();
    let replies: Vec<_> = writers.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(replies.iter().filter(|r| r["result"]["version"] == 1).count(), 1);
    assert_eq!(replies.iter().filter(|r| r["error"]["data"]["kind"] == "conflict").count(), 1);
}

#[test]
fn terminal_filtered_subscribers_do_not_hear_document_changes() {
    let s = Session::new();
    let terminal = s.json(&["run", "--bg", "sleep", "30"])["address"].as_str().unwrap().to_string();
    let mut events = s.connect();
    events.call(1, "events.subscribe", json!({"terminal": terminal}));
    let mut c = s.connect();
    c.call(1, "doc.set", json!({"name": "app", "value": true, "version": 0}));
    c.call(2, "terminal.label", json!({"terminal": terminal, "labels": {"test": "yes"}}));
    let event = next_event(&mut events, |m| m["method"] == "event");
    assert_eq!(event["params"]["type"], "labels");
}

#[test]
fn documents_reject_invalid_names_and_oversized_values() {
    let s = Session::new();
    s.json(&["server", "status"]);
    let mut c = s.connect();
    for name in ["", "bad/name", "t1"] {
        assert_eq!(c.call(1, "doc.get", json!({"name": name}))["error"]["data"]["kind"], "invalid_params");
    }
    let result = c.call(2, "doc.set", json!({"name": "app", "value": "x".repeat(1024 * 1024), "version": 0}));
    assert_eq!(result["error"]["data"]["kind"], "invalid_params");
    assert_eq!(c.call(3, "doc.get", json!({"name": "app"}))["result"]["version"], 0);
}

#[test]
fn reconnecting_a_machine_tells_document_readers_to_refetch() {
    let remote = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &remote)]));
    s.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    s.json(&["machine", "add", "rohan"]);
    let mut events = s.connect();
    events.call(1, "events.subscribe", json!({}));
    let mut c = s.connect();
    c.call(1, "doc.set", json!({"on": "rohan", "name": "app", "value": "before", "version": 0}));
    next_event(&mut events, |m| m["params"]["type"] == "doc");

    std::fs::write(s.root.join("down"), "").unwrap();
    remote.hc(&["server", "restart"]);
    wait_until("remote unreachable", || s.json(&["machine", "ls"])["machines"][0]["state"] == "unreachable");
    remote.json(&["server", "status"]);
    assert_eq!(
        remote.connect().call(2, "doc.set", json!({"name": "app", "value": "during gap", "version": 1}))["result"]["version"],
        2
    );
    std::fs::remove_file(s.root.join("down")).unwrap();
    let missed = next_event(&mut events, |m| m["method"] == "missed");
    assert!(missed["params"]["seq"].is_u64());
    assert_eq!(
        c.call(3, "doc.get", json!({"on": "rohan", "name": "app"}))["result"],
        json!({"value": "during gap", "version": 2})
    );
}

#[test]
fn documents_work_through_another_core_without_rewriting_json() {
    let remote = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &remote)]));
    s.json(&["machine", "add", "rohan"]);
    wait_until("connected", || s.json(&["machine", "ls"])["machines"][0]["state"] == "connected");
    s.json(&["machine", "rename", "rohan", "desktop"]);
    let mut events = s.connect();
    events.call(1, "events.subscribe", json!({}));
    let mut c = s.connect();
    let value = json!({"terminal": "t1", "parent": "t2", "nested": {"address": "t3"}});
    assert_eq!(
        c.call(1, "doc.set", json!({"on": "desktop", "name": "app", "value": value, "version": 0}))["result"]["version"],
        1
    );
    assert_eq!(
        c.call(2, "doc.get", json!({"on": "desktop", "name": "app"}))["result"],
        json!({"value": value, "version": 1})
    );
    let event = next_event(&mut events, |m| m["params"]["type"] == "doc");
    assert_eq!(event["params"]["data"], json!({"name": "app", "version": 1}));
    assert_eq!(event["params"]["machine"], "desktop");
    assert_eq!(c.call(3, "doc.get", json!({"name": "app"}))["result"]["version"], 0);
}
