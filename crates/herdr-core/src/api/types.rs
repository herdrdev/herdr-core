//! Every method's params and results.

use crate::rpc::RpcError;
use crate::terminals::Public;
use crate::wait::WaitResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct CoreInfo {
    /// Permanent random id, reset only when state is wiped.
    pub id: String,
    /// Display name, the hostname by default.
    pub name: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct Empty {}

// Request types below are both the API's params (serde, schema) and the CLI's
// arguments (clap): one definition, one doc comment for `--help` and schema.
// `#[arg(skip)]` fields are filled in by hc itself.

/// A `key=value` label. The CLI takes it repeated (`--label k=v`); JSON takes
/// an object.
#[derive(Clone, Debug, PartialEq)]
pub struct Label(pub String, pub String);

impl std::str::FromStr for Label {
    type Err = String;
    fn from_str(s: &str) -> Result<Label, String> {
        match s.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok(Label(k.into(), v.into())),
            _ => Err(format!("expected key=value, got {s:?}")),
        }
    }
}

/// Labels as a JSON object of strings (the last of a repeated key wins).
mod label_map {
    use super::Label;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(labels: &[Label], s: S) -> Result<S::Ok, S::Error> {
        super::labels(labels).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Label>, D::Error> {
        Ok(BTreeMap::<String, String>::deserialize(d)?.into_iter().map(|(k, v)| Label(k, v)).collect())
    }
}

pub fn labels(labels: &[Label]) -> BTreeMap<String, String> {
    labels.iter().map(|Label(k, v)| (k.clone(), v.clone())).collect()
}

/// The word a person or agent types for a value (`idle`, `any`, ...).
pub fn word<T: clap::ValueEnum>(v: &T) -> String {
    v.to_possible_value().map(|p| p.get_name().to_string()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Debug, clap::ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum IdleOrWorking {
    Idle,
    Working,
}

#[derive(Clone, Copy, PartialEq, Debug, clap::ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Children {
    Any,
    All,
}

/// Seconds on the command line: 0 or more, finite (JSON has no NaN, so a
/// bad value must stop here, not turn into null on the way).
fn cli_seconds(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v >= 0.0 => Ok(v),
        _ => Err(format!("expected seconds, 0 or more, got {s:?}")),
    }
}

/// Seconds from a request, checked before anything happens.
pub(super) fn seconds(s: Option<f64>) -> Result<Option<Duration>, RpcError> {
    s.map(|s| {
        Duration::try_from_secs_f64(s)
            .map_err(|_| RpcError::invalid_params(format!("timeout must be 0 or more seconds, not {s}")))
    })
    .transpose()
}

#[derive(Deserialize, JsonSchema)]
pub struct HelloParams {
    /// Protocol generation the caller speaks.
    pub generation: Option<u32>,
    /// Switch this connection to framed mode after the reply.
    #[serde(default)]
    pub framed: bool,
    /// Another core dialing in: who it is. Its terminals may then be parents
    /// of terminals here, and hear from them.
    pub core: Option<CoreInfo>,
    /// What that core calls this one (`rohan`), so news about `t7` reaches it as `rohan/t7`.
    pub calls_you: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct HelloResult {
    pub generation: u32,
    pub version: String,
    pub core: CoreInfo,
    pub framed: bool,
    pub methods: Vec<String>,
    pub features: Vec<String>,
}

#[derive(Serialize, JsonSchema, Clone)]
pub struct HolderInfo {
    pub pid: u32,
    pub instance: String,
    /// Same build as the core: takes new terminals. Others drain.
    pub current: bool,
    /// Live terminals it runs.
    pub terminals: usize,
}

#[derive(Serialize, JsonSchema)]
pub struct StatusResult {
    pub core: CoreInfo,
    pub version: String,
    pub pid: u32,
    pub uptime_ms: u64,
    /// The holder new terminals go to.
    pub holder: Option<HolderInfo>,
    /// Every holder: after an update, older ones run their terminals to the end.
    pub holders: Vec<HolderInfo>,
    pub live: usize,
    pub finished: usize,
    /// The package manager this hc came from (homebrew, nix, mise, or
    /// system package manager): it updates hc, `hc update` won't.
    /// Null for a replaceable plain install.
    pub installed_by: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DocGetParams {
    /// Document name, using the terminal name rules.
    pub name: String,
    /// Another machine; omitted for this core.
    pub on: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DocSetParams {
    pub name: String,
    /// Any JSON value, at most 1 MiB when serialized; null deletes it.
    pub value: serde_json::Value,
    /// Current version from doc.get; zero for a name never written.
    pub version: u64,
    pub on: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DocGetResult {
    pub value: serde_json::Value,
    /// Zero when never written. Deletions retain their version.
    pub version: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct DocSetResult {
    pub version: u64,
}

#[derive(Serialize, JsonSchema)]
pub struct StopResult {
    pub stopped: bool,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct RunParams {
    /// Program and arguments, or a template name and its task. With neither
    /// argv nor line: that machine's own shell.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    #[serde(default)]
    pub argv: Vec<String>,
    /// A shell line instead of argv, run by that machine's shell (`sh -c`;
    /// Windows `%COMSPEC% /d /s /c`). The CLI sends one string with spaces or
    /// shell signs as a line.
    #[arg(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// Name, unique among live terminals: 1-32 of a-z 0-9 - _ .
    #[arg(long)]
    pub name: Option<String>,
    /// Working directory (default: where hc runs; through the API, the core's;
    /// with --on, that machine's home).
    #[arg(long)]
    pub cwd: Option<String>,
    /// Run it on another machine (see `hc machine ls`); its address is `machine/t7`.
    #[arg(long, value_name = "MACHINE")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// Set on top of the core's environment. Clients pass their terminal
    /// identity here (TERM, TERM_PROGRAM, COLORTERM).
    #[arg(skip)]
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Labels, key=value (repeatable).
    #[arg(long = "label", value_name = "K=V")]
    #[serde(default, with = "label_map")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub labels: Vec<Label>,
    /// Parent terminal (default: the terminal it is started from).
    #[arg(long, conflicts_with = "no_parent")]
    pub parent: Option<String>,
    /// No parent, even when started from a terminal.
    #[arg(long)]
    #[serde(default)]
    pub no_parent: bool,
    /// Don't type notes about it into its parent.
    #[arg(long)]
    #[serde(default)]
    pub no_notify: bool,
    /// Keep it running when its parent's program ends (by default it ends too).
    #[arg(long)]
    #[serde(default)]
    pub outlive_parent: bool,
    /// Ignored: children end with their parent unless `outlive_parent`.
    #[arg(long, hide = true)]
    #[serde(default)]
    pub die_with_parent: bool,
    /// Remove it right after it exits (once a waiting parent has the exit code).
    #[arg(long)]
    #[serde(default)]
    pub rm: bool,
    /// Replace lost programs with a shell in place, never re-run the original command.
    #[arg(long)]
    #[serde(default)]
    pub keep: bool,
    /// Then wait until it is not working, like `terminal.wait`, and return its
    /// output (the CLI waits unless --bg).
    #[arg(skip)]
    #[serde(default)]
    pub wait: bool,
    /// Start it and come back at once (a server, several workers).
    #[arg(long)]
    #[serde(skip)]
    #[schemars(skip)]
    pub bg: bool,
    /// Seconds before a wait checks in, leaving it running (default: config
    /// `wait_timeout`, else 180).
    #[arg(long, value_parser = cli_seconds)]
    pub timeout: Option<f64>,
    /// Who is calling (`HC_TERMINAL_ID` inside a terminal).
    #[arg(skip)]
    pub caller: Option<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct TerminalParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    #[arg(default_value = "", hide_default_value = true)]
    pub terminal: String,
    /// Who is calling; a parent looking at its child has seen its news.
    #[arg(skip)]
    pub caller: Option<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Include finished terminals.
    #[arg(long)]
    #[serde(default)]
    pub all: bool,
    /// Only terminals carrying all of these labels (repeatable).
    #[arg(long = "label", value_name = "K=V")]
    #[serde(default, with = "label_map")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub labels: Vec<Label>,
}

#[derive(Serialize, JsonSchema)]
pub struct ListResult {
    /// This machine's terminals, then each connected machine's (`machine/t7`).
    #[schemars(with = "Vec<Public>")]
    pub terminals: Vec<serde_json::Value>,
    /// Finished terminals left out (see `all`).
    pub hidden_finished: usize,
    /// Machines added here and their lines: one that is not connected is
    /// listed here, not waited on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<crate::remote::MachineView>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct LabelParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    pub terminal: String,
    /// Labels to set, key=value; `key=` removes one.
    #[arg(required = true, value_name = "K=V")]
    #[serde(with = "label_map")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub labels: Vec<Label>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct KeepParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    pub terminal: String,
    /// Keep this live terminal after loss. Existing streams end with the loss reason; reattach after `restarted`.
    #[arg(long = "off", action = clap::ArgAction::SetFalse, default_value_t = true)]
    pub keep: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct RemoveResult {
    pub removed: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ReadParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    #[arg(default_value = "", hide_default_value = true)]
    pub terminal: String,
    #[arg(skip)]
    pub caller: Option<String>,
    /// Lines of history above the screen.
    #[arg(long, default_value_t = 0)]
    #[serde(default)]
    pub lines: usize,
    /// The screen with its colors and styles as SGR sequences, for a viewer
    /// that draws them. Only the screen: `lines` is ignored (read history
    /// without it). A finished terminal reads as plain text.
    #[arg(long)]
    #[serde(default)]
    pub ansi: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct ReadResult {
    pub text: String,
    /// False when part of what was asked for may be missing.
    pub complete: bool,
    /// "screen", "history", "stitched" (a full-screen app, read by scrolling it),
    /// or "ansi" (the screen with SGR styles).
    pub how: String,
    /// Why only the screen: "working" (a full-screen app isn't scrolled while
    /// it works; read again once it stops).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct AttachParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    #[arg(default_value = "", hide_default_value = true)]
    pub terminal: String,
    /// Watch only: never type or resize.
    #[arg(long)]
    #[serde(default)]
    pub observe: bool,
    /// Scrollback lines to show first (main screen only).
    #[arg(long, default_value_t = 0)]
    #[serde(default)]
    pub history: usize,
    /// The viewer's size in cells.
    #[arg(skip)]
    pub cols: u16,
    #[arg(skip)]
    pub rows: u16,
    /// The viewer's own colors and cell size, used to answer display queries.
    #[arg(skip)]
    #[serde(default)]
    pub display: crate::viewers::Display,
    /// Back after a dropped line: sets the size only once active again.
    #[arg(skip)]
    #[serde(default)]
    pub resumed: bool,
    /// Who is calling (`HC_TERMINAL_ID` inside a terminal): never itself.
    #[arg(skip)]
    #[serde(default)]
    pub caller: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct AttachResult {
    /// Stream channel: first paint, then live bytes. Send keys as DATA on it;
    /// RESIZE, FOCUS, FACTS, CREDIT and CLOSE frames control it.
    pub channel: u32,
    pub terminal: Public,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
pub struct SubscribeParams {
    /// Only this terminal.
    #[arg(long)]
    pub terminal: Option<String>,
    /// Only children of this terminal.
    #[arg(long)]
    pub children_of: Option<String>,
    /// Only terminals carrying all of these labels (repeatable).
    #[arg(long = "label", value_name = "K=V")]
    #[serde(default, with = "label_map")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub labels: Vec<Label>,
}

#[derive(Serialize, JsonSchema)]
pub struct SubscribeResult {
    /// Events after this sequence follow. After a `missed` snapshot, ignore
    /// events whose seq is not above the snapshot's.
    pub seq: u64,
    /// Matching terminals here and on connected machines (`machine/t7`).
    #[schemars(with = "Vec<Public>")]
    pub terminals: Vec<serde_json::Value>,
    /// Framed mode: the channel events arrive on (JSON lines, with credit).
    /// Plain mode: none; this connection now carries only notifications.
    pub channel: Option<u32>,
}

/// A plain-mode subscription handed to the connection after the reply.
pub struct PlainSub {
    pub rx: tokio::sync::mpsc::Receiver<String>,
    pub behind: Arc<std::sync::atomic::AtomicBool>,
    pub filter: crate::events::Filter,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct SendParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    pub terminal: String,
    /// Pasted, then submitted with Enter.
    pub text: String,
    /// Type even if the program is blocked (it may answer its dialog).
    #[arg(long)]
    #[serde(default)]
    pub force: bool,
    /// Only type if the status is exactly this.
    #[arg(long = "if", value_name = "STATUS")]
    #[serde(rename = "if")]
    pub only_if: Option<IdleOrWorking>,
    /// Then wait for the turn this send started.
    #[arg(long)]
    #[serde(default)]
    pub wait: bool,
    /// With wait: seconds before it checks in (default: config, else 180).
    #[arg(long, value_parser = cli_seconds)]
    pub timeout: Option<f64>,
    /// Who is sending (`HC_TERMINAL_ID`, else the calling process's parent), so a
    /// bare wait from the same caller waits for this send's turn.
    #[arg(skip)]
    pub caller: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct SendResult {
    /// The program's status word when the text went in (null: it never said).
    pub status: Option<String>,
    /// The turn when the text went in.
    pub turn: u64,
    /// Whether it went `working` within a few seconds; null when it doesn't
    /// report status, so nobody can tell.
    pub started: Option<bool>,
    /// With `wait`: how that turn ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<crate::wait::WaitResult>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct WaitParams {
    /// The terminal; with `children`, whose children (default: the caller).
    pub terminal: Option<String>,
    /// Wait on children instead: all = until none is working, any = next news or a message to you.
    ///
    /// `all`: until none of the children is working (one that hasn't reported
    /// yet counts as working). `any`: the parent's unseen news (a turn ended, it
    /// blocked, it hit an error, it exited), else the next child to stop working; for the parent
    /// itself, also a message typed into it while it worked (reason `message`).
    #[arg(long)]
    pub children: Option<Children>,
    /// For programs, not agents: end when output printed after the wait started
    /// contains this text (plain text, escape sequences stripped). Blocked,
    /// error or exit end it too.
    #[arg(long)]
    pub output: Option<String>,
    /// Seconds before it checks in (default: config `wait_timeout`, else 180).
    #[arg(long, value_parser = cli_seconds)]
    pub timeout: Option<f64>,
    /// See `terminal.send`.
    #[arg(skip)]
    pub caller: Option<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct KeysParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    pub terminal: String,
    #[arg(skip)]
    pub caller: Option<String>,
    /// Key names: enter, esc, tab, up, ctrl+c, alt+x, f5, a, ...
    #[arg(required = true)]
    pub keys: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct KeysResult {
    pub status: Option<String>,
    pub turn: u64,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct InterruptParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    #[arg(default_value = "", hide_default_value = true)]
    pub terminal: String,
    /// Seconds to watch for an effect (default 10).
    #[arg(long, value_parser = cli_seconds)]
    pub timeout: Option<f64>,
}

#[derive(Serialize, JsonSchema)]
pub struct InterruptResult {
    /// stopped (left working, or a shell got its prompt back), exited,
    /// still_working (no change in time), or unknown (it doesn't report status).
    pub outcome: String,
    /// The key sent: esc for programs that report status, else ctrl+c.
    pub key: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct PutParams {
    /// The terminal: `t7`, a name, or `machine/t7`.
    pub terminal: String,
    /// The file's name, kept so `.png` stays `.png` (CLI: the local file to send).
    #[arg(value_name = "FILE")]
    pub name: String,
    /// Its bytes, base64; at most 8 MB raw.
    #[arg(skip)]
    pub data: String,
}

#[derive(Serialize, JsonSchema)]
pub struct PutResult {
    /// Where it landed, as the terminal's machine sees it.
    pub path: String,
    pub bytes: usize,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct ExecParams {
    /// Program and arguments, run as given (no shell).
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    /// A shell line instead of argv, run by that machine's shell (`sh -c`;
    /// Windows `%COMSPEC% /d /s /c`). The CLI sends one string with spaces or
    /// shell signs as a line.
    #[arg(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// Run it on another machine (see `hc machine ls`).
    #[arg(long, value_name = "MACHINE")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// Working directory (default: where hc runs; through the API, the core's;
    /// with --on, that machine's home).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Set on top of the core's environment, key=value (repeatable).
    #[arg(long = "env", value_name = "K=V")]
    #[serde(default, with = "label_map")]
    #[schemars(with = "BTreeMap<String, String>")]
    pub env: Vec<Label>,
    /// Seconds before it and everything it started are ended (default 60, at most 600).
    #[arg(long, value_parser = cli_seconds)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    /// Bytes for its stdin, base64, at most 8 MB raw (without: stdin is closed).
    #[arg(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Feed what is piped into hc to its stdin.
    #[arg(short = 'i', long)]
    #[serde(skip)]
    #[schemars(skip)]
    pub stdin: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct ExecResult {
    /// Its exit code; null when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended it (Unix).
    pub signal: Option<i32>,
    /// What it wrote, base64: the first 8 MB.
    pub stdout: String,
    /// What it wrote, base64: the first 1 MB.
    pub stderr: String,
    /// It wrote more than was kept (it still ran to its end).
    pub truncated: bool,
    /// It ran out of time and was ended: the output is what came so far.
    pub timed_out: bool,
    /// How long it ran, in milliseconds.
    pub ms: u64,
}

/// A file here or on a machine.
#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FileAt {
    /// The file: absolute, or from `cwd`.
    pub path: String,
    /// On another machine (see `hc machine ls`).
    #[arg(long, value_name = "MACHINE")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// Where a relative path starts (default: where hc runs; through the API,
    /// the core's; with --on, that machine's home).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FileReadParams {
    #[command(flatten)]
    #[serde(flatten)]
    pub at: FileAt,
    /// The first line to give, from 1.
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    /// At most this many lines (default: all, up to 8 MB).
    #[arg(long)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct FileReadResult {
    /// The lines as they are in the file, line ends kept.
    pub text: String,
    /// The first and last line given (last < first: none).
    pub first: usize,
    pub last: usize,
    /// Lines in the file.
    pub lines: usize,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FileWriteParams {
    #[command(flatten)]
    #[serde(flatten)]
    pub at: FileAt,
    /// Its new content, base64, at most 8 MB raw (CLI: what is piped in).
    #[arg(skip)]
    pub data: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct FileEditParams {
    #[command(flatten)]
    #[serde(flatten)]
    pub at: FileAt,
    /// Replacements, all made or none (CLI: --old/--new pairs).
    #[arg(skip)]
    pub edits: Vec<Edit>,
    /// Exact text found once in the file (repeatable, paired with --new in order).
    #[arg(long, required = true)]
    #[serde(skip)]
    #[schemars(skip)]
    pub old: Vec<String>,
    /// What replaces it.
    #[arg(long, required = true)]
    #[serde(skip)]
    #[schemars(skip)]
    pub new: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct Edit {
    /// Exact text, found exactly once in the file as it was.
    pub old: String,
    pub new: String,
}

#[derive(Serialize, JsonSchema)]
pub struct FileResult {
    /// The file written, absolute.
    pub path: String,
    pub bytes: usize,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct MachineAddParams {
    /// How ssh reaches it: `vps`, `can@10.0.0.7`, a Host from ~/.ssh/config.
    #[arg(required_unless_present = "via", conflicts_with = "via")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Reach it with this command instead of ssh: it is given a shell line to
    /// run there as its last argument, like ssh (`ssh -p 2222 -i key user@ip`,
    /// `fly ssh console -a app --pty=false -C`). It must pass bytes both ways
    /// untouched and ask nothing. Needs --name.
    #[arg(long, value_name = "COMMAND")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// Mark it short-lived (a sandbox for a task), so clients can show it apart
    /// from machines that stay. hc treats it like any other; remove it with
    /// `hc machine rm`.
    #[arg(long = "temp")]
    #[serde(default)]
    pub temporary: bool,
    /// Copy this hc there when it has none or one too old, without asking
    /// (for scripts; in a terminal you are asked).
    #[arg(long)]
    #[serde(skip)]
    #[schemars(skip)]
    pub install: bool,
    /// What to call it here (default: the target's host).
    #[arg(long)]
    pub name: Option<String>,
    /// The hc session on that machine (default: its default).
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct MachineParams {
    /// The machine's name.
    pub machine: String,
}

#[derive(clap::Args, Serialize, Deserialize, JsonSchema)]
pub struct MachineRenameParams {
    /// The machine's name now.
    pub machine: String,
    /// Its new name. Addresses with the old one stop working.
    pub new_name: String,
}

#[derive(Serialize, JsonSchema)]
pub struct MachineListResult {
    pub machines: Vec<crate::remote::MachineView>,
}

#[derive(Serialize, JsonSchema)]
pub struct RunResult {
    #[serde(flatten)]
    pub terminal: Public,
    /// Template runs: whether the first message went in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_message: Option<crate::templates::FirstMessage>,
    /// With `wait`: how the wait ended (exit, a status, or timeout: still running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitResult>,
    /// With `wait`: its text then, like `terminal.read` (the screen, plus
    /// history unless it is a full-screen app).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct TemplatesResult {
    pub templates: Vec<crate::templates::TemplateInfo>,
}
