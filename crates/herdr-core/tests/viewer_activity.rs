#![cfg(unix)]
mod common;

use common::framed::Viewer;
use common::*;
use herdr_core::wire;
use serde_json::json;
use std::time::Duration;

#[test]
fn automatic_replies_do_not_take_the_size() {
    let s = Session::new();
    let log = s.root.join("input");
    let program = format!(
        "stty raw -echo; dd bs=1 of={} </dev/tty 2>/dev/null & printf ready; \
         for n in 0 1 2; do while [ ! -e {}/query$n ]; do sleep 0.02; done; \
         printf '\\033[?2048l\\033[?2048h\\033_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\\033\\\\'; done; wait",
        log.display(),
        s.root.display()
    );
    let t = run(&s, &["sh", "-c", &program]);
    let mut idle = Viewer::attach(&s, &t, 80, 24, json!({}));
    idle.shows("ready");
    let mut leader = Viewer::attach(&s, &t, 100, 30, json!({}));
    leader.shows("ready");
    for round in 0..3 {
        idle.raw.clear();
        leader.raw.clear();
        touch(&s, &format!("query{round}"));
        idle.until("kitty query", |v| v.raw.windows(6).any(|w| w == b"AAAA\x1b\\"));
        leader.until("kitty query", |v| v.raw.windows(6).any(|w| w == b"AAAA\x1b\\"));
        for viewer in [&mut idle, &mut leader] {
            let replies = viewer.screen.take_responses();
            assert!(!replies.is_empty());
            viewer.conn.send(viewer.channel, wire::AUTOMATIC_INPUT, &replies);
            viewer.conn.call("terminal.get", json!({"terminal": t}));
            assert_eq!(get(&s, &t)["cols"], 100);
        }
    }
    wait_until("program received both viewers replies", || {
        std::fs::read(&log).is_ok_and(|bytes| bytes.windows(3).filter(|w| *w == b";OK").count() == 6)
    });
    // Every possible frame boundary is safe, including a delayed lone ESC.
    for byte in b"\x1bP1+r544e=787465726d\x1b\\\x1b[48;24;80;384;640t" {
        idle.conn.send(idle.channel, wire::AUTOMATIC_INPUT, &[*byte]);
    }
    idle.conn.send(idle.channel, wire::AUTOMATIC_INPUT, b"\x1b");
    idle.conn.call("terminal.get", json!({"terminal": t}));
    std::thread::sleep(Duration::from_millis(100));
    idle.conn.send(idle.channel, wire::AUTOMATIC_INPUT, b"[48;24;80;384;640t");
    idle.conn.call("terminal.get", json!({"terminal": t}));
    assert_eq!(get(&s, &t)["cols"], 100);
    idle.typed(b"x");
    wait_until("typing takes size", || get(&s, &t)["cols"] == 80);
    leader.conn.send(leader.channel, wire::FOCUS, &[1]);
    wait_until("focus takes size", || get(&s, &t)["cols"] == 100);
    idle.conn.send(idle.channel, wire::RESIZE, &[90, 0, 25, 0]);
    wait_until("real resize takes size", || get(&s, &t)["cols"] == 90);
    leader.typed(b"\x1b");
    wait_until("Escape key takes size", || get(&s, &t)["cols"] == 100);
    idle.typed(b"\x1b[A");
    wait_until("arrow key takes size", || get(&s, &t)["cols"] == 90);
}
