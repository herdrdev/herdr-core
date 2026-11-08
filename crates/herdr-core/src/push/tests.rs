use super::*;
use crate::status::base64url_decode as b64;

fn spec(format: PushFormat) -> PushAddParams {
    PushAddParams {
        name: "phone".into(),
        url: "https://ntfy.example/upAbc?up=1".into(),
        format,
        events: vec![],
        states: vec![],
        terminal: None,
        children_of: None,
        labels: vec![],
        secret: None,
        p256dh: None,
        auth: None,
        contact: None,
    }
}

fn status(state: &str) -> Value {
    json!({"type": "status", "terminal": "t7", "data": {"status": {"state": state}}})
}

#[test]
fn seal_reproduces_rfc_8291_appendix_a() {
    // The appendix's ECDH secret stands in for the agreement: ring's ECDH takes no fixed sender key.
    let shared = b64("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs").unwrap();
    let ua_public =
        b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4").unwrap();
    let as_public =
        b64("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8").unwrap();
    let auth = b64("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
    let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").unwrap().try_into().unwrap();
    let (cek, nonce) = keys(&shared, &ua_public, &as_public, &auth, &salt);
    assert_eq!((base64url(&cek), base64url(&nonce)), ("oIhVW04MRdy2XN9CiKLxTg".into(), "4h_95klXJ5E_qnoN".into()));
    let body = seal(b"When I grow up, I want to be a watermelon", &shared, &ua_public, &as_public, &auth, salt);
    assert_eq!(
        base64url(&body),
        "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
    );
}

#[test]
fn the_vapid_jwt_verifies_and_carries_aud_exp_and_sub() {
    use ring::signature::{ECDSA_P256_SHA256_FIXED, ECDSA_P256_SHA256_FIXED_SIGNING as ES256, EcdsaKeyPair};
    let rng = ring::rand::SystemRandom::new();
    let key =
        EcdsaKeyPair::from_pkcs8(&ES256, EcdsaKeyPair::generate_pkcs8(&ES256, &rng).unwrap().as_ref(), &rng).unwrap();
    let jwt = vapid_jwt(&key, "https://push.example:8443", 1_700_000_000, Some("mailto:me@example.com"));
    let (signed, signature) = jwt.rsplit_once('.').unwrap();
    let public = ring::signature::UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, b64(&vapid_key(&key)).unwrap());
    public.verify(signed.as_bytes(), &b64(signature).unwrap()).unwrap();
    let claims: Value = serde_json::from_slice(&b64(signed.split('.').nth(1).unwrap()).unwrap()).unwrap();
    assert_eq!(
        claims,
        json!({"aud": "https://push.example:8443", "exp": 1_700_000_000u64, "sub": "mailto:me@example.com"})
    );
    let k = vapid_key(&key);
    assert_eq!((k.len(), k.contains('=')), (87, false), "k= is unpadded");
}

#[test]
fn the_origin_keeps_a_port_and_drops_the_path() {
    assert_eq!(origin("https://Nc.example:8443/index.php/apps/uppush/push/tok").unwrap(), "https://nc.example:8443");
    assert_eq!(origin("http://u:p@10.0.0.7/upAbc?up=1").unwrap(), "http://10.0.0.7");
    assert_eq!(origin("https://"), None);
}

#[test]
fn the_signature_is_hmac_sha256_over_timestamp_dot_body() {
    // Python: hmac.new(b"Jefe", b"1700000000.what do ya want for nothing?", sha256).hexdigest()
    assert_eq!(
        sign("Jefe", 1_700_000_000, b"what do ya want for nothing?"),
        "1cdd0650c8be1cb0974b1788d458b1e781206cfef59b85faafc582d2e182c57e"
    );
}

#[test]
fn wants_counts_listed_events_and_new_words() {
    let s = spec(PushFormat::Json);
    assert!(wants(&s, &status("blocked"), Some("working")));
    assert!(!wants(&s, &status("blocked"), Some("blocked")), "same word again (a message update)");
    assert!(!wants(&s, &status("working"), Some("idle")), "not a default state");
    assert!(wants(&s, &json!({"type": "finished", "data": {}}), None));
    assert!(!wants(&s, &json!({"type": "title", "data": {}}), None));
    let mut chatty = spec(PushFormat::Json);
    chatty.events = vec!["turn".into(), "title".into()];
    chatty.states = vec!["idle".into()];
    assert!(wants(&chatty, &json!({"type": "turn", "data": {"state": "idle"}}), None));
    assert!(!wants(&chatty, &json!({"type": "turn", "data": {"state": "done"}}), None));
    assert!(wants(&chatty, &json!({"type": "title", "data": {}}), None));
    assert!(!wants(&chatty, &status("idle"), None), "status not listed");
}

#[test]
fn a_cleared_status_or_a_removed_terminal_makes_the_next_word_count_again() {
    let s = spec(PushFormat::Json);
    let mut words = Words::default();
    let mut pushed = |event: Value| {
        let was = words.note(&event);
        wants(&s, &event, was.as_deref())
    };
    assert!(pushed(status("blocked")));
    assert!(!pushed(status("blocked")));
    assert!(!pushed(json!({"type": "status", "terminal": "t7", "data": {"status": null}})));
    assert!(pushed(status("blocked")), "blocked, cleared, blocked: pushed twice");
    pushed(json!({"type": "removed", "terminal": "t7", "data": {}}));
    assert!(pushed(status("blocked")), "a removed terminal is forgotten");
}

#[test]
fn a_full_backlog_drops_the_oldest() {
    let mut backlog = VecDeque::new();
    (0..BACKLOG as u64).for_each(|s| assert_eq!(queue(&mut backlog, entry(s, 1)), None));
    assert_eq!(queue(&mut backlog, entry(100, 1)), Some(entry(0, 1)));
    assert_eq!((backlog.len(), backlog.front(), backlog.back()), (BACKLOG, Some(&entry(1, 1)), Some(&entry(100, 1))));
}

#[test]
fn the_envelope_carries_the_event_only_when_it_fits() {
    let event = json!({"type": "finished", "terminal": "rohan/t7", "data": {"exit_code": 0}, "seq": 9, "ms": 5,
        "record": {"name": "review", "screen": "x".repeat(5000)}});
    let entry = Entry { seq: 9, ms: 5, event };
    let big: Value = serde_json::from_slice(&envelope("core1", &entry, JSON_LIMIT)).unwrap();
    assert_eq!(big["event"], entry.event);
    let small: Value = serde_json::from_slice(&envelope("core1", &entry, UNIFIEDPUSH_LIMIT)).unwrap();
    assert_eq!(
        small,
        json!({"v": 1, "core": "core1", "seq": 9, "ms": 5, "type": "finished", "terminal": "rohan/t7",
            "name": "review", "machine": "rohan", "exit": 0})
    );
    let mut huge = entry;
    huge.event["record"]["name"] = "é".repeat(5000).into();
    huge.event["terminal"] = "t".repeat(10_000).into();
    let body = envelope(&"c".repeat(1000), &huge, UNIFIEDPUSH_LIMIT);
    assert!(body.len() < 1024, "summary fields are capped: {}", body.len());
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["name"].as_str().unwrap().len(), 64, "cut at a char boundary");
}

#[test]
fn outcomes_follow_the_distributors() {
    assert_eq!(outcome(Some(200), None), Outcome::Ok);
    assert_eq!(outcome(Some(201), None), Outcome::Ok);
    for gone in [404, 410, 507] {
        assert_eq!(outcome(Some(gone), None), Outcome::Gone, "{gone}");
    }
    assert_eq!(outcome(Some(401), None), Outcome::Rejected);
    assert_eq!(outcome(Some(403), None), Outcome::Rejected);
    assert_eq!(outcome(Some(429), Some(1)), Outcome::Retry(Some(1)));
    assert_eq!(outcome(Some(503), None), Outcome::Retry(None));
    assert_eq!(outcome(None, None), Outcome::Retry(None));
    assert_eq!(outcome(Some(413), None), Outcome::Drop);
    assert_eq!((backoff(1, Some(1)), backoff(1, None), backoff(2, None), backoff(1, Some(9999))), (1, 2, 4, 300));
}

fn entry(seq: u64, ms: u64) -> Entry {
    Entry { seq, ms, event: json!({"type": "status", "seq": seq}) }
}

const KEEP: Keep = Keep { age_ms: 1000, count: 1000, bytes: 1 << 20 };

#[test]
fn retention_drops_by_age_count_and_size() {
    let mut log = Log::default();
    log.append(entry(1, 100));
    log.append(entry(2, 900));
    assert_eq!(log.expire(&KEEP, 1500), 1, "older than age_ms");
    assert_eq!(log.find(2, 900), Some(&entry(2, 900)));
    let mut log = Log::default();
    (1..=5).for_each(|s| drop(log.append(entry(s, 1000))));
    assert_eq!(log.expire(&Keep { count: 3, ..KEEP }, 1000), 2);
    assert!(log.find(3, 1000).is_some() && log.find(2, 1000).is_none());
    let line = log.append(entry(6, 1000)).len();
    assert_eq!(log.expire(&Keep { bytes: 2 * line, ..KEEP }, 1000), 2);
    assert_eq!(log.len(), 2);
    assert_eq!(log.expire(&Keep { age_ms: 0, ..KEEP }, 1000), 2, "zero days keeps nothing");
}

#[test]
fn loading_skips_a_cut_off_line_and_compaction_keeps_the_live_entries() {
    let mut log = Log::default();
    let text: String = (1..=3).map(|s| log.append(entry(s, 10 * s))).collect();
    let crashed = format!("{text}{{\"seq\":4,\"ms\"");
    let mut log = Log::parse(crashed.as_bytes());
    assert_eq!(log.len(), 3);
    assert!(!log.bloated());
    log.expire(&Keep { count: 1, ..KEEP }, 30);
    assert!(log.bloated());
    let rewritten = log.text();
    assert!(!log.bloated());
    assert_eq!(Log::parse(rewritten.as_bytes()).find(3, 30), Some(&entry(3, 30)));
    assert_eq!(Log::parse(rewritten.as_bytes()).len(), 1);
}

#[test]
fn since_follows_log_order_and_falls_back_to_ms() {
    let mut log = Log::default();
    for (seq, ms) in [(1, 500), (2, 400), (3, 600), (1, 700)] {
        log.append(entry(seq, ms));
    }
    let ids = |v: Vec<&Entry>| v.iter().map(|e| (e.seq, e.ms)).collect::<Vec<_>>();
    assert_eq!(ids(log.since(1, 500, 100)), [(2, 400), (3, 600), (1, 700)], "a clock jump back skips nothing");
    assert_eq!(ids(log.since(1, 500, 1)), [(2, 400)]);
    assert_eq!(ids(log.since(9, 550, 100)), [(3, 600), (1, 700)], "expired: from its ms on");
}

#[test]
fn params_are_checked_per_format() {
    let mut up = spec(PushFormat::Unifiedpush);
    assert!(check(&up).unwrap().contains("--p256dh is required"));
    up.p256dh = Some("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into());
    up.auth = Some("BTBZMqHH6r4Tts7J_aSIgg".into());
    assert_eq!(check(&up), None);
    up.secret = Some("s".into());
    assert!(check(&up).unwrap().contains("--secret is only for --format json"));
    let mut json = spec(PushFormat::Json);
    json.auth = Some("x".into());
    assert!(check(&json).unwrap().contains("--auth is only for --format unifiedpush"));
    json.auth = None;
    json.url = "ftp://x/y".into();
    assert!(check(&json).unwrap().contains("http"));
    json.url = "https://x/y".into();
    json.events = vec!["status".into(), "statuss".into()];
    assert_eq!(
        check(&json).unwrap(),
        r#"--event "statuss": not one of created, restarted, lost, finished, removed, keep, status, turn, title, labels, size, foreground, doc"#
    );
    (json.events, json.states) = (vec![], vec!["blocked".into(), "waiting".into()]);
    assert_eq!(check(&json).unwrap(), r#"--state "waiting": not one of idle, working, done, blocked, error"#);
}
