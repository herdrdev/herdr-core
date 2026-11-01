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
    /// An agent's input is going in right now: keys a person presses wait.
    busy: bool,
    held: Vec<u8>,
    /// Roughly how many characters a person typed and hasn't sent.
    draft: usize,
    /// Inside a person's bracketed paste.
    pasting: bool,
}

impl Gate {
    /// How long agent input must still wait: until a person has paused for
    /// `quiet` and, for input that submits, until they sent or cleared what they
    /// typed. A draft holds for at most `HC_DRAFT_MS` after their last key: one
    /// erased in a way the keys don't show must not hold input forever.
    pub fn wait(&self, quiet: Duration, submits: bool) -> Duration {
        let Some(at) = self.last_human else { return Duration::ZERO };
        let hold = if submits && self.draft > 0 { quiet.max(env_ms("HC_DRAFT_MS", 60_000)) } else { quiet };
        hold.saturating_sub(at.elapsed())
    }

    /// Follows a person's draft from their keys: Enter sends it, ctrl+c and
    /// ctrl+u clear it, backspace takes one back; other key sequences don't count.
    fn track(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if self.pasting {
                if bytes[i..].starts_with(b"\x1b[201~") {
                    self.pasting = false;
                    i += 6;
                } else {
                    self.draft += 1;
                    i += 1;
                }
                continue;
            }
            if bytes[i..].starts_with(b"\x1b[200~") {
                self.pasting = true;
                i += 6;
                continue;
            }
            match bytes[i] {
                b'\r' | 0x03 | 0x15 => self.draft = 0,
                0x7f | 0x08 => self.draft = self.draft.saturating_sub(1),
                0x1b => {
                    // Skip the sequence: ESC [ … final, ESC O x, or alt+x.
                    i += 1;
                    match bytes.get(i) {
                        Some(b'[') => {
                            i += 1;
                            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                                i += 1;
                            }
                        }
                        Some(b'O') => i += 1,
                        _ => {}
                    }
                }
                b if b >= 0x20 && b & 0xc0 != 0x80 => self.draft += 1,
                _ => {}
            }
            i += 1;
        }
    }

    /// A person pressed keys while an agent's input was going in.
    pub fn has_held(&self) -> bool {
        !self.held.is_empty()
    }

    /// Starts or ends an agent's input; ending returns the keys a person pressed meanwhile.
    pub fn set_busy(&mut self, busy: bool) -> Vec<u8> {
        self.busy = busy;
        if busy { Vec::new() } else { std::mem::take(&mut self.held) }
    }

    /// A typing viewer's keys: straight through, or held while an agent types.
    /// Returns what to write now.
    pub fn human(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.last_human = Some(Instant::now());
        self.track(bytes);
        if self.busy {
            self.held.extend_from_slice(bytes);
            Vec::new()
        } else {
            bytes.to_vec()
        }
    }
}

pub enum Step {
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
    /// `submits`: the steps press Enter, so they also wait out a person's unsent draft.
    pub async fn type_in<T>(
        &self,
        id: u32,
        quiet: Duration,
        submits: bool,
        prepare: impl FnOnce(&State, &Terminal) -> Result<(T, Vec<Step>), RpcError>,
    ) -> Result<T, RpcError> {
        let queue = self.lock().queues.entry(id).or_default().clone();
        let _turn = queue.lock().await;
        loop {
            let left = self.lock().gates.get(&id).map_or(Duration::ZERO, |g| g.wait(quiet, submits));
            if left.is_zero() {
                break;
            }
            // Look again soon: sending or clearing a draft ends the hold early.
            tokio::time::sleep(left.min(Duration::from_millis(50))).await;
        }
        let (out, steps) = {
            let mut st = self.lock();
            let t = st.reg.terminals.get(&id).filter(|t| t.is_live()).ok_or_else(|| {
                RpcError::new(
                    "finished",
                    format!("{} has finished", crate::terminals::address(id)),
                    format!("hc read {}", crate::terminals::address(id)),
                )
            })?;
            t.check_program()?;
            let prepared = prepare(&st, t)?;
            st.gates.entry(id).or_default().busy = true;
            prepared
        };
        for step in steps {
            match step {
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
        gate.busy = false;
        let held = std::mem::take(&mut gate.held);
        if let Some(link) = st.link(id) {
            link.input(id, &held);
        }
        Ok(out)
    }

    /// Paste + Enter, after a person has paused typing. Refuses a blocked
    /// program (typing would answer its dialog) unless `force`.
    pub async fn send(&self, id: u32, text: &str, force: bool, only_if: Option<&str>) -> Result<Sent, RpcError> {
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
                let steps =
                    vec![Step::Write(paste(screen.dec_mode(2004), text)), Step::Pause(gap), Step::Write(submit)];
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
                steps.push(Step::Write(crate::keys::encode(screen, name)?));
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
    fn pastes_are_bracketed_only_when_asked_and_cannot_end_early() {
        assert_eq!(paste(true, "hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste(false, "hi"), b"hi");
        assert_eq!(paste(true, "a\x1b[201~b"), b"\x1b[200~ab\x1b[201~");
    }

    #[test]
    fn a_draft_lasts_until_the_person_sends_or_clears_it() {
        let draft = |keys: &[&[u8]]| {
            let mut g = Gate::default();
            keys.iter().for_each(|k| g.track(k));
            g.draft
        };
        assert_eq!(draft(&[b"hey", "é".as_bytes()]), 4);
        assert_eq!(draft(&[b"hey\r"]), 0, "Enter sends it");
        assert_eq!(draft(&[b"hey", b"\x15"]), 0, "ctrl+u clears it");
        assert_eq!(draft(&[b"hi", b"\x7f\x7f\x7f"]), 0, "backspaced away");
        assert_eq!(draft(&[b"\x1b[A\x1bOB\x1bb\x1b[13;2u"]), 0, "arrows, alt+b, shift+enter type nothing");
        assert_eq!(draft(&[b"\x1b[200~a\rb\x1b[201~"]), 3, "a pasted Enter doesn't send");
        assert_eq!(draft(&[b"\x1b[200~ab", b"c\x1b[201~", b"\r"]), 0, "a paste across reads, then sent");
    }
}
