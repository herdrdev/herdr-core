//! Webhooks: a URL plus a filter over events, POSTed by the core even when no
//! client is connected. `format` decides only the body and headers: plain
//! JSON (HMAC signed), or Web Push encrypted (RFC 8291) with VAPID (RFC 8292)
//! for UnifiedPush distributors. This file decides; `send.rs` does the IO.

use crate::api::{PushAddParams, PushFormat};
use crate::status::base64url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

#[cfg(test)]
mod tests;

/// Largest Web Push plaintext: 4096 bytes of record, less its header (86), tag (16) and delimiter.
pub const UNIFIEDPUSH_LIMIT: usize = 3993;
pub const JSON_LIMIT: usize = 64 * 1024;
/// POSTs per event, the first included.
pub const TRIES: u32 = 3;
/// Events waiting for one webhook beyond the one being POSTed: past it the oldest goes (it stays in the push log).
pub const BACKLOG: usize = 100;
const DEFAULT_EVENTS: &[&str] = &["status", "finished", "lost"];
const DEFAULT_STATES: &[&str] = &["blocked", "done", "error"];
/// Every event type the stream carries (schema/README.md).
const EVENTS: &[&str] = &[
    "created",
    "restarted",
    "lost",
    "finished",
    "removed",
    "keep",
    "status",
    "turn",
    "title",
    "labels",
    "size",
    "foreground",
    "doc",
];
/// Every status word OSC 7501 has.
const STATES: &[&str] = &["idle", "working", "done", "blocked", "error"];
/// Summary fields are cut to this, so the envelope without `event` always fits.
const FIELD: usize = 64;

/// The status word an event carries (status and turn events).
pub fn word(event: &Value) -> Option<&str> {
    match event["type"].as_str()? {
        "status" => event["data"]["status"]["state"].as_str(),
        "turn" => event["data"]["state"].as_str(),
        _ => None,
    }
}

/// Whether `event` is pushed to a webhook. `was`: the word this webhook last saw
/// for that terminal, since a status event counts only when its word changes.
pub fn wants(spec: &PushAddParams, event: &Value, was: Option<&str>) -> bool {
    let has = |list: &[String], defaults: &[&str], x: &str| {
        if list.is_empty() { defaults.contains(&x) } else { list.iter().any(|l| l == x) }
    };
    let kind = event["type"].as_str().unwrap_or_default();
    if !has(&spec.events, DEFAULT_EVENTS, kind) {
        return false;
    }
    match (kind, word(event)) {
        ("status", w) => w.is_some_and(|w| has(&spec.states, DEFAULT_STATES, w)) && w != was,
        ("turn", w) => w.is_some_and(|w| has(&spec.states, DEFAULT_STATES, w)),
        _ => true,
    }
}

/// The word one webhook last saw per terminal, so a status event counts only when its word changes.
#[derive(Default)]
pub struct Words(HashMap<String, String>);

impl Words {
    /// Notes `event`; returns the terminal's word before it. A cleared status
    /// forgets the word, so the next one counts again; so does a removed terminal.
    pub fn note(&mut self, event: &Value) -> Option<String> {
        let terminal = event["terminal"].as_str().unwrap_or_default();
        match (event["type"].as_str(), word(event)) {
            (Some("status"), Some(w)) => self.0.insert(terminal.into(), w.into()),
            (Some("status" | "removed"), _) => self.0.remove(terminal),
            _ => self.0.get(terminal).cloned(),
        }
    }
}

/// Queues `entry` for one webhook; returns the oldest when that passes `BACKLOG`.
pub fn queue(backlog: &mut VecDeque<Entry>, entry: Entry) -> Option<Entry> {
    backlog.push_back(entry);
    (backlog.len() > BACKLOG).then(|| backlog.pop_front()).flatten()
}

/// One pushed event, as the push log keeps it: `(seq, ms)` is its id on this core.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub ms: u64,
    pub event: Value,
}

fn cut(s: &str) -> &str {
    let mut end = s.len().min(FIELD);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The body both formats carry: a summary a phone can always show, plus the
/// whole event when it fits in `limit` (otherwise fetch it by `seq` and `ms`).
pub fn envelope(core: &str, entry: &Entry, limit: usize) -> Vec<u8> {
    let e = &entry.event;
    let mut v = json!({"v": 1, "core": cut(core), "seq": entry.seq, "ms": entry.ms});
    let mut put = |key: &str, value: Option<&str>| {
        if let Some(value) = value {
            v[key] = cut(value).into();
        }
    };
    let terminal = e["terminal"].as_str();
    put("type", e["type"].as_str());
    put("terminal", terminal);
    put("name", e["record"]["name"].as_str());
    put("machine", e["machine"].as_str().or_else(|| terminal.and_then(|t| t.split_once('/')).map(|(m, _)| m)));
    put("state", word(e));
    if e["type"] == "finished" && e["data"]["exit_code"].is_i64() {
        v["exit"] = e["data"]["exit_code"].clone();
    }
    let summary = v.clone();
    v["event"] = e.clone();
    let full = serde_json::to_vec(&v).expect("json");
    if full.len() <= limit { full } else { serde_json::to_vec(&summary).expect("json") }
}

/// What an HTTP status means for a webhook.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Ok,
    /// The endpoint is gone: stop sending to it.
    Gone,
    /// The push service refused our VAPID signature (NextPush: missing or another key).
    Rejected,
    /// Try again, after this many seconds when the server said.
    Retry(Option<u64>),
    /// Refused for this event (413 and other 4xx): recorded, not retried.
    Drop,
}

/// `status`: none when nothing answered.
pub fn outcome(status: Option<u16>, retry_after: Option<u64>) -> Outcome {
    match status {
        Some(200..=299) => Outcome::Ok,
        // ntfy answers 507 for a topic nobody subscribes to: retrying counts toward its ban list.
        Some(404 | 410 | 507) => Outcome::Gone,
        Some(401 | 403) => Outcome::Rejected,
        Some(429 | 500..=599) | None => Outcome::Retry(retry_after),
        Some(_) => Outcome::Drop,
    }
}

impl Outcome {
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Gone => "gone",
            Outcome::Rejected => "rejected",
            Outcome::Retry(_) => "retry",
            Outcome::Drop => "dropped",
        }
    }
}

/// Seconds before try `n + 1` (n from 1): what the server asked, else 2, 4...; at most 5 minutes.
pub fn backoff(n: u32, retry_after: Option<u64>) -> u64 {
    retry_after.unwrap_or(1 << n.min(8)).min(300)
}

/// How long the push log keeps entries: an entry goes once any limit is passed.
#[derive(Clone, Copy, Debug)]
pub struct Keep {
    /// Zero keeps nothing.
    pub age_ms: u64,
    pub count: usize,
    pub bytes: usize,
}

/// The push log: every pushed event, in the order they were appended, one JSON line each.
#[derive(Default)]
pub struct Log {
    entries: VecDeque<(Entry, usize)>,
    live: usize,
    /// Bytes in the file, dropped entries included, until it is rewritten.
    pub file: usize,
}

impl Log {
    /// The file's lines; a line cut off by a crash mid-write is skipped.
    pub fn parse(bytes: &[u8]) -> Log {
        let mut log = Log { file: bytes.len(), ..Default::default() };
        for line in bytes.split(|b| *b == b'\n') {
            if let Ok(entry) = serde_json::from_slice::<Entry>(line) {
                log.live += line.len() + 1;
                log.entries.push_back((entry, line.len() + 1));
            }
        }
        log
    }

    /// Adds an entry; returns the line to append to the file.
    pub fn append(&mut self, entry: Entry) -> String {
        let line = format!("{}\n", serde_json::to_string(&entry).expect("json"));
        self.live += line.len();
        self.file += line.len();
        self.entries.push_back((entry, line.len()));
        line
    }

    /// Drops the oldest entries past any limit; returns how many went.
    pub fn expire(&mut self, keep: &Keep, now: u64) -> usize {
        let before = self.entries.len();
        while let Some((front, size)) = self.entries.front() {
            let old = keep.age_ms == 0 || now.saturating_sub(front.ms) > keep.age_ms;
            if !(old || self.entries.len() > keep.count || self.live > keep.bytes) {
                break;
            }
            self.live -= size;
            self.entries.pop_front();
        }
        before - self.entries.len()
    }

    /// The file holds more than twice what is live: time to rewrite it.
    pub fn bloated(&self) -> bool {
        self.file > 2 * self.live
    }

    /// The live entries as the file's new contents.
    pub fn text(&mut self) -> String {
        self.file = self.live;
        self.entries.iter().map(|(e, _)| format!("{}\n", serde_json::to_string(e).expect("json"))).collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn find(&self, seq: u64, ms: u64) -> Option<&Entry> {
        self.entries.iter().map(|(e, _)| e).find(|e| e.seq == seq && e.ms == ms)
    }

    /// Entries after `(seq, ms)` in log order; once it has expired, those from its `ms` on.
    pub fn since(&self, seq: u64, ms: u64, limit: usize) -> Vec<&Entry> {
        let all = self.entries.iter().map(|(e, _)| e);
        match self.entries.iter().position(|(e, _)| e.seq == seq && e.ms == ms) {
            Some(i) => all.skip(i + 1).take(limit).collect(),
            None => all.filter(|e| e.ms >= ms).take(limit).collect(),
        }
    }
}

/// Why a webhook's params can't work; `None` when they can.
pub fn check(spec: &PushAddParams) -> Option<String> {
    let scheme =
        ["http://", "https://"].iter().any(|s| spec.url.get(..s.len()).is_some_and(|p| p.eq_ignore_ascii_case(s)));
    if !scheme || origin(&spec.url).is_none() {
        return Some(format!("url {:?}: give an http:// or https:// address", spec.url));
    }
    if spec.url.chars().any(|c| c.is_whitespace() || c.is_control() || c == '"') {
        return Some("url: no spaces, quotes or control characters".into());
    }
    let known = |given: &[String], valid: &[&str], flag: &str| {
        let wrong = given.iter().find(|g| !valid.contains(&g.as_str()))?;
        Some(format!("--{flag} {wrong:?}: not one of {}", valid.join(", ")))
    };
    if let Some(why) = known(&spec.events, EVENTS, "event").or_else(|| known(&spec.states, STATES, "state")) {
        return Some(why);
    }
    let unifiedpush = spec.format == PushFormat::Unifiedpush;
    let field = |name: &str, given: bool, wanted: bool| {
        (given != wanted).then(|| match wanted {
            true => format!("--{name} is required with --format unifiedpush"),
            false => format!("--{name} is only for --format {}", if unifiedpush { "json" } else { "unifiedpush" }),
        })
    };
    field("secret", spec.secret.is_some() && unifiedpush, false)
        .or_else(|| field("p256dh", spec.p256dh.is_some(), unifiedpush))
        .or_else(|| field("auth", spec.auth.is_some(), unifiedpush))
        .or_else(|| field("contact", spec.contact.is_some() && !unifiedpush, false))
        .or_else(|| {
            let key = spec.p256dh.as_deref().map(crate::status::base64url_decode);
            (unifiedpush && key.flatten().is_none_or(|k| encrypt(&[], &k, &[0; 16]).is_none()))
                .then(|| "--p256dh: give the receiver's P-256 public key, base64url".into())
        })
        .or_else(|| {
            let auth = spec.auth.as_deref().map(crate::status::base64url_decode);
            (unifiedpush && auth.flatten().is_none_or(|a| a.len() != 16))
                .then(|| "--auth: give the receiver's 16-byte auth secret, base64url".into())
        })
}

/// `scheme://host[:port]` of a URL: the VAPID audience.
pub fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    (!host.is_empty()).then(|| format!("{}://{}", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

/// `X-Hc-Signature`'s hex: HMAC-SHA256 over `<timestamp>.<body>`, so a captured request can't be replayed later.
pub fn sign(secret: &str, timestamp: u64, body: &[u8]) -> String {
    use ring::hmac;
    let mut mac = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes()));
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    mac.sign().as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

struct Len(usize);

impl ring::hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf<const N: usize>(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; N] {
    use ring::hkdf::{HKDF_SHA256, Salt};
    let mut out = [0u8; N];
    let prk = Salt::new(HKDF_SHA256, salt).extract(ikm);
    prk.expand(&[info], Len(N)).and_then(|okm| okm.fill(&mut out)).expect("short output");
    out
}

/// RFC 8291's key schedule: the content key and nonce from the ECDH secret,
/// both public keys (receiver's, then sender's), the auth secret and the salt.
pub fn keys(shared: &[u8], receiver: &[u8], sender: &[u8], auth: &[u8], salt: &[u8]) -> ([u8; 16], [u8; 12]) {
    let ikm: [u8; 32] = hkdf(auth, shared, &[b"WebPush: info\0", receiver, sender].concat());
    (hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0"), hkdf(salt, &ikm, b"Content-Encoding: nonce\0"))
}

/// One `aes128gcm` record once the keys are agreed: all of RFC 8291 but the ECDH.
pub fn seal(plain: &[u8], shared: &[u8], receiver: &[u8], sender: &[u8], auth: &[u8], salt: [u8; 16]) -> Vec<u8> {
    use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
    let (cek, nonce) = keys(shared, receiver, sender, auth, &salt);
    let cipher = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, &cek).expect("16 bytes"));
    let mut sealed = [plain, &[2]].concat();
    cipher
        .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut sealed)
        .expect("one record");
    [&salt, &4096u32.to_be_bytes()[..], &[sender.len() as u8], sender, &sealed].concat()
}

/// RFC 8291 `aes128gcm` for `p256dh`, with a fresh key and salt; none when it is not an uncompressed P-256 point.
pub fn encrypt(plain: &[u8], p256dh: &[u8], auth: &[u8]) -> Option<Vec<u8>> {
    use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
    let eph = EphemeralPrivateKey::generate(&ECDH_P256, &ring::rand::SystemRandom::new()).ok()?;
    let ours = eph.compute_public_key().ok()?;
    let mut salt = [0u8; 16];
    crate::platform::random_bytes(&mut salt);
    let receiver = UnparsedPublicKey::new(&ECDH_P256, p256dh);
    agree_ephemeral(eph, &receiver, |shared| seal(plain, shared, p256dh, ours.as_ref(), auth, salt)).ok()
}

/// `k=` in the VAPID header and what `hc push key` prints: the public key, unpadded.
pub fn vapid_key(key: &ring::signature::EcdsaKeyPair) -> String {
    use ring::signature::KeyPair;
    base64url(key.public_key().as_ref())
}

/// The VAPID JWT (ES256) for one push service: `aud` its origin, `exp` in seconds.
pub fn vapid_jwt(key: &ring::signature::EcdsaKeyPair, aud: &str, exp: u64, sub: Option<&str>) -> String {
    let mut claims = json!({"aud": aud, "exp": exp});
    if let Some(sub) = sub {
        claims["sub"] = sub.into();
    }
    let head = base64url(br#"{"typ":"JWT","alg":"ES256"}"#);
    let signed = format!("{head}.{}", base64url(claims.to_string().as_bytes()));
    let signature = key.sign(&ring::rand::SystemRandom::new(), signed.as_bytes()).expect("ecdsa");
    format!("{signed}.{}", base64url(signature.as_ref()))
}
