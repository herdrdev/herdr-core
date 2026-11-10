//! Input from agents: one queue per terminal, so send, keys and interrupt
//! never interleave with each other, and never with a person typing.

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

/// A person's typing, as it affects agent input to one terminal.
#[derive(Default)]
pub struct Gate {
    last_human: Option<Instant>,
    /// An agent's input is going in right now: viewer input waits.
    busy: bool,
    held: Vec<u8>,
    held_human: bool,
    draft: usize,
    pasting: bool,
    sequence: Vec<u8>,
    draft_ended: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SavedGate {
    last_human_ms: u64,
    draft: usize,
    pasting: bool,
    sequence: Vec<u8>,
    draft_ended: bool,
}

impl Gate {
    pub fn snapshot(&self) -> Option<SavedGate> {
        let elapsed = self.last_human?.elapsed().as_millis().min(u64::MAX as u128) as u64;
        Some(SavedGate {
            last_human_ms: crate::runtime::now_ms().saturating_sub(elapsed),
            draft: self.draft,
            pasting: self.pasting,
            sequence: self.sequence.clone(),
            draft_ended: self.draft_ended,
        })
    }

    pub fn restore(saved: SavedGate) -> Self {
        let elapsed = Duration::from_millis(crate::runtime::now_ms().saturating_sub(saved.last_human_ms));
        Self {
            last_human: Instant::now().checked_sub(elapsed),
            draft: saved.draft,
            pasting: saved.pasting,
            sequence: saved.sequence,
            draft_ended: saved.draft_ended,
            ..Self::default()
        }
    }

    /// Submitting input also waits for the draft, bounded since the last human key.
    fn wait(&self, quiet: Duration, submits: bool, draft: Duration) -> Duration {
        let Some(at) = self.last_human else { return Duration::ZERO };
        if submits && self.draft_ended {
            return PASTE_GAP.saturating_sub(at.elapsed());
        }
        let hold = if submits && self.draft > 0 { quiet.max(draft) } else { quiet };
        hold.saturating_sub(at.elapsed())
    }

    fn key(&mut self, code: u32, modifiers: u32) {
        self.draft_ended = false;
        let modifiers = modifiers.saturating_sub(1) & !0xc0;
        match (code, modifiers + 1) {
            (13 | 3 | 21, 1) | (99 | 117, 5) => {
                self.draft = 0;
                self.draft_ended = true;
            }
            (8 | 127, 1) => self.draft = self.draft.saturating_sub(1),
            (32..=0x10ffff, 1 | 2)
                if char::from_u32(code).is_some_and(|c| !c.is_control()) && !(57344..=63743).contains(&code) =>
            {
                self.draft = self.draft.saturating_add(1);
            }
            _ => {}
        }
    }

    /// Common CSI-u and Win32 press forms; other terminal sequences type no text.
    fn escape(&mut self) {
        let seq = &self.sequence;
        if seq == b"\x1b[200~" {
            self.pasting = true;
        } else if seq.starts_with(b"\x1b[") && seq.ends_with(b"_") {
            let Ok(body) = std::str::from_utf8(&seq[2..seq.len() - 1]) else { return };
            // Omitted fields default to zero, the repeat count to one.
            let mut params = [0, 0, 0, 0, 0, 1];
            let mut fields = body.split(';');
            for (param, field) in params.iter_mut().zip(&mut fields) {
                if !field.is_empty() {
                    let Ok(value) = field.parse::<u32>() else { return };
                    *param = value;
                }
            }
            if fields.next().is_some() {
                return;
            }
            let [_, _, code, down, control, repeat] = &params;
            if *down == 0 || *repeat == 0 || (0xdc00..=0xdfff).contains(code) {
                return;
            }
            // UnicodeChar can be a UTF-16 pair: count its leading event once.
            let code = if (0xd800..=0xdbff).contains(code) { 0xfffd } else { *code };
            // AltGr arrives as Ctrl+Alt with the character it typed.
            let altgr = control & 3 != 0 && control & 12 != 0 && code >= 32;
            let modifiers = if altgr {
                1
            } else {
                1 + u32::from(control & 16 != 0)
                    + 2 * u32::from(control & 3 != 0)
                    + 4 * u32::from(control & 12 != 0 && !matches!(code, 3 | 21))
            };
            let before = self.draft;
            self.key(code, modifiers);
            let repeats = (*repeat - 1) as usize;
            if self.draft > before {
                self.draft = self.draft.saturating_add(repeats);
            } else if self.draft < before && !self.draft_ended {
                self.draft = self.draft.saturating_sub(repeats);
            }
            return;
        } else if seq.starts_with(b"\x1b[") && seq.ends_with(b"u") {
            let Ok(body) = std::str::from_utf8(&seq[2..seq.len() - 1]) else { return };
            let mut params = body.split(';');
            let code = params.next().and_then(|p| p.split(':').next()?.parse::<u32>().ok());
            let mut mods = params.next().unwrap_or("").split(':');
            let modifiers = mods.next().filter(|s| !s.is_empty()).unwrap_or("1").parse::<u32>().ok();
            let event = mods.next().unwrap_or("1");
            // Releases do not type another character.
            if event == "3" {
                return;
            }
            if let (Some(code), Some(modifiers)) = (code, modifiers) {
                self.key(code, modifiers);
            }
            return;
        }
        self.draft_ended = false;
    }

    fn track(&mut self, bytes: &[u8]) {
        for (index, &b) in bytes.iter().enumerate() {
            if self.pasting {
                if b == 0x1b || !self.sequence.is_empty() {
                    self.sequence.push(b);
                    if self.sequence == b"\x1b[201~" {
                        self.pasting = false;
                        self.sequence.clear();
                    } else if !b"\x1b[201~".starts_with(&self.sequence) {
                        let restart = b == 0x1b;
                        self.draft = self.draft.saturating_add(
                            self.sequence.iter().filter(|b| **b & 0xc0 != 0x80).count() - usize::from(restart),
                        );
                        self.sequence.clear();
                        if restart {
                            self.sequence.push(b);
                        }
                    }
                } else if b & 0xc0 != 0x80 {
                    self.draft = self.draft.saturating_add(1);
                }
                self.draft_ended = false;
            } else if self.sequence == b"\x1b" && (b == 0x1b || (index == 0 && (b < 0x20 || b == 0x7f))) {
                self.sequence.clear();
                if b == 0x1b {
                    self.sequence.push(b);
                    self.draft_ended = false;
                } else {
                    self.key(b.into(), 1);
                }
            } else if b == 0x1b || !self.sequence.is_empty() {
                self.sequence.push(b);
                let seq = &self.sequence;
                let complete = match seq.as_slice() {
                    [0x1b] | [0x1b, b'[' | b'O'] => false,
                    [0x1b, b'[', b'M', ..] => seq.len() >= 6, // X10 mouse coordinates
                    [0x1b, b'[', ..] => (0x40..=0x7e).contains(&b),
                    _ => true,
                };
                if complete || self.sequence.len() >= 128 {
                    self.escape();
                    self.sequence.clear();
                }
            } else if b < 0x80 {
                self.key(b.into(), 1);
            } else if b & 0xc0 != 0x80 {
                self.draft_ended = false;
                self.draft = self.draft.saturating_add(1);
            }
        }
    }
    /// A person pressed keys while an agent's input was going in.
    pub fn has_held(&self) -> bool {
        self.held_human
    }

    /// Starts or ends an agent's input; ending returns the viewer bytes held meanwhile.
    pub fn set_busy(&mut self, busy: bool) -> Vec<u8> {
        self.busy = busy;
        if busy {
            Vec::new()
        } else {
            self.held_human = false;
            std::mem::take(&mut self.held)
        }
    }

    /// Viewer bytes: straight through, or held while an agent types.
    /// Only human input resets the quiet period.
    pub fn viewer(&mut self, bytes: &[u8], human: bool) -> Vec<u8> {
        if human {
            self.last_human = Some(Instant::now());
            self.track(bytes);
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

pub enum Step {
    FirstTask(Vec<u8>),
    Write(Vec<u8>),
    Pause(Duration),
    EndDraft(Vec<u8>),
}

fn env_ms(var: &str, default: u64) -> Duration {
    Duration::from_millis(std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
}

/// How long a person must have stopped typing before an agent's send goes in.
pub fn quiet() -> Duration {
    env_ms("HC_QUIET_MS", 3000)
}

pub(crate) fn default_draft_timeout() -> Duration {
    env_ms("HC_DRAFT_MS", 60_000)
}

pub(crate) fn draft_timeout(config: &std::path::Path) -> Duration {
    crate::config::delivery(config).draft_wait.unwrap_or_else(default_draft_timeout)
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
}

pub enum Interrupted {
    Stopped,
    Exited,
    StillWorking,
    Unknown,
}

impl Core {
    /// Types `steps` (built by `prepare` from the live state) into terminal `id`.
    /// `quiet`: how long a person must have stopped typing first.
    /// `submits`: also wait for their unfinished draft, releasing immediately on send/clear.
    pub async fn type_in<T>(
        &self,
        id: u32,
        quiet: Duration,
        submits: bool,
        prepare: impl FnOnce(&State, &Terminal) -> Result<(T, Vec<Step>), RpcError>,
    ) -> Result<T, RpcError> {
        let draft = draft_timeout(&self.paths.config);
        let queue = self.lock().queues.entry(id).or_default().clone();
        let sends = self.lock().sends.entry(id).or_default().clone();
        let _send_turn = if submits { Some(sends.lock().await) } else { None };
        let (out, steps, _turn) = 'waiting: loop {
            if submits {
                loop {
                    crate::server::check_cancelled()?;
                    let left = self.lock().gates.get(&id).map_or(Duration::ZERO, |g| g.wait(quiet, true, draft));
                    if left.is_zero() || !self.lock().reg.terminals.get(&id).is_some_and(|t| t.is_live()) {
                        break;
                    }
                    tokio::time::sleep(left.min(Duration::from_millis(25))).await;
                }
            }
            let turn = queue.lock().await;
            loop {
                if submits {
                    crate::server::check_cancelled()?;
                }
                let left = {
                    let mut st = self.lock();
                    let t = st.reg.terminals.get(&id).filter(|t| t.is_live()).ok_or_else(|| {
                        RpcError::new(
                            "finished",
                            format!("{} has finished", crate::terminals::address(id)),
                            format!("hc read {}", crate::terminals::address(id)),
                        )
                    })?;
                    t.check_program()?;
                    let left = st.gates.get(&id).map_or(Duration::ZERO, |g| g.wait(quiet, submits, draft));
                    if left.is_zero() {
                        let prepared = prepare(&st, t)?;
                        let gate = st.gates.entry(id).or_default();
                        gate.busy = true;
                        let ends_draft = prepared.1.iter().any(|step| matches!(step, Step::EndDraft(_)));
                        if submits || ends_draft {
                            gate.draft = 0;
                        }
                        if ends_draft {
                            gate.draft_ended = true;
                        }
                        break 'waiting (prepared.0, prepared.1, turn);
                    }
                    left
                };
                if submits {
                    drop(turn);
                    continue 'waiting;
                }
                tokio::time::sleep(left.min(Duration::from_millis(25))).await;
            }
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
                Step::Write(bytes) | Step::EndDraft(bytes) => {
                    if let Some(link) = self.lock().link(id) {
                        link.input(id, &bytes);
                    }
                }
                Step::Pause(d) => tokio::time::sleep(d).await,
            }
        }
        let mut st = self.lock();
        let gate = st.gates.entry(id).or_default();
        let held = gate.set_busy(false);
        if let Some(link) = st.link(id) {
            link.input(id, &held);
        }
        Ok(out)
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
        let (status, turn, reported) = self
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
        Ok(Sent { status, turn, started })
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
                let bytes = crate::keys::encode(screen, name)?;
                let mut key = Gate::default();
                key.track(&bytes);
                steps.push(if key.draft_ended { Step::EndDraft(bytes) } else { Step::Write(bytes) });
                steps.push(Step::Pause(KEY_GAP));
            }
            Ok(((state_word(t), t.turn), steps))
        })
        .await
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
            .await?;
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
        gate.viewer(b"x", true);
        let human = gate.last_human;
        gate.set_busy(true);
        assert!(gate.viewer(b"automatic", false).is_empty());
        assert!(!gate.has_held(), "automatic replies are not a person interrupting a scroll");
        assert_eq!(gate.last_human, human);
        gate.viewer(b"typed", true);
        assert!(gate.has_held());
        assert_eq!(gate.set_busy(false), b"automatictyped");
        assert!(!gate.has_held());
    }

    #[test]
    fn a_draft_lasts_until_sent_or_cleared_in_legacy_and_kitty_modes() {
        let draft = |chunks: &[&[u8]]| {
            let mut g = Gate::default();
            for chunk in chunks {
                g.viewer(chunk, true);
            }
            g.draft
        };
        assert_eq!(draft(&[b"hey", "é".as_bytes()]), 4);
        for clear in [b"\r".as_slice(), b"\x03", b"\x15", b"\x1b[13u", b"\x1b[99;5u", b"\x1b[117;5:1u"] {
            assert_eq!(draft(&[b"hey", clear]), 0, "{clear:?}");
        }
        assert_eq!(draft(&[b"hi\x7f\x08\x7f"]), 0);
        assert_eq!(draft(&[b"\x1b[97u\x1b[98;;98u\x1b[127u"]), 1);
        assert_eq!(draft(&[b"\x1b[97;1:1u\x1b[97;1:2u\x1b[97;1:3u"]), 2);
        assert_eq!(draft(&[b"\x1b[97;129u\x1b[97;129:2u\x7f"]), 1);
        assert_eq!(draft(&[b"hey\x1b[99;133u"]), 0);
        assert_eq!(draft(&[b"hey\x1b", b"\r"]), 0);
        assert_eq!(draft(&[b"hey\x1b\r"]), 3, "legacy shift+enter does not submit");
        assert_eq!(draft(&[b"\x1b", b"\x1b[200~a\r\x1b[201~"]), 2);
        assert_eq!(draft(&[b"hey\x1b[13;1:3u\x1b[99;5:3u"]), 3);
        assert_eq!(draft(&[b"\x1b[A\x1bOB\x1bb\x1b[120;3u\x1b[13;2u\x1b[I\x1b[<0;1;1M"]), 0);
        assert_eq!(draft(&[b"\x1b[M !!"]), 0, "legacy mouse report");
        assert_eq!(draft(&[b"\x1b[200~a\rb\x1b[201~"]), 3, "pasted Enter does not submit");
        assert_eq!(draft(&[b"\x1b[200~a\x1b\x1b[201~\r"]), 0, "pasted Esc before end");
        assert_eq!(draft(&[b"\x1b[20", b"0~ab\x1b[20", b"1~\x1b[13", b"u"]), 0, "fragmented sequences");
        let mut g = Gate::default();
        g.viewer(b"automatic", false);
        assert_eq!(g.draft, 0);
        g.viewer(b"x", true);
        g.viewer(b"\r", false);
        assert_eq!(g.draft, 1);
        assert!(!g.wait(Duration::ZERO, true, Duration::from_secs(60)).is_zero());
        g.viewer(b"\x1b[99;5u", true);
        assert!(!g.wait(Duration::from_secs(3), true, Duration::from_secs(60)).is_zero());
        g.last_human = Some(Instant::now() - PASTE_GAP);
        assert!(g.wait(Duration::from_secs(3), true, Duration::from_secs(60)).is_zero());
        assert!(
            !g.wait(Duration::from_secs(3), false, Duration::from_secs(60)).is_zero(),
            "keys keep their quiet wait"
        );
        g.viewer(b"\x1b[A", true);
        assert!(
            !g.wait(Duration::from_secs(3), true, Duration::from_secs(60)).is_zero(),
            "other keys keep the quiet wait"
        );
        g.viewer(b"x", true);
        g.last_human = Some(Instant::now() - Duration::from_secs(61));
        assert!(
            g.wait(Duration::from_secs(3), true, Duration::from_secs(60)).is_zero(),
            "a draft expires after the last key"
        );
    }

    #[test]
    fn handover_restores_partial_input_and_elapsed_quiet() {
        let mut g = Gate::default();
        g.viewer(b"\x1b[200~draft\x1b[20", true);
        g.last_human = Some(Instant::now() - Duration::from_secs(20));
        let saved = serde_json::to_vec(&g.snapshot().unwrap()).unwrap();
        let mut restored = Gate::restore(serde_json::from_slice(&saved).unwrap());
        let left = restored.wait(Duration::ZERO, true, Duration::from_secs(60));
        assert!(left > Duration::from_secs(39) && left <= Duration::from_secs(40));
        restored.viewer(b"1~\r", true);
        assert_eq!(restored.draft, 0);
        assert!(!restored.pasting);
        assert!(restored.draft_ended);
    }

    #[test]
    fn win32_key_events_track_text_repeats_and_clear_controls() {
        let mut g = Gate::default();
        for chunk in [b"\x1b[65;30;97;1;0;1_\x1b[0;0;".as_slice(), b"233;1;0;3_\x1b[65;30;97;0;0;1_"] {
            g.viewer(chunk, true);
        }
        assert_eq!(g.draft, 4);
        g.viewer(b"\x1b[0;0;55357;1;0;1_\x1b[0;0;56832;1;0;1_", true);
        assert_eq!(g.draft, 5, "a UTF-16 surrogate pair types one character");
        g.viewer(b"\x1b[8;14;8;1;0;3_", true);
        assert_eq!(g.draft, 2);
        for clear in [b"\x1b[13;28;13;1;0;1_".as_slice(), b"\x1b[67;46;3;1;8;1_", b"\x1b[85;22;21;1;4;1_"] {
            g.viewer(b"x", true);
            g.viewer(clear, true);
            assert_eq!(g.draft, 0);
            assert!(g.draft_ended);
        }
        g.viewer(b"\x1b[13;28;13;0;0;1_", true);
        assert!(g.draft_ended, "key-up preserves the submit");
        g.viewer(b"\x1b[81;16;64;1;9;1_x\x7f", true);
        assert_eq!(g.draft, 1, "AltGr types its character");
        g.viewer(b"\x1b[;;100;1_", true);
        assert_eq!(g.draft, 2, "omitted fields take their defaults");
        g.viewer(b"\x1b[;;13;1_", true);
        assert!(g.draft_ended);
    }

    #[test]
    fn pastes_are_bracketed_only_when_asked_and_cannot_end_early() {
        assert_eq!(paste(true, "hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste(false, "hi"), b"hi");
        assert_eq!(paste(true, "a\x1b[201~b"), b"\x1b[200~ab\x1b[201~");
    }
}
