//! Webhook params and results: `push.*`.

use super::*;

#[derive(Clone, Copy, PartialEq, Debug, Default, clap::ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PushFormat {
    /// Plain JSON, signed when it has a secret.
    #[default]
    Json,
    /// Web Push (RFC 8291) encrypted, VAPID signed: ntfy, NextPush.
    Unifiedpush,
}

/// A webhook: where to POST which events. These are also its CLI flags.
#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Clone, PartialEq)]
pub struct PushAddParams {
    /// Its name here. Adding a name again replaces it (a new endpoint).
    pub name: String,
    /// Where to POST, kept byte for byte (`https://ntfy.sh/upAbc?up=1`). http or https.
    #[arg(long)]
    pub url: String,
    #[arg(long, value_enum, default_value = "json")]
    #[serde(default)]
    pub format: PushFormat,
    /// Event types that count (repeatable; default status, finished, lost).
    #[arg(long = "event", value_name = "TYPE")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    /// Status words that count for status and turn events (repeatable;
    /// default blocked, done, error). A status event counts when its word changes.
    #[arg(long = "state", value_name = "WORD")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    /// Only this terminal (`t7`, `rohan/t7`).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    /// Only children of this terminal.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children_of: Option<String>,
    /// Only terminals carrying all of these labels, exactly (repeatable).
    #[arg(long = "label", value_name = "K=V")]
    #[serde(default, with = "label_map", skip_serializing_if = "Vec::is_empty")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub labels: Vec<Label>,
    /// json: sign every POST (`X-Hc-Signature: sha256=<hex hmac of "<X-Hc-Timestamp>.<body>">`).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// unifiedpush: the receiver's public key (base64url), required.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p256dh: Option<String>,
    /// unifiedpush: the receiver's auth secret (base64url), required.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// unifiedpush: who runs this core, for the push service (`mailto:you@example.com`).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<String>,
}

/// What the last POSTs to a webhook observed.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default, Debug, PartialEq)]
pub struct PushSeen {
    /// When a POST last succeeded (2xx).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_ms: Option<u64>,
    /// The HTTP status the last POST got; none when it got no answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<u16>,
    /// Why the last POST failed; cleared by a success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The endpoint said it is gone (404, 410, 507): nothing more is sent until it is added again.
    #[serde(default)]
    pub gone: bool,
}

/// A webhook as kept. Listed without `secret` and `auth`.
#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct PushHook {
    #[serde(flatten)]
    pub spec: PushAddParams,
    #[serde(flatten)]
    pub seen: PushSeen,
}

#[derive(Serialize, JsonSchema)]
pub struct PushListResult {
    pub webhooks: Vec<PushHook>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PushNameParams {
    /// The webhook's name.
    pub name: String,
}

#[derive(Serialize, JsonSchema)]
pub struct PushTestResult {
    /// The HTTP status that came back; none when nothing answered.
    pub status: Option<u16>,
    /// ok, gone, rejected, retry or dropped.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// An event's id on this core: its events-stream seq and when it happened.
/// On the command line `SEQ:MS`.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq)]
pub struct PushEventId {
    pub seq: u64,
    pub ms: u64,
}

impl std::str::FromStr for PushEventId {
    type Err = String;
    fn from_str(s: &str) -> Result<PushEventId, String> {
        s.split_once(':')
            .and_then(|(seq, ms)| Some(PushEventId { seq: seq.parse().ok()?, ms: ms.parse().ok()? }))
            .ok_or_else(|| format!("expected SEQ:MS, got {s:?}"))
    }
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PushEventsParams {
    /// One pushed event: its seq (with --ms).
    #[arg(long, requires = "ms", conflicts_with = "since")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// One pushed event: its ms (with --seq).
    #[arg(long, requires = "seq")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    /// Every pushed event after this one, in the order they happened (`SEQ:MS`).
    /// When it has expired: those from its ms on.
    #[arg(long, value_name = "SEQ:MS")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<PushEventId>,
    /// At most this many (default 100).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct PushEventsResult {
    /// Each `{seq, ms, event}`: `event` as events.subscribe delivers it, with
    /// `ms`, and the terminal's `record` at that moment.
    pub events: Vec<serde_json::Value>,
}

#[derive(Serialize, JsonSchema)]
pub struct PushKeyResult {
    /// This core's VAPID public key, base64url without padding (87 characters):
    /// give it to `UnifiedPush.register(..., vapid)`.
    pub key: String,
}
