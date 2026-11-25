#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

#[test]
fn a_file_lands_beside_the_terminal_keeps_its_name_and_goes_with_it() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let image: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let file = s.root.join("shot.png");
    std::fs::write(&file, &image).unwrap();

    let here = run(&s, &["sleep", "100"]);
    let first = s.json(&["put", &here, file.to_str().unwrap()]);
    let path = first["path"].as_str().unwrap();
    assert!(path.starts_with(&s.root.join("state").display().to_string()) && path.ends_with("/shot.png"), "{first}");
    assert_eq!((std::fs::read(path).unwrap(), first["bytes"].as_u64()), (image.clone(), Some(300_000)));
    let second = s.json(&["put", &here, file.to_str().unwrap()]);
    assert!(second["path"].as_str().unwrap().ends_with("/shot-2.png"), "{second}");

    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    let afar = s.json(&["put", &there, file.to_str().unwrap()]);
    let afar_path = afar["path"].as_str().unwrap();
    assert!(afar_path.starts_with(&rohan.root.display().to_string()), "lands on rohan: {afar}");
    assert_eq!(std::fs::read(afar_path).unwrap(), image);

    s.json(&["kill", &here]);
    s.json(&["rm", &here]);
    assert!(!std::path::Path::new(path).exists(), "removed with the terminal");

    std::fs::write(&file, vec![0u8; 9 << 20]).unwrap();
    assert_eq!(
        s.err(&["--json", "put", &there, file.to_str().unwrap()]).1["error"]["kind"],
        "invalid_params",
        "too big"
    );
    // Too big even to carry to rohan: refused here, and the line lives on.
    std::fs::write(&file, vec![0u8; 13 << 20]).unwrap();
    assert_eq!(s.err(&["--json", "put", &there, file.to_str().unwrap()]).1["error"]["kind"], "invalid_params");
    std::fs::write(&file, b"small").unwrap();
    s.json(&["put", &there, file.to_str().unwrap()]);
}

#[test]
fn a_put_can_cross_two_machines() {
    let end = Session::new();
    let mut middle = Session::new();
    middle.env.push(fake_ssh(&middle, &[("end", &end)]));
    middle.json(&["machine", "add", "end"]);
    let t = run(&end, &["sleep", "100"]);
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("middle", &middle)]));
    s.json(&["machine", "add", "middle"]);
    let file = s.root.join("two-hop.txt");
    std::fs::write(&file, b"through two machines").unwrap();
    let result = s.json(&["put", &format!("middle/end/{t}"), file.to_str().unwrap()]);
    let path = std::path::Path::new(result["path"].as_str().unwrap());
    assert!(path.starts_with(&end.root), "{result}");
    assert_eq!(std::fs::read(path).unwrap(), b"through two machines");
}

/// `s` with rohan behind a fake ssh whose uplink carries ~100 KB/s, and calls
/// there that may take 2 s each. While `<s root>/stall` exists the uplink
/// carries nothing, but still takes in what is sent, counted in `<s root>/sent`.
fn slow_uplink(s: &mut Session, rohan: &Session) {
    let (var, fake) = fake_ssh(s, &[("rohan", rohan)]);
    let throttle = s.root.join("throttle.py");
    std::fs::write(
        &throttle,
        "import os, select, sys, time\n\
         stall, count = sys.argv[1], sys.argv[2]\n\
         held, seen = b'', 0\n\
         while True:\n\
         \x20   if select.select([0], [], [], 0.02)[0]:\n\
         \x20       b = os.read(0, 65536)\n\
         \x20       if not b: break\n\
         \x20       held += b; seen += len(b)\n\
         \x20       open(count, 'w').write(str(seen))\n\
         \x20   if held and not os.path.exists(stall):\n\
         \x20       os.write(1, held[:10000]); held = held[10000:]; time.sleep(0.1)\n",
    )
    .unwrap();
    let (stall, sent) = (s.root.join("stall"), s.root.join("sent"));
    let slow = s.root.join("slow-ssh");
    let script =
        format!("#!/bin/sh\npython3 {} {} {} | {fake} \"$@\"\n", throttle.display(), stall.display(), sent.display());
    std::fs::write(&slow, script).unwrap();
    std::fs::set_permissions(&slow, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push((var, slow.display().to_string()));
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "machine_timeout = 2\n").unwrap();
    s.json(&["machine", "add", "rohan"]);
}

fn parts(rohan: &Session) -> Vec<std::path::PathBuf> {
    let files = rohan.root.join("state/herdr-core/test/files");
    let dirs = std::fs::read_dir(files).into_iter().flatten().flatten();
    let all = dirs.flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten());
    all.map(|f| f.path()).filter(|p| p.file_name().unwrap().to_string_lossy().starts_with('.')).collect()
}

#[test]
fn a_put_slower_than_a_deadline_lands_whole_while_the_line_keeps_answering() {
    let rohan = Session::new();
    let mut s = Session::new();
    slow_uplink(&mut s, &rohan);
    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    // Two at once, ~4 s together: twice the deadline of any one call.
    let files: Vec<(std::path::PathBuf, Vec<u8>)> = (0..2u8)
        .map(|n| {
            let bytes: Vec<u8> = (0..150_000u32).map(|i| (i * 7 + n as u32) as u8).collect();
            let file = s.root.join(format!("shot{n}.png"));
            std::fs::write(&file, &bytes).unwrap();
            (file, bytes)
        })
        .collect();
    let puts: Vec<_> = files
        .iter()
        .map(|(file, _)| {
            s.cmd().args(["--json", "put", &there]).arg(file).stdout(std::process::Stdio::piped()).spawn().unwrap()
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let started = std::time::Instant::now();
    s.json(&["get", &there]);
    let took = started.elapsed();
    for (put, (_, bytes)) in puts.into_iter().zip(&files) {
        let out = put.wait_with_output().unwrap();
        let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        assert_eq!(std::fs::read(r["path"].as_str().unwrap_or_default()).ok().as_ref(), Some(bytes), "{r}");
    }
    assert!(slow_runner() || took < std::time::Duration::from_millis(1500), "a call during the puts took {took:?}");
    assert!(parts(&rohan).is_empty(), "{:?}", parts(&rohan));
}

#[test]
fn a_put_whose_caller_goes_leaves_nothing_there_and_the_next_one_goes_on() {
    let rohan = Session::new();
    let mut s = Session::new();
    slow_uplink(&mut s, &rohan);
    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    let (gone, next) = (s.root.join("gone.png"), s.root.join("next.png"));
    std::fs::write(&gone, vec![7u8; 400_000]).unwrap();
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i * 13) as u8).collect();
    std::fs::write(&next, &bytes).unwrap();
    let mut put = s.cmd().args(["put", &there]).arg(&gone).spawn().unwrap();
    wait_until("a piece arrived", || !parts(&rohan).is_empty());
    // Waiting its turn behind the first while that one's caller goes.
    let waiting =
        s.cmd().args(["--json", "put", &there]).arg(&next).stdout(std::process::Stdio::piped()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    put.kill().unwrap();
    put.wait().unwrap();
    let started = std::time::Instant::now();
    s.json(&["get", &there]);
    let took = started.elapsed();
    let r: serde_json::Value = serde_json::from_slice(&waiting.wait_with_output().unwrap().stdout).unwrap_or_default();
    assert_eq!(std::fs::read(r["path"].as_str().unwrap_or_default()).ok(), Some(bytes), "{r}");
    assert!(slow_runner() || took < std::time::Duration::from_millis(1500), "a call meanwhile took {took:?}");
    wait_until("the gone one's part is removed", || parts(&rohan).is_empty());
    let files = rohan.root.join("state/herdr-core/test/files");
    let landed: Vec<_> = std::fs::read_dir(files)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap())
        .map(|f| f.unwrap().file_name())
        .collect();
    assert_eq!(landed, ["next.png"], "only the finished one landed");
}

#[test]
fn a_put_piped_into_connect_answers_after_its_caller_is_done_sending() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    for t in [run(&s, &["sleep", "100"]), run(&s, &["--on", "rohan", "sleep", "100"])] {
        let data = herdr_core::put::encode(b"piped");
        let req = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.put", "params": {"terminal": t, "name": "p.txt", "data": data}});
        let mut connect = s
            .cmd()
            .arg("connect")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut connect.stdin.take().unwrap(), format!("{req}\n").as_bytes()).unwrap();
        let out = connect.wait_with_output().unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        assert_eq!(
            std::fs::read(reply["result"]["path"].as_str().unwrap_or_default()).ok().as_deref(),
            Some(&b"piped"[..]),
            "{t}: {reply}"
        );
    }
}

/// A framed connection's call and its answer.
fn framed(conn: &mut Conn, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    send(conn, Some(id), method, params);
    loop {
        let reply = next_reply(conn);
        if reply["id"] == id {
            return reply;
        }
    }
}

/// The next answer to a call on a framed connection.
fn next_reply(conn: &mut Conn) -> serde_json::Value {
    use herdr_core::wire;
    use std::io::Read;
    loop {
        let mut header = [0u8; wire::HEADER];
        conn.reader.read_exact(&mut header).unwrap();
        let (channel, _, len) = wire::decode_header(&header).unwrap();
        let mut payload = vec![0; len];
        conn.reader.read_exact(&mut payload).unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
        if channel == 0 && reply.get("id").is_some() {
            return reply;
        }
    }
}

#[test]
fn pieces_must_come_in_order_and_whole_and_go_with_their_connection() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let taken = s.root.join("a.png");
    std::fs::write(&taken, b"old").unwrap();
    let old = s.json(&["put", &t, taken.to_str().unwrap()]);
    let mut conn = s.connect();
    assert_eq!(conn.call(1, "hello", serde_json::json!({"framed": true}))["result"]["framed"], true);
    let mut id = 1;
    let mut call = |conn: &mut Conn, method: &str, params: serde_json::Value| {
        id += 1;
        framed(conn, id, method, params)
    };
    let kind = |r: &serde_json::Value| r["error"]["data"]["kind"].as_str().unwrap_or_default().to_string();
    let b64 = herdr_core::put::encode;

    let begin = |name: &str, bytes: usize, transfer: &str| serde_json::json!({"terminal": t, "name": name, "bytes": bytes, "transfer": transfer});
    assert_eq!(
        kind(&call(&mut conn, "terminal.put_begin", begin("a.png", 9 << 20, "aaaaaaaaaaaaaaaa"))),
        "invalid_params"
    );
    assert_eq!(kind(&call(&mut conn, "terminal.put_begin", begin("a.png", 6, "../not-hex-at-all"))), "invalid_params");
    let begun = call(&mut conn, "terminal.put_begin", begin("a.png", 6, "aaaaaaaaaaaaaaaa"));
    let transfer = begun["result"]["transfer"].clone();
    assert_eq!(
        kind(&call(&mut conn, "terminal.put_begin", begin("a.png", 6, "aaaaaaaaaaaaaaaa"))),
        "invalid_params",
        "taken"
    );
    let piece =
        |offset: usize, data: &[u8]| serde_json::json!({"transfer": transfer, "offset": offset, "data": b64(data)});
    assert_eq!(
        kind(&call(&mut conn, "terminal.put_chunk", piece(1, b"abc"))),
        "invalid_params",
        "not where it left off"
    );
    assert_eq!(kind(&call(&mut conn, "terminal.put_chunk", piece(0, b"abcdefg"))), "invalid_params", "past its size");
    assert_eq!(call(&mut conn, "terminal.put_chunk", piece(0, b"abc"))["result"]["bytes"], 3);
    let early = call(&mut conn, "terminal.put_finish", serde_json::json!({"transfer": transfer}));
    assert_eq!(kind(&early), "invalid_params", "half a file never lands: {early}");
    assert_eq!(kind(&call(&mut conn, "terminal.put_chunk", piece(3, b"def"))), "not_found", "gone with its failure");

    let begun = call(&mut conn, "terminal.put_begin", begin("a.png", 3, "bbbbbbbbbbbbbbbb"));
    let transfer = begun["result"]["transfer"].clone();
    call(&mut conn, "terminal.put_chunk", serde_json::json!({"transfer": transfer, "offset": 0, "data": b64(b"new")}));
    let done = call(&mut conn, "terminal.put_finish", serde_json::json!({"transfer": transfer}));
    let path = done["result"]["path"].as_str().unwrap();
    assert!(path.ends_with("/a-2.png"), "{done}");
    assert_eq!(std::fs::read(path).unwrap(), b"new");
    assert_eq!(std::fs::read(old["path"].as_str().unwrap()).unwrap(), b"old", "never replaced");

    call(&mut conn, "terminal.put_begin", begin("b.png", 3, "cccccccccccccccc"));
    let dir = std::path::Path::new(path).parent().unwrap().to_path_buf();
    let hidden = || {
        std::fs::read_dir(&dir).unwrap().flatten().filter(|f| f.file_name().to_string_lossy().starts_with('.')).count()
    };
    assert_eq!(hidden(), 1);
    drop(conn);
    wait_until("its part goes with its connection", || hidden() == 0);
}

/// Sends a call on a framed connection without waiting for its answer.
fn send(conn: &mut Conn, id: Option<u64>, method: &str, params: serde_json::Value) {
    let mut req = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
    if let Some(id) = id {
        req["id"] = id.into();
    }
    std::io::Write::write_all(
        &mut conn.writer,
        &herdr_core::wire::encode(0, herdr_core::wire::DATA, req.to_string().as_bytes()),
    )
    .unwrap();
}

fn framed_conn(s: &Session) -> Conn {
    let mut conn = s.connect();
    assert_eq!(conn.call(1, "hello", serde_json::json!({"framed": true}))["result"]["framed"], true);
    conn
}

#[test]
fn puts_given_up_before_their_begin_ran_keep_no_slot() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let begin =
        |n: usize| serde_json::json!({"terminal": t, "name": "x.png", "bytes": 3, "transfer": format!("{n:016x}")});
    // Begun, then the connection closed before any answer: more than the 16 a core takes.
    for n in 0..20 {
        let mut conn = framed_conn(&s);
        send(&mut conn, Some(2), "terminal.put_begin", begin(n));
    }
    // Begun and aborted at once on a connection that stays: the abort may run first.
    let mut stays = framed_conn(&s);
    for n in 100..120 {
        send(&mut stays, Some(n as u64), "terminal.put_begin", begin(n));
        send(&mut stays, None, "terminal.put_abort", serde_json::json!({"transfer": format!("{n:016x}")}));
    }
    for n in 100..120 {
        framed(&mut stays, 1000 + n, "terminal.get", serde_json::json!({"terminal": t}));
    }
    let dir = s.root.join("state/herdr-core/test/files").join(&t);
    let hidden = || {
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|f| f.file_name().to_string_lossy().starts_with('.'))
            .count()
    };
    wait_until("no part left", || hidden() == 0);
    let mut fresh = framed_conn(&s);
    for n in 200..216 {
        let r = framed(&mut fresh, n as u64, "terminal.put_begin", begin(n));
        assert!(r["result"]["transfer"].is_string(), "slot {n} free: {r}");
    }
    let full = framed(&mut fresh, 300, "terminal.put_begin", begin(300));
    assert_eq!(full["error"]["data"]["kind"], "unavailable", "{full}");
}

#[test]
fn pieces_stop_once_their_terminal_has_finished() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let mut conn = framed_conn(&s);
    let begin = serde_json::json!({"terminal": t, "name": "x.png", "bytes": 6, "transfer": "dddddddddddddddd"});
    assert!(framed(&mut conn, 2, "terminal.put_begin", begin)["result"].is_object());
    s.json(&["kill", &t]);
    let piece = |offset: usize| serde_json::json!({"transfer": "dddddddddddddddd", "offset": offset, "data": herdr_core::put::encode(b"abc")});
    assert_eq!(framed(&mut conn, 3, "terminal.put_chunk", piece(0))["error"]["data"]["kind"], "finished");
    assert_eq!(framed(&mut conn, 4, "terminal.put_chunk", piece(0))["error"]["data"]["kind"], "not_found", "dropped");
}

#[test]
fn a_late_piece_leaves_alone_a_put_that_took_its_handle_again() {
    let s = Session::new();
    let (a, b) = (run(&s, &["sleep", "100"]), run(&s, &["sleep", "100"]));
    let mut conn = framed_conn(&s);
    let h = "eeeeeeeeeeeeeeee";
    let begin = |t: &str| serde_json::json!({"terminal": t, "name": "x.png", "bytes": 3, "transfer": h});
    let piece = |data: &[u8]| serde_json::json!({"transfer": h, "offset": 0, "data": herdr_core::put::encode(data)});
    assert!(framed(&mut conn, 2, "terminal.put_begin", begin(&a))["result"].is_object());
    // A's part becomes a fifo: writing a piece into it waits for a reader.
    let part = s.root.join("state/herdr-core/test/files").join(&a).join(format!(".hc-put-{h}"));
    std::fs::remove_file(&part).unwrap();
    assert!(std::process::Command::new("mkfifo").arg(&part).status().unwrap().success());
    send(&mut conn, Some(3), "terminal.put_chunk", piece(b"old"));
    let mut n = 3;
    wait_until("A's piece is being written", || {
        n += 1;
        let r = framed(&mut conn, n, "terminal.put_chunk", piece(b"four"));
        r["error"]["message"].as_str().unwrap_or_default().contains("next piece starts")
    });
    assert!(framed(&mut conn, 100, "terminal.put_abort", serde_json::json!({"transfer": h}))["result"].is_object());
    assert!(framed(&mut conn, 101, "terminal.put_begin", begin(&b))["result"].is_object());
    drop(std::fs::File::open(&part).unwrap());
    wait_until("A's part removed by its late piece", || !part.exists());
    assert_eq!(framed(&mut conn, 102, "terminal.put_chunk", piece(b"new"))["result"]["bytes"], 3, "B untouched");
    let done = framed(&mut conn, 103, "terminal.put_finish", serde_json::json!({"transfer": h}));
    assert_eq!(std::fs::read(done["result"]["path"].as_str().unwrap()).unwrap(), b"new", "{done}");
}

#[test]
fn concurrent_puts_wait_when_the_recipient_is_full() {
    let rohan = Session::new();
    let mut s = Session::new();
    slow_uplink(&mut s, &rohan);
    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    let mut conn = framed_conn(&s);
    let count = 4 + herdr_core::put::MAX_TRANSFERS as u64;
    let data = herdr_core::put::encode(&[3; 4096]);
    for id in 1..=count {
        send(
            &mut conn,
            Some(id),
            "terminal.put",
            serde_json::json!({"terminal": there, "name": format!("{id}.png"), "data": data}),
        );
    }
    for _ in 0..count {
        let done = next_reply(&mut conn);
        let path = done["result"]["path"].as_str().unwrap_or_else(|| panic!("{done}"));
        assert_eq!(std::fs::read(path).unwrap(), [3; 4096]);
    }
}

#[test]
fn a_piece_past_its_deadline_keeps_the_line_until_it_is_answered() {
    let rohan = Session::new();
    let mut s = Session::new();
    slow_uplink(&mut s, &rohan);
    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    let put = |name: &str, bytes: &[u8]| {
        let file = s.root.join(name);
        std::fs::write(&file, bytes).unwrap();
        let piped = std::process::Stdio::piped;
        s.cmd().args(["--json", "put", &there]).arg(file).stdout(piped()).stderr(piped()).spawn().unwrap()
    };
    let (a, b) = (put("a.png", &[1u8; 400_000]), put("b.png", &[2u8; 400_000]));
    wait_until("both begun, taking turns", || parts(&rohan).len() == 2);
    let sent = || std::fs::read_to_string(s.root.join("sent")).unwrap_or_default().parse::<usize>().unwrap_or(0);
    std::fs::write(s.root.join("stall"), b"").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (stalled, at) = (std::time::Instant::now(), sent());
    // The piece on its way times out; the other put must not send one behind it.
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    let took = stalled.elapsed();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let more = sent() - at;
    std::fs::remove_file(s.root.join("stall")).unwrap();
    for out in [&a, &b] {
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap_or_default();
        assert_eq!(err["error"]["kind"], "unavailable", "{err}");
    }
    assert!(slow_runner() || took < std::time::Duration::from_secs(6), "they heard after {took:?}");
    assert!(more < herdr_core::put::FIRST_PIECE, "{more} bytes queued behind an unanswered piece");
    wait_until("both parts removed once the line is free", || parts(&rohan).is_empty());
    // A put meets the stuck lane at once until the late piece is answered.
    wait_until("the line takes puts again", || put("c.png", b"after").wait_with_output().unwrap().status.success());
}

#[test]
fn one_handle_is_one_transfer_even_begun_twice_at_once() {
    let s = Session::new();
    let (t1, t2) = (run(&s, &["sleep", "100"]), run(&s, &["sleep", "100"]));
    let mut conn = framed_conn(&s);
    let handle = "eeeeeeeeeeeeeeee";
    for (id, t) in [(2, &t1), (3, &t2)] {
        send(
            &mut conn,
            Some(id),
            "terminal.put_begin",
            serde_json::json!({"terminal": t, "name": "x.png", "bytes": 3, "transfer": handle}),
        );
    }
    let replies: Vec<serde_json::Value> = (0..2).map(|_| next_reply(&mut conn)).collect();
    let ok = replies.iter().filter(|r| r["result"]["transfer"] == handle).count();
    assert_eq!(ok, 1, "{replies:?}");
    let hidden: usize = [&t1, &t2]
        .iter()
        .map(|t| {
            std::fs::read_dir(s.root.join("state/herdr-core/test/files").join(t))
                .into_iter()
                .flatten()
                .flatten()
                .count()
        })
        .sum();
    assert_eq!(hidden, 1, "only the begun one keeps a part");
}

#[test]
fn an_abort_with_a_bad_handle_is_refused() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = framed_conn(&s);
    for bad in ["not hex at all!".to_string(), "a".repeat(100_000)] {
        let r = framed(&mut conn, 2, "terminal.put_abort", serde_json::json!({"transfer": bad}));
        assert_eq!(r["error"]["data"]["kind"], "invalid_params", "{r}");
    }
}

#[test]
fn a_piece_never_answered_gives_the_line_up_and_the_next_put_works() {
    let rohan = Session::new();
    let mut s = Session::new();
    let (var, fake) = fake_ssh(&s, &[("rohan", &rohan)]);
    // Rohan's answer to the first piece ever is lost; everything else comes back.
    let filter = s.root.join("withhold.py");
    std::fs::write(
        &filter,
        "import os, struct, sys\n\
         def read(n):\n\
         \x20   b = b''\n\
         \x20   while len(b) < n:\n\
         \x20       c = os.read(0, n - len(b))\n\
         \x20       if not c: sys.exit(0)\n\
         \x20       b += c\n\
         \x20   return b\n\
         hello = b''\n\
         while not hello.endswith(b'\\n'): hello += read(1)\n\
         os.write(1, hello)\n\
         while True:\n\
         \x20   h = read(9); p = read(struct.unpack('<IBI', h)[2])\n\
         \x20   if b'\"result\":{\"bytes\":' in p and not os.path.exists(sys.argv[1]):\n\
         \x20       open(sys.argv[1], 'w').close(); continue\n\
         \x20   os.write(1, h + p)\n",
    )
    .unwrap();
    let withheld = s.root.join("withheld");
    let ssh = s.root.join("withholding-ssh");
    let script = format!("#!/bin/sh\n{fake} \"$@\" | python3 {} {}\n", filter.display(), withheld.display());
    std::fs::write(&ssh, script).unwrap();
    std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    s.env.push((var, ssh.display().to_string()));
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "machine_timeout = 1\n").unwrap();
    s.json(&["machine", "add", "rohan"]);
    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    let file = s.root.join("shot.png");
    std::fs::write(&file, b"withheld piece").unwrap();

    let (_, err) = s.err(&["--json", "put", &there, file.to_str().unwrap()]);
    assert!(withheld.exists() && err["error"]["message"].as_str().unwrap_or_default().contains("a piece"), "{err}");
    s.json(&["get", &there]); // the line still answers everything else
    wait_until("a put works again on a fresh line", || s.hc(&["put", &there, file.to_str().unwrap()]).status.success());
}

#[test]
fn aborts_for_puts_never_begun_keep_nothing() {
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = framed_conn(&s);
    for n in 0..100 {
        send(&mut conn, None, "terminal.put_abort", serde_json::json!({"transfer": format!("{n:016x}")}));
    }
    let r = framed(&mut conn, 2, "server.status", serde_json::json!({}));
    assert!(r["result"]["pid"].is_number(), "still connected: {r}");
    // Aborted before they were begun on this connection: these begins are not refused.
    let t = run(&s, &["sleep", "100"]);
    let begin = serde_json::json!({"terminal": t, "name": "x.png", "bytes": 1, "transfer": format!("{:016x}", 7)});
    assert!(framed(&mut conn, 3, "terminal.put_begin", begin)["result"].is_object());
}

#[test]
fn failed_puts_over_a_machine_line_never_end_it() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let there = run(&s, &["--on", "rohan", "true"]);
    wait_until("it finished", || get(&s, &there)["state"] == "finished");
    let mut conn = s.connect();
    let data = herdr_core::put::encode(b"x");
    for id in 0..100 {
        let r = conn.call(id, "terminal.put", serde_json::json!({"terminal": there, "name": "x", "data": data}));
        assert_eq!(r["error"]["data"]["kind"], "finished", "{r}");
    }
    let log = std::fs::read_to_string(rohan.root.join("state/herdr-core/test/core.log")).unwrap_or_default();
    assert!(!log.contains("too many puts"), "{log}");
    let alive = run(&s, &["--on", "rohan", "sleep", "100"]);
    let file = s.root.join("ok.txt");
    std::fs::write(&file, b"ok").unwrap();
    s.json(&["put", &alive, file.to_str().unwrap()]);
}

#[test]
fn malformed_begins_leave_nothing_pending() {
    let s = Session::new();
    let t = run(&s, &["sleep", "100"]);
    let mut conn = framed_conn(&s);
    for n in 0..100u64 {
        let bad = serde_json::json!({"transfer": format!("{n:016x}"), "bytes": "many"});
        assert_eq!(framed(&mut conn, n + 2, "terminal.put_begin", bad)["error"]["data"]["kind"], "invalid_params");
    }
    // Still pending, each abort would be kept, and past 64 the connection ended.
    for n in 0..100u64 {
        send(&mut conn, None, "terminal.put_abort", serde_json::json!({"transfer": format!("{n:016x}")}));
    }
    let begin = serde_json::json!({"terminal": t, "name": "x.png", "bytes": 1, "transfer": format!("{:016x}", 5)});
    let r = framed(&mut conn, 500, "terminal.put_begin", begin);
    assert!(r["result"].is_object(), "not refused as aborted, and still connected: {r}");
}
