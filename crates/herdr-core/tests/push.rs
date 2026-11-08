#![cfg(unix)] // the fake agents are sh scripts; no Windows test covers webhooks (the sender is the same code)
//! Webhooks end to end: a fake agent reports status, the core POSTs to local
//! listeners shaped like ntfy and NextPush (loopback http).
mod common;
use common::*;
use herdr_core::status::{base64url, base64url_decode as b64};
use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    at: Instant,
}

impl Hit {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

type Answer = Box<dyn Fn(&Hit, usize) -> (u16, &'static str) + Send>;

/// A listener on loopback: every request is kept, and `answer` (given the
/// request and how many came to that path before) picks the status and extra headers.
fn listen(answer: Answer) -> (String, Arc<Mutex<Vec<Hit>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let kept = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split(' ').nth(1).unwrap_or_default().to_string();
            let mut headers = Vec::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                match line.trim_end().split_once(':') {
                    Some((k, v)) => headers.push((k.trim().to_string(), v.trim().to_string())),
                    None => break,
                }
            }
            let mut out = stream;
            let hit = Hit { path, headers, body: Vec::new(), at: Instant::now() };
            if hit.header("expect").is_some() {
                out.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
            }
            let length: usize = hit.header("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let hit = Hit { body, ..hit };
            let before = kept.lock().unwrap().iter().filter(|h: &&Hit| h.path == hit.path).count();
            let (status, extra) = answer(&hit, before);
            kept.lock().unwrap().push(hit);
            let reply = format!("HTTP/1.1 {status} X\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = out.write_all(reply.as_bytes());
        }
    });
    (base, hits)
}

fn hits(all: &Arc<Mutex<Vec<Hit>>>, path: &str) -> Vec<Hit> {
    all.lock().unwrap().iter().filter(|h| h.path == path).cloned().collect()
}

/// A UnifiedPush receiver: its private key (good for one message), and its p256dh and auth as an app registers them.
fn receiver() -> (EphemeralPrivateKey, String, String) {
    let key = EphemeralPrivateKey::generate(&ECDH_P256, &ring::rand::SystemRandom::new()).unwrap();
    let public = base64url(key.compute_public_key().unwrap().as_ref());
    (key, public, base64url(&[9; 16]))
}

/// RFC 8291 decryption, as the phone does it.
fn decrypt(body: &[u8], key: EphemeralPrivateKey, auth: &str) -> Value {
    use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
    let (salt, rest) = body.split_at(16);
    assert_eq!(rest[..4], 4096u32.to_be_bytes());
    let (sender, sealed) = rest[5..].split_at(rest[4] as usize);
    let receiver = key.compute_public_key().unwrap();
    let (cek, nonce) = agree_ephemeral(key, &UnparsedPublicKey::new(&ECDH_P256, sender), |shared| {
        herdr_core::push::keys(shared, receiver.as_ref(), sender, &b64(auth).unwrap(), salt)
    })
    .unwrap();
    let cipher = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).unwrap());
    let mut sealed = sealed.to_vec();
    let plain = cipher.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut sealed).unwrap();
    assert_eq!(plain.last(), Some(&2), "one last record");
    serde_json::from_slice(&plain[..plain.len() - 1]).unwrap()
}

/// The VAPID header verifies against `hc push key`, for this origin.
fn vapid_ok(hit: &Hit, key: &str, origin: &str) -> bool {
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};
    let Some(auth) = hit.header("authorization").and_then(|a| a.strip_prefix("vapid t=")) else { return false };
    let Some((jwt, k)) = auth.split_once(", k=") else { return false };
    let (signed, signature) = jwt.rsplit_once('.').unwrap();
    let public = UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, b64(key).unwrap());
    let claims: Value = serde_json::from_slice(&b64(signed.split('.').nth(1).unwrap()).unwrap()).unwrap();
    k == key && claims["aud"] == origin && public.verify(signed.as_bytes(), &b64(signature).unwrap()).is_ok()
}

/// The VAPID JWT's claims, unverified.
fn claims(hit: &Hit) -> Value {
    let auth = hit.header("authorization").and_then(|a| a.strip_prefix("vapid t=")).unwrap();
    serde_json::from_slice(&b64(auth.split('.').nth(1).unwrap()).unwrap()).unwrap()
}

/// A fake agent that reports working, then `blocked` with this message, then waits.
fn blocked_agent(s: &Session, msg: &str) -> String {
    run(
        s,
        &[
            "--",
            "sh",
            "-c",
            &script(s, &format!("r state=working; r 'state=blocked:msg={}'; w never", base64url(msg.as_bytes()))),
        ],
    )
}

fn add(s: &Session, name: &str, url: &str, extra: &[&str]) {
    s.json(&[&["push", "add", name, "--url", url], extra].concat());
}

#[test]
fn a_blocked_agent_is_pushed_once_to_each_format_and_verifies() {
    let s = Session::new();
    let k = s.json(&["push", "key"])["key"].as_str().unwrap().to_string();
    // NextPush checks the VAPID header against the key the app registered: 403 otherwise.
    let (base, all) = listen(Box::new(move |hit, _| match hit.path.as_str() {
        "/index.php/apps/uppush/push/tok"
            if !vapid_ok(hit, &k, &hit.header("host").map(|h| format!("http://{h}")).unwrap()) =>
        {
            (403, "")
        }
        "/index.php/apps/uppush/push/tok" => (201, ""),
        _ => (200, ""),
    }));
    let (secret_key, p256dh, auth) = receiver();
    add(&s, "hook", &format!("{base}/hook"), &["--secret", "s3cret"]);
    let up = ["--format", "unifiedpush", "--p256dh", &p256dh, "--auth", &auth];
    add(&s, "ntfy", &format!("{base}/upAbc?up=1"), &up);
    let contact = [&up[..], &["--contact", "mailto:ops@example.com"]].concat();
    add(&s, "nextpush", &format!("{base}/index.php/apps/uppush/push/tok"), &contact);
    let listed = s.json(&["push", "ls"]);
    assert!(!listed.to_string().contains("s3cret") && !listed.to_string().contains(&auth), "secrets are never echoed");
    let t = blocked_agent(&s, "Run tests?");
    wait_until("three posts", || all.lock().unwrap().len() >= 3);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(all.lock().unwrap().len(), 3, "once each, working not pushed: {:?}", all.lock().unwrap());

    let hook = &hits(&all, "/hook")[0];
    let ts: u64 = hook.header("x-hc-timestamp").unwrap().parse().unwrap();
    let signature = format!("sha256={}", herdr_core::push::sign("s3cret", ts, &hook.body));
    assert_eq!(hook.header("x-hc-signature"), Some(signature.as_str()));
    let envelope: Value = serde_json::from_slice(&hook.body).unwrap();
    assert_eq!(
        (&envelope["type"], &envelope["state"], &envelope["terminal"]),
        (&json!("status"), &json!("blocked"), &json!(t))
    );
    assert_eq!(envelope["event"]["record"]["status"]["msg"], "Run tests?");

    let ntfy = &hits(&all, "/upAbc?up=1")[0];
    assert_eq!((ntfy.header("content-encoding"), ntfy.header("urgency")), (Some("aes128gcm"), Some("high")));
    assert!(ntfy.body.len() <= 4096);
    let plain = decrypt(&ntfy.body, secret_key, &auth);
    assert_eq!((&plain["seq"], &plain["ms"], &plain["state"]), (&envelope["seq"], &envelope["ms"], &json!("blocked")));
    let nextpush =
        s.json(&["push", "ls"])["webhooks"].as_array().unwrap().iter().find(|h| h["name"] == "nextpush").cloned();
    assert_eq!(nextpush.unwrap()["last_status"], 201, "the VAPID header verified");
    let subs = |path: &str| claims(&hits(&all, path)[0])["sub"].clone();
    assert_eq!(
        (subs("/upAbc?up=1"), subs("/index.php/apps/uppush/push/tok")),
        (Value::Null, json!("mailto:ops@example.com")),
        "one origin, a JWT per contact"
    );
    s.json(&["kill", &t]);
}

#[test]
fn a_gone_endpoint_stops_and_a_retry_keeps_the_order() {
    let s = Session::new();
    let (base, all) = listen(Box::new(|hit, before| match (hit.path.as_str(), before) {
        ("/upGone?up=1", _) => (507, ""),
        ("/busy", 0) => (429, "Retry-After: 1\r\n"),
        _ => (200, ""),
    }));
    add(&s, "gone", &format!("{base}/upGone?up=1"), &[]);
    add(&s, "busy", &format!("{base}/busy"), &[]);
    let gone = || {
        let listed = s.json(&["push", "ls"])["webhooks"].clone();
        let gone = listed.as_array().unwrap().iter().find(|h| h["name"] == "gone").unwrap().clone();
        (gone["gone"].clone(), gone["last_status"].clone())
    };
    let first = blocked_agent(&s, "one");
    wait_until("gone", || gone() == (json!(true), json!(507)));
    let second = blocked_agent(&s, "two"); // while the first waits for its retry
    wait_until("the retry and the second event", || hits(&all, "/busy").len() >= 3);
    let busy = hits(&all, "/busy");
    let msg = |h: &Hit| serde_json::from_slice::<Value>(&h.body).unwrap()["event"]["record"]["status"]["msg"].clone();
    assert_eq!(busy.iter().map(msg).collect::<Vec<_>>(), ["one", "one", "two"], "one at a time, in order");
    let gap = busy[1].at - busy[0].at;
    assert!(gap >= Duration::from_millis(900) && gap < Duration::from_secs(5), "{gap:?}");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(hits(&all, "/upGone?up=1").len(), 1, "507 is not retried, and nothing follows it");
    s.json(&["kill", &first]);
    s.json(&["kill", &second]);
}

#[test]
fn a_big_event_is_fetched_from_the_log_also_after_a_restart() {
    let s = Session::new();
    let (base, all) = listen(Box::new(|_, _| (200, "")));
    let (secret_key, p256dh, auth) = receiver();
    add(&s, "json", &format!("{base}/json"), &[]);
    add(&s, "up", &format!("{base}/up"), &["--format", "unifiedpush", "--p256dh", &p256dh, "--auth", &auth]);
    let key = s.json(&["push", "key"]);
    let first = blocked_agent(&s, &"x".repeat(2000)); // in data and in the record: over 3993
    wait_until("both posts", || all.lock().unwrap().len() >= 2);
    let up = &hits(&all, "/up")[0];
    assert!(up.body.len() <= 4096, "{}", up.body.len());
    let summary = decrypt(&up.body, secret_key, &auth);
    assert!(summary.get("event").is_none() && summary["state"] == "blocked", "{summary}");
    let (seq, ms) = (summary["seq"].as_u64().unwrap(), summary["ms"].as_u64().unwrap());
    let full: Value = serde_json::from_slice(&hits(&all, "/json")[0].body).unwrap();
    assert_eq!((full["seq"].as_u64(), full["ms"].as_u64()), (Some(seq), Some(ms)), "one id for every target");
    let fetched = s.json(&["push", "events", "--seq", &seq.to_string(), "--ms", &ms.to_string()]);
    assert_eq!(fetched["events"][0]["event"], full["event"]);

    s.json(&["server", "restart"]);
    assert_eq!(s.json(&["push", "key"]), key, "the VAPID key is kept");
    assert_eq!(s.json(&["push", "ls"])["webhooks"].as_array().unwrap().len(), 2, "webhooks are kept");
    let second = blocked_agent(&s, "after");
    wait_until("posts after the restart", || all.lock().unwrap().len() >= 4);
    let after = s.json(&["push", "events", "--since", &format!("{seq}:{ms}")])["events"].clone();
    assert_eq!(after.as_array().unwrap().len(), 1, "{after}");
    assert_eq!(after[0]["event"]["terminal"], json!(second));
    assert!(after[0]["seq"].as_u64() < Some(seq) || after[0]["ms"].as_u64() > Some(ms));
    let again = s.json(&["push", "events", "--seq", &seq.to_string(), "--ms", &ms.to_string()]);
    assert_eq!(again["events"][0]["event"], full["event"], "still there after the restart");
    let (_, err) = s.err(&["push", "events", "--seq", "1", "--ms", "1"]);
    assert_eq!(err["error"]["kind"], "not_found", "{err}");
    s.json(&["kill", &first]);
    s.json(&["kill", &second]);
}

#[test]
fn a_restart_does_not_wait_for_an_endpoint_that_never_answers() {
    let s = Session::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/silent", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = tx.send(stream); // held open, never answered
        }
    });
    add(&s, "silent", &url, &[]);
    let mut test = s.cmd().args(["push", "test", "silent"]).stdout(std::process::Stdio::null()).spawn().unwrap();
    let _held = rx.recv_timeout(Duration::from_secs(10)).expect("the POST connects");
    let old = s.json(&["server", "status"])["pid"].as_i64().unwrap();
    let started = Instant::now();
    let new = s.json(&["server", "restart"])["pid"].as_i64().unwrap();
    assert!(new != old && !pid_alive(old), "restarted");
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    let _ = test.kill();
    let _ = test.wait();
}
