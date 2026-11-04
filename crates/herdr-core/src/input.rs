//! Input from agents: messages stay ordered while controls can pass draft holds.
//! Actual delivery never interleaves operations or a person's typing.

use crate::holder_proto as proto;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::terminals::Terminal;
use std::time::{Duration, Instant};

/// Pause between the paste and Enter, so the program sees them apart.
pub const PASTE_GAP: Duration = Duration::from_millis(200);
/// Pause between keys, so ESC then x isn't read as alt+x.
const KEY_GAP: Duration = Duration::from_millis(30);
/// A program whose output stopped this long ago has finished drawing (`ready: quiet`).
pub const OUTPUT_SETTLE: Duration = Duration::from_millis(800);
/// How long after start an agent may still be setting itself up.
const STARTUP_MS: u64 = 10_000;
/// The longest a send waits for a starting agent to settle.
const SETTLE_MAX: Duration = Duration::from_secs(5);
/// Maximum extra hold for an unsent human draft; remote calls budget for it too.
pub const DRAFT_MAX_MS: u64 = 180_000;

#[derive(Default)]
pub struct Queue {
    messages: tokio::sync::Mutex<()>,
    pub(crate) delivery: tokio::sync::Mutex<()>,
}

/// A person's typing, as it affects agent input to one terminal.
#[derive(Default)]
pub struct Gate {
    last_human: Option<Instant>,
    draft: Draft,
    pub waiting_for_person: bool,
    /// An agent's input is going in right now: viewer input waits.
    busy: bool,
    held: Vec<u8>,
    held_human: bool,
    held_draft: Draft,
}

impl Gate {
    pub fn clear_draft(&mut self) {
        self.draft = Draft::default();
    }

    fn submitted(&mut self) {
        // Held human keys have not reached the program's composer yet.
        self.draft = std::mem::take(&mut self.held_draft);
    }

    /// A person pressed keys while an agent's input was going in.
    pub fn has_held(&self) -> bool {
        self.held_human
    }

    /// Starts or ends an agent's input; ending returns the viewer bytes held meanwhile.
    pub fn set_busy(&mut self, busy: bool) -> Vec<u8> {
        self.busy = busy;
        if busy {
            self.held_draft = Draft::default();
            Vec::new()
        } else {
            self.held_human = false;
            std::mem::take(&mut self.held)
        }
    }

    /// Viewer bytes: straight through, or held while an agent types.
    /// Only human input resets the quiet period.
    pub fn viewer(&mut self, bytes: &[u8], human: bool) -> Vec<u8> {
        if human && !bytes.is_empty() {
            self.last_human = Some(Instant::now());
            self.draft.feed(bytes);
            if self.busy {
                self.held_draft.feed(bytes);
            }
        }
        if self.busy {
            self.held_human |= human && !bytes.is_empty();
            self.held.extend_from_slice(bytes);
            Vec::new()
        } else {
            bytes.to_vec()
        }
    }
}

/// Conservative input tracking, not a claim about the program's composer.
/// Escape does not mean clear in every program; only Enter, ctrl+c and ctrl+u release.
#[derive(Default)]
struct Draft {
    pending: bool,
    escape: Vec<u8>,
    pasted: bool,
    control_string: Option<ControlString>,
    before_string: bool,
}

struct ControlString {
    osc: bool,
    selector: bool,
    escaped: bool,
}

impl Draft {
    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.pasted {
                // Only the end marker has meaning inside paste; retain overlapping ESCs.
                self.escape.push(byte);
                while !b"\x1b[201~".starts_with(&self.escape) {
                    self.pending = true;
                    self.escape.remove(0);
                }
                if self.escape == b"\x1b[201~" {
                    self.pasted = false;
                    self.escape.clear();
                }
                continue;
            }
            if let Some(reply) = &mut self.control_string {
                if (reply.escaped && byte == b'\\') || (reply.osc && !reply.selector && byte == 7) {
                    self.pending = self.before_string;
                    self.control_string = None;
                    continue;
                }
                let invalid = matches!(byte, b'\r' | b'\n' | 3 | 21 | 127)
                    || (reply.escaped && byte != b'\\')
                    || (reply.selector && !byte.is_ascii_digit() && byte != b';');
                if !invalid {
                    reply.selector &= byte != b';';
                    reply.escaped = byte == 0x1b;
                    continue;
                }
                if reply.escaped {
                    self.escape.push(0x1b);
                }
                self.control_string = None;
            }
            if let [0x1b, kind @ (b'_' | b']' | b'P')] = self.escape.as_slice() {
                let reply = match kind {
                    b'_' => byte == b'G',
                    b']' => byte.is_ascii_digit(),
                    b'P' => byte.is_ascii_digit() || matches!(byte, b'>' | b'!' | b'+' | b'$'),
                    _ => unreachable!(),
                };
                if reply {
                    self.control_string =
                        Some(ControlString { osc: *kind == b']', selector: *kind == b']', escaped: false });
                    self.escape.clear();
                    continue;
                }
                // Not a reply prefix: retain the Alt key and process this byte normally.
                self.key(u32::from(*kind));
                self.escape.clear();
            }
            if byte == 0x1b {
                self.escape.clear();
                self.escape.push(byte);
            } else if !self.escape.is_empty() {
                self.escape.push(byte);
                if self.escape.len() == 2 && matches!(byte, b'_' | b']' | b'P') {
                    self.before_string = self.pending;
                    self.pending = true;
                    continue;
                }
                if self.escape.len() == 2 && matches!(byte, b'[' | b'O') {
                    continue;
                }
                if self.escape.len() > 2 && !(0x40..=0x7e).contains(&byte) {
                    if self.escape.len() > 128 {
                        self.escape.clear();
                    }
                    continue;
                }
                match self.escape.as_slice() {
                    b"\x1b[200~" => self.pasted = true,
                    b"\x1b[201~" => self.pasted = false,
                    [0x1b, b'b' | b'f' | b'B' | b'F'] => {} // Alt word navigation.
                    [0x1b, b] => self.key(u32::from(*b)),
                    _ => self.encoded_key(),
                }
                self.escape.clear();
            } else {
                self.key(u32::from(byte));
            }
        }
    }

    fn key(&mut self, code: u32) {
        match code {
            13 | 3 | 21 if !self.pasted => self.pending = false,
            9 | 32..=126 | 128..=0x10ffff => self.pending = true,
            _ => {}
        }
    }

    fn encoded_key(&mut self) {
        let Some(body) = self.escape.strip_prefix(b"\x1b[") else { return };
        let Some((&kind, params)) = body.split_last() else { return };
        let Ok(params) = std::str::from_utf8(params) else { return };
        let fields: Vec<&str> = params.split(';').collect();
        let number = |i: usize| fields.get(i)?.split(':').next()?.parse::<u32>().ok();
        let (code, ctrl, shift, alt) = match kind {
            // Kitty/CSI-u: release events do not type, nor do functional key codes.
            b'u' if !fields.get(1).is_some_and(|s| s.ends_with(":3")) => {
                let code = match number(0) {
                    Some(57414) => 13,         // Keypad Enter.
                    Some(57399..=57416) => 32, // Keypad text.
                    Some(code) if !(57344..=63743).contains(&code) => code,
                    _ => return,
                };
                let modifiers = number(1).unwrap_or(1).saturating_sub(1);
                (code, modifiers & 4 != 0, modifiers & 1 != 0, modifiers & 2 != 0)
            }
            // xterm modifyOtherKeys.
            b'~' if number(0) == Some(27) => {
                let Some(code) = number(2) else { return };
                let modifiers = number(1).unwrap_or(1).saturating_sub(1);
                (code, modifiers & 4 != 0, modifiers & 1 != 0, modifiers & 2 != 0)
            }
            // Win32-input-mode: Unicode character, key-down, control-state.
            b'_' if number(3) == Some(1) => {
                let Some(code) = number(2) else { return };
                let modifiers = number(4).unwrap_or(0);
                (code, modifiers & 12 != 0, modifiers & 16 != 0, modifiers & 3 != 0)
            }
            _ => return,
        };
        if (code == 13 && shift) || (alt && matches!(code, 66 | 70 | 98 | 102)) {
            return;
        }
        self.key(if ctrl && (64..=127).contains(&code) { code & 31 } else { code });
    }
}

/// Cleans up the transient waiting flag even if an input operation is cancelled.
struct PersonWait<'a> {
    core: &'a Core,
    id: u32,
}

impl Drop for PersonWait<'_> {
    fn drop(&mut self) {
        if let Some(gate) = self.core.lock().gates.get_mut(&self.id) {
            gate.waiting_for_person = false;
        }
    }
}

pub enum Step {
    FirstTask(Vec<u8>),
    Write(Vec<u8>),
    Pause(Duration),
}

fn env_ms(var: &str, default: u64) -> Duration {
    Duration::from_millis(std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
}

/// How long a person must have stopped typing before an agent's send goes in.
pub fn quiet() -> Duration {
    env_ms("HC_QUIET_MS", 3000)
}

/// How long `send` watches for the program to start working.
fn started_window() -> Duration {
    env_ms("HC_STARTED_MS", 5000)
}

fn working(t: &Terminal) -> bool {
    t.status.as_ref().is_some_and(|s| s.state == "working")
}

fn state_word(t: &Terminal) -> Option<String> {
    t.status.as_ref().map(|s| s.state.clone())
}

/// A paste the way a terminal sends one: bracketed when the program asked.
pub fn paste(bracketed: bool, text: &str) -> Vec<u8> {
    let text = text.replace("\x1b[201~", "");
    if bracketed { format!("\x1b[200~{text}\x1b[201~").into_bytes() } else { text.into_bytes() }
}

pub struct Sent {
    pub status: Option<String>,
    pub turn: u64,
    /// Whether it went `working` soon after; None when it never reports status.
    pub started: Option<bool>,
    pub waited_for_person: bool,
}

pub enum Interrupted {
    Stopped,
    Exited,
    StillWorking,
    Unknown,
}

impl Core {
    /// Types `steps` (built by `prepare` from the live state) into terminal `id`.
    /// `quiet`: how long a person must have stopped typing first (zero: don't wait).
    /// Messages also hold for an unsent human draft, capped at three minutes.
    /// Returns whether that draft held this operation (including time in the queue).
    pub async fn type_in<T>(
        &self,
        id: u32,
        quiet: Duration,
        protect_draft: bool,
        prepare: impl FnOnce(&State, &Terminal) -> Result<(T, Vec<Step>), RpcError>,
    ) -> Result<(T, bool), RpcError> {
        let deadline = Instant::now() + env_ms("HC_DRAFT_MS", DRAFT_MAX_MS);
        let (queue, mut waited_for_person) = {
            let mut st = self.lock();
            let draft = protect_draft && st.gates.get(&id).is_some_and(|g| g.draft.pending);
            (st.queues.entry(id).or_default().clone(), draft)
        };
        let _message = if protect_draft { Some(queue.messages.lock().await) } else { None };
        let _waiting = protect_draft.then(|| PersonWait { core: self, id });
        let mut prepare = Some(prepare);
        let (out, steps, _delivery) = loop {
            let delivery = queue.delivery.lock().await;
            let prepared = {
                let mut st = self.lock();
                let t = st.reg.terminals.get(&id).filter(|t| t.is_live()).ok_or_else(|| {
                    RpcError::new(
                        "finished",
                        format!("{} has finished", crate::terminals::address(id)),
                        format!("hc read {}", crate::terminals::address(id)),
                    )
                })?;
                t.check_program()?;
                let gate = st.gates.entry(id).or_default();
                let draft = protect_draft && gate.draft.pending && Instant::now() < deadline;
                if protect_draft {
                    gate.waiting_for_person = draft;
                }
                waited_for_person |= draft;
                let left = gate.last_human.map_or(Duration::ZERO, |t| quiet.saturating_sub(t.elapsed()));
                if draft || !left.is_zero() {
                    None
                } else {
                    let t = &st.reg.terminals[&id];
                    let prepared = prepare.take().expect("prepared once")(&st, t)?;
                    st.gates.entry(id).or_default().set_busy(true);
                    Some(prepared)
                }
            };
            if let Some(prepared) = prepared {
                break (prepared.0, prepared.1, delivery);
            }
            drop(delivery);
            // Also notice submit/clear or an exit while waiting.
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        for step in steps {
            match step {
                Step::FirstTask(bytes) => {
                    let mut st = self.lock();
                    if st.link(id).is_some() {
                        if let Some(t) = st.reg.terminals.get_mut(&id) {
                            t.first_task_turn = Some(t.turn);
                        }
                        st.link(id).expect("delivery link").input(id, &bytes);
                        self.save(&st);
                        self.changed.notify_waiters();
                    }
                }
                Step::Write(bytes) => {
                    if let Some(link) = self.lock().link(id) {
                        link.input(id, &bytes);
                    }
                }
                Step::Pause(d) => tokio::time::sleep(d).await,
            }
        }
        let mut st = self.lock();
        let gate = st.gates.entry(id).or_default();
        if protect_draft {
            gate.submitted();
        }
        let held = gate.set_busy(false);
        if let Some(link) = st.link(id) {
            link.input(id, &held);
        }
        Ok((out, waited_for_person))
    }

    /// Paste + Enter, after a person has paused typing. Refuses a blocked
    /// program (typing would answer its dialog) unless `force`.
    pub async fn send(&self, id: u32, text: &str, force: bool, only_if: Option<&str>) -> Result<Sent, RpcError> {
        self.send_message(id, text, force, only_if, false).await
    }

    pub(crate) async fn send_first_task(&self, id: u32, text: &str) -> Result<Sent, RpcError> {
        self.send_message(id, text, false, None, true).await
    }

    async fn send_message(
        &self,
        id: u32,
        text: &str,
        force: bool,
        only_if: Option<&str>,
        first_task: bool,
    ) -> Result<Sent, RpcError> {
        if let Some(t) = self.lock().reg.terminals.get(&id) {
            t.check_program()?;
        }
        self.settle(id).await?;
        let ((status, turn, reported), waited_for_person) = self
            .type_in(id, quiet(), true, |st, t| {
                let word = state_word(t);
                if word.as_deref() == Some("blocked") && !force {
                    return Err(RpcError::new(
                        "blocked",
                        format!(
                            "{} is blocked ({})",
                            crate::terminals::address(id),
                            t.status.as_ref().and_then(|s| s.kind.as_deref()).unwrap_or("waiting")
                        ),
                        format!(
                            "typing would answer its dialog; read its screen first (hc read {}), or --force",
                            crate::terminals::address(id)
                        ),
                    ));
                }
                if let Some(want) = only_if
                    && word.as_deref() != Some(want)
                {
                    return Err(RpcError::new(
                        "status_mismatch",
                        format!(
                            "{} is {}, not {want}",
                            crate::terminals::address(id),
                            word.as_deref().unwrap_or("silent")
                        ),
                        "nothing was typed",
                    ));
                }
                let screen = st.screens.get(&id).expect("live terminal has a screen");
                // A template may say how its agent submits (default: Enter after 200 ms).
                let submit = match &t.submit {
                    Some(key) => crate::keys::encode(screen, key)?,
                    None => b"\r".to_vec(),
                };
                let gap = t.paste_gap_ms.map_or(PASTE_GAP, Duration::from_millis);
                let bytes = paste(screen.dec_mode(2004), text);
                let paste = if first_task { Step::FirstTask(bytes) } else { Step::Write(bytes) };
                let steps = vec![paste, Step::Pause(gap), Step::Write(submit)];
                Ok(((word, t.turn, t.reported), steps))
            })
            .await?;
        let started = if reported {
            let went =
                |st: &State| st.reg.terminals.get(&id).is_none_or(|t| t.turn > turn || working(t) || !t.is_live());
            self.wait_for(started_window(), went).await;
            let st = self.lock();
            Some(st.reg.terminals.get(&id).is_some_and(|t| t.turn > turn || working(t)))
        } else {
            None
        };
        Ok(Sent { status, turn, started, waited_for_person })
    }

    /// A starting agent can report before it is ready for a paste (Pi swaps its
    /// input box right after its first report and drops a paste in flight):
    /// in its first seconds, wait until its screen has been still a moment.
    async fn settle(&self, id: u32) -> Result<(), RpcError> {
        let deadline = Instant::now() + SETTLE_MAX;
        loop {
            let ready = {
                let st = self.lock();
                let age = st
                    .reg
                    .terminals
                    .get(&id)
                    .map(|t| Duration::from_millis(crate::runtime::now_ms().saturating_sub(t.created_ms)));
                age.is_none_or(|age| {
                    age >= Duration::from_millis(STARTUP_MS)
                        || st.last_output.get(&id).map_or(age, Instant::elapsed) >= env_ms("HC_SETTLE_MS", 800)
                })
            };
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(RpcError::new(
                    "unavailable",
                    format!("{} is still starting: its output has not settled", crate::terminals::address(id)),
                    format!("nothing was typed; check hc read {}, then retry", crate::terminals::address(id)),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Named keys, after a person has paused typing.
    pub async fn keys(&self, id: u32, names: &[String]) -> Result<(Option<String>, u64), RpcError> {
        self.type_in(id, quiet(), false, |st, t| {
            let screen = st.screens.get(&id).expect("live terminal has a screen");
            let mut steps = Vec::new();
            for name in names {
                steps.push(Step::Write(crate::keys::encode(screen, name)?));
                steps.push(Step::Pause(KEY_GAP));
            }
            Ok(((state_word(t), t.turn), steps))
        })
        .await
        .map(|(out, _)| out)
    }

    /// Esc for programs that report status, else ctrl+c; then watch what happens.
    pub async fn interrupt(&self, id: u32, timeout: Duration) -> Result<(Interrupted, String), RpcError> {
        let (reported, pid) = {
            let st = self.lock();
            let t = st.reg.terminals.get(&id).ok_or_else(|| RpcError::new("not_found", "terminal removed", ""))?;
            t.check_program()?;
            (t.reported, t.pid.unwrap_or(0) as i32)
        };
        let before = if reported { -1 } else { self.foreground(id).await };
        let key = self
            .type_in(id, Duration::ZERO, false, |st, t| {
                let default = if reported { "esc" } else { "ctrl+c" };
                let key = t.stop.clone().unwrap_or_else(|| default.into());
                let screen = st.screens.get(&id).expect("live terminal has a screen");
                let bytes = crate::keys::encode(screen, &key)?;
                Ok((key, vec![Step::Write(bytes)]))
            })
            .await?
            .0;
        let live = |st: &State| st.reg.terminals.get(&id).is_some_and(|t| t.is_live());
        if reported {
            let settled = |st: &State| st.reg.terminals.get(&id).is_none_or(|t| !t.is_live() || !working(t));
            self.wait_for(timeout, settled).await;
            let st = self.lock();
            let outcome = match st.reg.terminals.get(&id) {
                Some(t) if !t.is_live() => Interrupted::Exited,
                Some(t) if working(t) => Interrupted::StillWorking,
                _ => Interrupted::Stopped,
            };
            return Ok((outcome, key));
        }
        // No status reports: a shell that gets the terminal back has stopped its job.
        let deadline = Instant::now() + timeout;
        loop {
            if !live(&self.lock()) {
                return Ok((Interrupted::Exited, key));
            }
            if before != pid && self.foreground(id).await == pid {
                return Ok((Interrupted::Stopped, key));
            }
            if Instant::now() >= deadline {
                return Ok((Interrupted::Unknown, key));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The process group in the foreground of terminal `id`, or -1.
    pub(crate) async fn foreground(&self, id: u32) -> i32 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut st = self.lock();
            st.foreground.entry(id).or_default().push(tx);
            let Some(link) = st.link(id) else { return -1 };
            link.send(&proto::frame(proto::FOREGROUND, id, &[]));
        }
        tokio::time::timeout(Duration::from_secs(1), rx).await.ok().and_then(Result::ok).unwrap_or(-1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_replies_preserve_the_human_quiet_gate() {
        let mut gate = Gate::default();
        assert_eq!(gate.viewer(b"reply", false), b"reply");
        assert!(gate.last_human.is_none());
        assert!(!gate.draft.pending);
        gate.viewer(b"x", true);
        assert!(gate.draft.pending);
        let human = gate.last_human;
        gate.set_busy(true);
        assert!(gate.viewer(b"automatic", false).is_empty());
        assert!(!gate.has_held(), "automatic replies are not a person interrupting a scroll");
        assert_eq!(gate.last_human, human);
        gate.viewer(b"\r\x15", false);
        assert!(gate.draft.pending, "automatic input cannot submit or clear a person's draft");
        gate.viewer(b"typed", true);
        assert!(gate.has_held());
        assert_eq!(gate.set_busy(false), b"automatic\r\x15typed");
        assert!(!gate.has_held());
    }

    #[test]
    fn delivery_consumes_the_old_draft_but_preserves_held_human_keys() {
        for held in [b"".as_slice(), b"new draft", b"\x1b[97u", b"discarded\r"] {
            let mut gate = Gate::default();
            gate.viewer(b"old draft", true);
            gate.set_busy(true);
            gate.viewer(held, true);
            gate.viewer(b"automatic reply", false);
            gate.submitted();
            assert_eq!(gate.draft.pending, !held.is_empty() && !held.ends_with(b"\r"), "{held:?}");
            let mut expected = held.to_vec();
            expected.extend_from_slice(b"automatic reply");
            assert_eq!(gate.set_busy(false), expected);
        }
    }

    #[test]
    fn drafts_clear_only_on_submit_or_unambiguous_clear_keys() {
        for clear in [
            b"\r".as_slice(),
            b"\x03",
            b"\x15",
            b"\x1b[13u",
            b"\x1b[57414u",
            b"\x1b[99;5u",
            b"\x1b[27;5;117~",
            b"\x1b[13;0;13;1;0;1_",
        ] {
            let mut draft = Draft::default();
            draft.feed(b"unfinished");
            for byte in clear {
                draft.feed(&[*byte]);
            }
            assert!(!draft.pending, "{clear:?}");
        }
        for keep in [b"\x7f".as_slice(), b"\x08", b"\x1b", b"\x1b[A", b"\x1b[13;1:3u"] {
            let mut draft = Draft::default();
            draft.feed(b"unfinished");
            draft.feed(keep);
            assert!(draft.pending, "{keep:?}");
        }
    }

    #[test]
    fn paste_newlines_and_navigation_do_not_release_a_draft() {
        let mut draft = Draft::default();
        draft.feed(b"\x1b[200~\x1b[201~");
        assert!(!draft.pending, "an empty paste is not a draft");
        draft.feed(b"\x1b[A\x1b[<64;5;5M\x7f\x08");
        assert!(!draft.pending);
        for byte in b"\x1b[200~a\nb\r\x1b[201~" {
            draft.feed(&[*byte]);
        }
        assert!(draft.pending);
        draft.feed(b"\x1b\r");
        assert!(!draft.pending);
        for key in [b"\x1b[97u".as_slice(), b"\x1b[57400u", b"\x1b[65;30;97;1;0;1_", b"\x1b[27;1;97~"] {
            draft.feed(key);
            assert!(draft.pending, "{key:?}");
            draft.feed(b"\x15");
        }
    }

    #[test]
    fn multiline_keys_do_not_release_drafts() {
        for key in [b"\n".as_slice(), b"\x1b[13;2u", b"\x1b[57414;2u", b"\x1b[27;2;13~"] {
            let mut draft = Draft::default();
            draft.feed(b"unfinished");
            draft.feed(key);
            assert!(draft.pending, "{key:?}");
        }
    }

    #[test]
    fn word_navigation_and_escape_then_arrows_do_not_create_drafts() {
        for key in [
            b"\x1bb".as_slice(),
            b"\x1bf",
            b"\x1bB",
            b"\x1bF",
            b"\x1b\x1b[A",
            b"\x1b\x1bOD",
            b"\x1b[98;3u",
            b"\x1b[102;3u",
            b"\x1b[66;4u",
            b"\x1b[70;4u",
            b"\x1b[27;3;98~",
            b"\x1b[27;3;102~",
            b"\x1b[66;48;98;1;2;1_",
            b"\x1b[70;33;102;1;1;1_",
            b"\x1b[1;3D",
            b"\x1b[1;3C",
            b"\x1b[57350;3u",
            b"\x1b[57351;3u",
            b"\x1b[37;75;0;1;2;1_",
            b"\x1b[39;77;0;1;2;1_",
            b"\x01",
            b"\x05",
            b"\x1b[97;5u",
            b"\x1b[101;5u",
            b"\x1b[27;5;97~",
            b"\x1b[27;5;101~",
            b"\x1b[65;30;97;1;8;1_",
            b"\x1b[69;18;101;1;4;1_",
        ] {
            let mut draft = Draft::default();
            for byte in key {
                draft.feed(&[*byte]);
            }
            assert!(!draft.pending, "{key:?}");
            draft.feed(b"unfinished");
            draft.feed(key);
            assert!(draft.pending, "navigation must preserve an existing draft: {key:?}");
        }
    }

    #[test]
    fn control_string_replies_do_not_create_or_clear_drafts() {
        for reply in [
            b"\x1b_Gi=31;OK\x1b\\".as_slice(),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
            b"\x1b]11;rgb:0000/0000/0000\x07",
            b"\x1bP>|terminal-name\x1b\\",
            b"\x1bP1+r544e=787465726d\x1b\\",
            b"\x1bP!|1234\x1b\\",
            b"\x1bP+r544e=787465726d\x1b\\",
            b"\x1bP$r0m\x1b\\",
        ] {
            for split in 0..=reply.len() {
                let mut draft = Draft::default();
                draft.feed(&reply[..split]);
                draft.feed(&reply[split..]);
                assert!(!draft.pending, "reply: {reply:?}, split {split}");
            }
            let mut draft = Draft::default();
            for byte in reply {
                draft.feed(&[*byte]);
            }
            assert!(!draft.pending, "bytewise reply: {reply:?}");
            draft.feed(b"x");
            assert!(draft.pending, "tracking resumes after reply: {reply:?}");
            for byte in reply {
                draft.feed(&[*byte]);
                assert!(draft.pending, "reply must preserve typing: {reply:?}");
            }
            draft.feed(b"\r");
            assert!(!draft.pending);
        }
    }

    #[test]
    fn long_control_string_replies_preserve_the_previous_draft() {
        for reply in
            [format!("\x1b]52;c;{}\x07", "YWFh".repeat(256)), format!("\x1bP1+r544e={}\x1b\\", "78".repeat(512))]
        {
            for pending in [false, true] {
                for chunk_size in [1, 31, reply.len()] {
                    let mut draft = Draft { pending, ..Default::default() };
                    for chunk in reply.as_bytes().chunks(chunk_size) {
                        draft.feed(chunk);
                    }
                    assert_eq!(draft.pending, pending, "long reply with chunks of {chunk_size}");
                    draft.feed(b"\r");
                    assert!(!draft.pending);
                    draft.feed(b"y");
                    assert!(draft.pending);
                }
            }
        }
    }

    #[test]
    fn alt_control_string_introducers_with_ordinary_text_are_drafts() {
        for key in [b"\x1b]hello".as_slice(), b"\x1b_world", b"\x1bPlease", b"\x1b]\r"] {
            let mut draft = Draft::default();
            for byte in key {
                draft.feed(&[*byte]);
            }
            assert_eq!(draft.pending, !key.ends_with(b"\r"), "{key:?}");
        }
    }

    #[test]
    fn standalone_alt_introducers_protect_drafts_until_a_reply_completes() {
        for key in [b"\x1b]".as_slice(), b"\x1b_", b"\x1bP"] {
            let mut draft = Draft::default();
            draft.feed(key);
            assert!(draft.pending, "standalone Alt key: {key:?}");
        }
    }

    #[test]
    fn invalid_control_strings_return_to_typing_and_clear_keys() {
        for prefix in [b"\x1b]1hello".as_slice(), b"\x1b_Ghello", b"\x1bP>hello"] {
            for invalid in [b'\r', b'\n', 3, 21, 127] {
                let mut draft = Draft::default();
                draft.feed(b"old draft");
                for byte in prefix {
                    draft.feed(&[*byte]);
                }
                draft.feed(&[invalid]);
                assert_eq!(draft.pending, matches!(invalid, b'\n' | 127), "{prefix:?}, {invalid}");
                draft.feed(b"new draft");
                assert!(draft.pending);
            }
            let mut draft = Draft::default();
            draft.feed(prefix);
            draft.feed(b"\x1b");
            draft.feed(b"[97u");
            assert!(draft.pending, "a new escape sequence aborts the candidate: {prefix:?}");
            draft.feed(b"\r");
            assert!(!draft.pending);
        }
        let mut draft = Draft::default();
        draft.feed(b"\x1b]1h");
        assert!(draft.pending, "OSC selectors require a semicolon, not ordinary text");
        draft.feed(b"\x07");
        assert!(draft.pending, "an invalid OSC cannot later be completed as a reply");
    }

    #[test]
    fn other_alt_keys_still_create_drafts() {
        for key in [b"\x1bx".as_slice(), b"\x1b[120;3u", b"\x1b[27;3;120~", b"\x1b[88;45;120;1;2;1_"] {
            let mut draft = Draft::default();
            draft.feed(key);
            assert!(draft.pending, "{key:?}");
        }
    }

    #[test]
    fn paste_ending_in_escape_still_recognizes_the_end_marker() {
        for clear in [b'\r', 3, 21] {
            let mut draft = Draft::default();
            for byte in b"\x1b[200~abc\x1b\x1b[201~" {
                draft.feed(&[*byte]);
            }
            assert!(draft.pending);
            assert!(!draft.pasted);
            draft.feed(&[clear]);
            assert!(!draft.pending);
        }
    }

    #[test]
    fn pastes_are_bracketed_only_when_asked_and_cannot_end_early() {
        assert_eq!(paste(true, "hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste(false, "hi"), b"hi");
        assert_eq!(paste(true, "a\x1b[201~b"), b"\x1b[200~ab\x1b[201~");
    }
}
