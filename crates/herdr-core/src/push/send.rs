//! Webhook IO: `push.json`, `vapid.json` and `push-log.jsonl` in the state
//! folder (only this user can read them), the sender task, and the POSTs.
//! ureq does HTTP, rustls TLS against the system's certificates.

use super::*;
use crate::api::{PushHook, PushSeen};
use crate::events::{Filter, Sink};
use crate::paths::write_private;
use crate::runtime::{Core, now_ms};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING as ES256, EcdsaKeyPair};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

const HOOKS: &str = "push.json";
const VAPID: &str = "vapid.json";
const LOG: &str = "push-log.jsonl";
/// The push log is swept this often, events or not.
const SWEEP: Duration = Duration::from_secs(3600);
/// A VAPID JWT lives this long; one with less than an hour left is made again.
const JWT_LIFE: u64 = 12 * 3600;

pub enum Msg {
    /// An event a webhook's filter matched: `event` carries `seq`, `ms` and `record`.
    Event { hook: String, event: Value },
    /// Apply retention now (every 1000th event).
    Sweep,
}

/// The core's webhooks. Each lock is held briefly and never with the core's.
pub struct Push {
    pub tx: mpsc::UnboundedSender<Msg>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Msg>>>,
    pub hooks: Mutex<BTreeMap<String, PushHook>>,
    pub log: Mutex<Log>,
    vapid: Mutex<Option<Arc<EcdsaKeyPair>>>,
    jwts: Mutex<HashMap<Audience, (String, u64)>>,
    dir: PathBuf,
    /// What POSTs observed changed since `push.json` was written: `save_when_due` writes it shortly.
    dirty: AtomicBool,
    save_due: Notify,
    /// Held while `push.json` is written, taken with `hooks` held: writes go in the order of their snapshots.
    writing: Mutex<()>,
}

/// A push service origin and the contact (`sub`): a VAPID JWT is cached per both, with its expiry.
type Audience = (String, Option<String>);

/// What the push log's file needs after a change.
enum Disk {
    Append(String),
    Rewrite(String),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A file that can't be read is moved aside to `.bad`, as documents are.
fn read_or_quarantine<T: serde::de::DeserializeOwned + Default>(dir: &Path, name: &str) -> T {
    let path = dir.join(name);
    crate::paths::read_state(&path).unwrap_or_else(|e| {
        eprintln!("core: cannot load {name}, starting empty: {e}");
        if let Err(e) = std::fs::rename(&path, dir.join(format!("{name}.bad"))) {
            eprintln!("core: cannot quarantine {name}: {e}");
        }
        T::default()
    })
}

pub fn keep(settings: &crate::config::Config) -> Keep {
    Keep { age_ms: settings.push_retention().as_millis() as u64, count: 1000, bytes: settings.push_log_bytes() }
}

impl Push {
    /// The webhooks and the push log as saved, the log already compacted.
    pub fn load(dir: &Path, keep: Keep) -> Push {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut log = Log::parse(&std::fs::read(dir.join(LOG)).unwrap_or_default());
        log.expire(&keep, now_ms());
        let push = Push {
            tx,
            rx: Mutex::new(Some(rx)),
            hooks: Mutex::new(read_or_quarantine(dir, HOOKS)),
            log: Mutex::new(Log::default()),
            vapid: Mutex::new(None),
            jwts: Default::default(),
            dir: dir.into(),
            dirty: Default::default(),
            save_due: Notify::new(),
            writing: Default::default(),
        };
        if log.file > 0 {
            push.write(Disk::Rewrite(log.text()));
        }
        *lock(&push.log) = log;
        push
    }

    /// Brings the push log's file up to date: blocking IO, kept off the runtime thread.
    fn write(&self, disk: Disk) {
        let path = self.dir.join(LOG);
        match disk {
            Disk::Rewrite(text) => {
                if let Err(e) = write_private(&path, text.as_bytes()) {
                    eprintln!("core: cannot rewrite the push log: {e}");
                }
            }
            Disk::Append(line) => {
                let written = match std::fs::OpenOptions::new().append(true).open(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::platform::private_file(&path),
                    other => other,
                }
                .and_then(|mut f| f.write_all(line.as_bytes()).and_then(|()| f.flush()));
                if let Err(e) = written {
                    eprintln!("core: cannot append to the push log: {e}");
                }
            }
        }
    }

    /// Writes `push.json` now (adding and removing a webhook). Call with `hooks` locked.
    pub fn save(&self, hooks: &BTreeMap<String, PushHook>) -> std::io::Result<()> {
        let _writing = lock(&self.writing);
        self.dirty.store(false, Ordering::SeqCst);
        write_private(&self.dir.join(HOOKS), &serde_json::to_vec(hooks).expect("json"))
    }

    /// Writes `push.json` if POSTs observed something since it was last written. Blocking IO.
    pub fn flush(&self) {
        let (bytes, _writing) = {
            let hooks = lock(&self.hooks);
            if !self.dirty.swap(false, Ordering::SeqCst) {
                return;
            }
            (serde_json::to_vec(&*hooks).expect("json"), lock(&self.writing))
        };
        if let Err(e) = write_private(&self.dir.join(HOOKS), &bytes) {
            eprintln!("core: cannot save webhooks: {e}");
        }
    }

    /// Saves what a POST to the webhook `spec` observed (written within `SAVE_SOON`);
    /// false when that webhook was removed or replaced meanwhile. Once `gone`, it
    /// stays gone until added again, whatever a later POST or test sees.
    pub fn record(&self, spec: &PushAddParams, seen: PushSeen) -> bool {
        let mut hooks = lock(&self.hooks);
        let Some(h) = hooks.get_mut(&spec.name).filter(|h| h.spec == *spec) else { return false };
        let ok = seen.last_ok_ms.is_some();
        h.seen = PushSeen { last_ok_ms: seen.last_ok_ms.or(h.seen.last_ok_ms), gone: seen.gone || h.seen.gone, ..seen };
        if !ok || h.seen.last_error.is_some() {
            eprintln!("core: webhook {}: {}", spec.name, h.seen.last_error.as_deref().unwrap_or("failed"));
        }
        self.dirty.store(true, Ordering::SeqCst);
        self.save_due.notify_one();
        true
    }

    /// This core's VAPID key, made the first time it is needed.
    pub fn vapid(&self) -> std::io::Result<Arc<EcdsaKeyPair>> {
        let mut vapid = lock(&self.vapid);
        if let Some(key) = vapid.as_ref() {
            return Ok(key.clone());
        }
        let (path, rng) = (self.dir.join(VAPID), ring::rand::SystemRandom::new());
        let saved: BTreeMap<String, String> = crate::paths::read_state(&path)?;
        let pkcs8 = match saved.get("pkcs8").and_then(|k| crate::status::base64url_decode(k)) {
            Some(pkcs8) => pkcs8,
            None => {
                let made =
                    EcdsaKeyPair::generate_pkcs8(&ES256, &rng).map_err(|e| std::io::Error::other(e.to_string()))?;
                write_private(&path, json!({"pkcs8": base64url(made.as_ref())}).to_string().as_bytes())?;
                made.as_ref().to_vec()
            }
        };
        let key = EcdsaKeyPair::from_pkcs8(&ES256, &pkcs8, &rng).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(vapid.insert(Arc::new(key)).clone())
    }

    fn jwt(&self, key: &EcdsaKeyPair, origin: &str, sub: Option<&str>) -> String {
        let now = now_ms() / 1000;
        let mut jwts = lock(&self.jwts);
        let cached = (origin.to_string(), sub.map(String::from));
        match jwts.get(&cached) {
            Some((jwt, exp)) if *exp > now + 3600 => jwt.clone(),
            _ => {
                let jwt = vapid_jwt(key, origin, now + JWT_LIFE, sub);
                jwts.insert(cached, (jwt.clone(), now + JWT_LIFE));
                jwt
            }
        }
    }

    /// Adds a log entry, then applies retention; returns what the file needs
    /// (rewritten when it grew to twice what is live).
    fn append(&self, entry: Entry, keep: &Keep) -> Option<Disk> {
        if keep.age_ms == 0 {
            return None;
        }
        let mut log = lock(&self.log);
        let line = log.append(entry);
        log.expire(keep, now_ms());
        Some(if log.bloated() { Disk::Rewrite(log.text()) } else { Disk::Append(line) })
    }

    /// Retention without a new event: expired entries leave the file.
    fn sweep(&self, keep: &Keep, now: u64) -> Option<Disk> {
        let mut log = lock(&self.log);
        (log.expire(keep, now) > 0).then(|| Disk::Rewrite(log.text()))
    }
}

/// Does `disk` on a blocking thread; the push log is written only from `run`, so in order.
async fn write_log(core: &Arc<Core>, disk: Option<Disk>) {
    let Some(disk) = disk else { return };
    let core = core.clone();
    let _ = tokio::task::spawn_blocking(move || core.push.write(disk)).await;
}

/// Writes what POSTs observed, once for a burst, off the runtime thread.
async fn save_when_due(core: Arc<Core>) {
    loop {
        core.push.save_due.notified().await;
        tokio::time::sleep(crate::runtime::SAVE_SOON).await;
        let core = core.clone();
        let _ = tokio::task::spawn_blocking(move || core.push.flush()).await;
    }
}

pub fn filter(spec: &PushAddParams) -> Filter {
    Filter {
        terminal: spec.terminal.clone(),
        children_of: spec.children_of.clone(),
        labels: crate::api::labels(&spec.labels),
        here_only: false,
    }
}

/// Starts (or restarts) hearing events for a webhook. Call with the core's lock.
pub fn subscribe(core: &Core, st: &mut crate::runtime::State, spec: &PushAddParams) {
    core.unsubscribe_push(st, &spec.name);
    let sink = Sink::Push { name: spec.name.clone(), tx: core.push.tx.clone() };
    core.subscribe(st, filter(spec), sink);
}

/// One webhook's sender: its events wait in `backlog` and are POSTed one at a
/// time, in order. Dropping it stops the sender, mid-POST included.
struct Lane {
    spec: PushAddParams,
    words: Words,
    backlog: Arc<Mutex<VecDeque<Entry>>>,
    wake: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Lane {
    fn start(core: &Arc<Core>, spec: PushAddParams) -> Lane {
        let (backlog, wake) = (Arc::<Mutex<VecDeque<Entry>>>::default(), Arc::new(Notify::new()));
        let task = tokio::spawn(send_all(core.clone(), spec.clone(), backlog.clone(), wake.clone()));
        Lane { spec, words: Words::default(), backlog, wake, task }
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The sender: every saved webhook is subscribed, then events are logged and
/// queued per webhook, and the log is swept hourly and on every 1000th event.
pub async fn run(core: Arc<Core>) {
    let Some(mut rx) = lock(&core.push.rx).take() else { return };
    {
        let hooks = lock(&core.push.hooks).clone();
        let mut st = core.lock();
        hooks.values().for_each(|h| subscribe(&core, &mut st, &h.spec));
    }
    tokio::spawn(save_when_due(core.clone()));
    let mut tick = tokio::time::interval(SWEEP);
    tick.tick().await; // the first is now: loading just compacted
    let mut lanes: HashMap<String, Lane> = HashMap::new();
    let mut logged = None;
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                None => return,
                Some(Msg::Sweep) => write_log(&core, core.push.sweep(&keep(&core.settings()), now_ms())).await,
                Some(Msg::Event { hook, event }) => {
                    let h = {
                        // A removed, replaced or gone webhook's lane goes, with what it remembered.
                        let hooks = lock(&core.push.hooks);
                        lanes.retain(|name, lane| {
                            !lane.task.is_finished() && hooks.get(name).is_some_and(|h| h.spec == lane.spec)
                        });
                        hooks.get(&hook).filter(|h| !h.seen.gone).cloned()
                    };
                    let Some(h) = h else { continue };
                    let lane = lanes.entry(hook).or_insert_with(|| Lane::start(&core, h.spec.clone()));
                    let was = lane.words.note(&event);
                    if !wants(&h.spec, &event, was.as_deref()) {
                        continue;
                    }
                    let entry = Entry { seq: event["seq"].as_u64().unwrap_or(0), ms: event["ms"].as_u64().unwrap_or(0), event };
                    if let Some(old) = queue(&mut lock(&lane.backlog), entry.clone()) {
                        eprintln!("core: webhook {}: over {BACKLOG} events behind, skipped {}:{} (kept in the push log)", h.spec.name, old.seq, old.ms);
                    }
                    lane.wake.notify_one();
                    // Several webhooks hear the same event: it is logged once. The entry is
                    // in the log before the sender runs: the write below only follows it to disk.
                    if logged != Some((entry.seq, entry.ms)) {
                        logged = Some((entry.seq, entry.ms));
                        write_log(&core, core.push.append(entry, &keep(&core.settings()))).await;
                    }
                }
            },
            _ = tick.tick() => write_log(&core, core.push.sweep(&keep(&core.settings()), now_ms())).await,
        }
    }
}

/// One webhook's events, oldest first; ends when the webhook is removed, replaced or gone.
async fn send_all(core: Arc<Core>, spec: PushAddParams, backlog: Arc<Mutex<VecDeque<Entry>>>, wake: Arc<Notify>) {
    loop {
        let next = lock(&backlog).pop_front();
        let Some(entry) = next else {
            wake.notified().await;
            continue;
        };
        let hook = lock(&core.push.hooks).get(&spec.name).filter(|h| h.spec == spec && !h.seen.gone).cloned();
        let Some(hook) = hook else { return };
        deliver(&core, &hook, &entry).await;
    }
}

/// POSTs one event, retrying what may pass later, and records what came back.
async fn deliver(core: &Arc<Core>, hook: &PushHook, entry: &Entry) {
    for n in 1..=TRIES {
        let (seen, outcome) = post(core, hook, entry).await;
        let retry = match outcome {
            Outcome::Retry(after) if n < TRIES => Some(backoff(n, after)),
            _ => None,
        };
        if !core.push.record(&hook.spec, seen) {
            return;
        }
        match retry {
            Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
            None => return,
        }
    }
}

/// One POST of `entry` and what it observed.
pub async fn post(core: &Arc<Core>, hook: &PushHook, entry: &Entry) -> (PushSeen, Outcome) {
    let request = match request(core, &hook.spec, entry) {
        Ok(r) => r,
        Err(e) => return (PushSeen { last_error: Some(e), ..Default::default() }, Outcome::Drop),
    };
    let answer = tokio::task::spawn_blocking(move || http(&request)).await;
    let (status, retry_after, error) = answer.unwrap_or_else(|e| (None, None, Some(e.to_string())));
    let outcome = outcome(status, retry_after);
    let unifiedpush = hook.spec.format == PushFormat::Unifiedpush;
    let error = match (&outcome, status) {
        (Outcome::Ok, _) => None,
        (Outcome::Rejected, _) if unifiedpush => Some("vapid rejected".into()),
        (_, Some(code)) => Some(format!("{} (HTTP {code})", outcome.word())),
        (_, None) => Some(error.unwrap_or_else(|| "no answer".into())),
    };
    let seen = PushSeen {
        last_ok_ms: (outcome == Outcome::Ok).then(now_ms),
        last_status: status,
        last_error: error,
        gone: outcome == Outcome::Gone,
    };
    (seen, outcome)
}

pub struct Request {
    url: String,
    headers: Vec<String>,
    body: Vec<u8>,
}

fn request(core: &Core, spec: &PushAddParams, entry: &Entry) -> Result<Request, String> {
    let mut headers = Vec::new();
    let body = match spec.format {
        PushFormat::Json => {
            let body = envelope(&core.info.id, entry, JSON_LIMIT);
            headers.push("Content-Type: application/json".into());
            if let Some(secret) = &spec.secret {
                let ts = now_ms() / 1000;
                headers.push(format!("X-Hc-Timestamp: {ts}"));
                headers.push(format!("X-Hc-Signature: sha256={}", sign(secret, ts, &body)));
            }
            body
        }
        PushFormat::Unifiedpush => {
            let plain = envelope(&core.info.id, entry, UNIFIEDPUSH_LIMIT);
            let decode =
                |k: &Option<String>| k.as_deref().and_then(crate::status::base64url_decode).unwrap_or_default();
            let body = encrypt(&plain, &decode(&spec.p256dh), &decode(&spec.auth))
                .ok_or("cannot encrypt for this receiver's key")?;
            let key = core.push.vapid().map_err(|e| format!("cannot load the VAPID key: {e}"))?;
            let origin = origin(&spec.url).ok_or("bad url")?;
            let jwt = core.push.jwt(&key, &origin, spec.contact.as_deref());
            let urgent = matches!(word(&entry.event), Some("blocked" | "error"));
            headers.extend([
                "Content-Encoding: aes128gcm".into(),
                "Content-Type: application/octet-stream".into(),
                "TTL: 86400".into(),
                format!("Urgency: {}", if urgent { "high" } else { "normal" }),
                format!("Authorization: vapid t={jwt}, k={}", vapid_key(&key)),
            ]);
            body
        }
    };
    Ok(Request { url: spec.url.clone(), headers, body })
}

/// One HTTP client for every webhook: rustls (ring) against the system's
/// certificates, loaded once. Redirects are not followed, any status is an answer.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        use ureq::tls::{Certificate, RootCerts, TlsConfig, TlsProvider};
        let found = rustls_native_certs::load_native_certs();
        if found.certs.is_empty() {
            eprintln!("core: no system certificates found for webhooks: {:?}", found.errors);
        }
        let roots: Vec<_> = found.certs.iter().map(|c| Certificate::from_der(c.as_ref()).to_owned()).collect();
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::new_with_certs(&roots))
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into()
    })
}

/// POSTs `r`: the HTTP status, Retry-After seconds, and why nothing answered.
fn http(r: &Request) -> (Option<u16>, Option<u64>, Option<String>) {
    let mut post = agent().post(&r.url);
    for h in &r.headers {
        let (name, value) = h.split_once(": ").expect("Name: value");
        post = post.header(name, value);
    }
    match post.send(&r.body[..]) {
        Ok(answer) => {
            let retry_after = answer.headers().get("retry-after").and_then(|v| v.to_str().ok()?.trim().parse().ok());
            (Some(answer.status().as_u16()), retry_after, None)
        }
        Err(e) => (None, None, Some(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sweep_rewrites_the_log_only_when_something_expired() {
        let dir = std::env::temp_dir().join(format!("hc-push-{}", crate::platform::random_hex()));
        crate::platform::private_dir(&dir).unwrap();
        let keep = Keep { age_ms: 3_600_000, count: 1000, bytes: 1 << 20 };
        let push = Push::load(&dir, keep);
        let now = now_ms();
        let write = |disk: Option<Disk>| disk.into_iter().for_each(|d| push.write(d));
        write(push.append(Entry { seq: 1, ms: now, event: json!({"type": "status"}) }, &keep));
        write(push.append(Entry { seq: 2, ms: now + 500, event: json!({"type": "status"}) }, &keep));
        let file = dir.join(LOG);
        let modified = || std::fs::metadata(&file).unwrap().modified().unwrap();
        let before = (std::fs::read(&file).unwrap(), modified());
        assert!(push.sweep(&keep, now + 3_600_000).is_none(), "nothing expired: untouched");
        assert_eq!((std::fs::read(&file).unwrap(), modified()), before);
        write(push.sweep(&keep, now + 3_600_200));
        let left = Log::parse(&std::fs::read(&file).unwrap());
        assert_eq!((left.len(), left.find(2, now + 500).is_some()), (1, true));
        let reloaded = Push::load(&dir, keep);
        assert!(lock(&reloaded.log).find(2, now + 500).is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
