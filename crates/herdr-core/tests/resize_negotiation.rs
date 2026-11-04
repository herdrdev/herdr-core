#![cfg(unix)]

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::json;
use std::time::Duration;

#[test]
fn attach_confirms_size_notifications_locally_and_through_two_relays() {
    let owner = Session::new();
    let t = run(&owner, &["sleep", "100"]);
    let _leader = Viewer::attach(&owner, &t, 80, 24, json!({}));
    let mut relay = Session::new();
    relay.env.push(fake_ssh(&relay, &[("owner", &owner)]));
    relay.json(&["machine", "add", "owner"]);
    let mut client = Session::new();
    client.env.push(fake_ssh(&client, &[("relay", &relay)]));
    client.json(&["machine", "add", "relay"]);
    for (session, terminal) in [(&owner, t.clone()), (&client, format!("relay/owner/{t}"))] {
        for (observe, requested) in [(true, true), (true, false), (false, true)] {
            let mut conn = Framed::open(session);
            let reply = conn.call(
                "terminal.attach",
                json!({
                    "terminal": terminal, "cols": 80, "rows": 24,
                    "observe": observe, "resize_notifications": requested
                }),
            );
            let enabled = observe && requested;
            assert_eq!(reply["result"]["resize_notifications"], enabled, "{reply}");
            if enabled {
                let channel = reply["result"]["channel"].as_u64().unwrap() as u32;
                assert!(conn.stash.is_empty(), "reply must precede stream frames");
                assert_eq!(conn.read(Duration::from_secs(5)), Some((channel, wire::RESIZE, vec![80, 0, 24, 0])));
            }
        }
    }
}
