//! Terminal records: ids, names, labels, lifecycle facts and retention.
//! Pure state, no IO; the core persists `Registry` as JSON.

use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RETAIN_MS: u64 = 24 * 60 * 60 * 1000;
pub const MAX_FINISHED: usize = 500;
pub const KEEP_RETRY_MS: u64 = 1500;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Terminal {
    /// `t7`: this terminal forever on this core.
    pub address: String,
    pub name: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub cwd: String,
    /// Replace a lost program with this machine’s shell, in place. Exit with a code and kill remain final.
    #[serde(default)]
    pub keep: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cwd: Option<String>,
    /// Only caller overrides, never the inherited core environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub restart_env: BTreeMap<String, String>,
    #[serde(default)]
    pub restart_after_ms: u64,
    #[serde(default)]
    pub restart_delay_ms: u64,
    #[serde(default)]
    pub program_started_ms: Option<u64>,
    /// "live" or "finished".
    pub state: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Why its program ended or is pending (`spawn_pending` means its initial
    /// spawn timed out, but the holder may still be starting it):
    /// exited, killed, server_stop, host_shutdown (Windows
    /// is ending the session, including logout), signaled (a kept program
    /// died by signal without an exit code), holder_lost (its holder
    /// died), stopped_by_restart (nothing ran it any more after a restart).
    pub reason: Option<String>,
    pub created_ms: u64,
    pub finished_ms: Option<u64>,
    /// "exact", or "inexact" after a core crash (older history may be missing).
    pub screen: String,
    pub cols: u16,
    pub rows: u16,
    /// Which terminal the program believes it runs in (TERM_PROGRAM and
    /// version), fixed at start; XTVERSION answers with it.
    #[serde(default)]
    pub identity: String,
    /// The program's own word for what it's doing (OSC 7501 root record);
    /// null if it never said, or after it exited.
    #[serde(default)]
    pub status: Option<crate::status::Status>,
    /// When that word last changed, ms since the epoch; null if it never said.
    #[serde(default)]
    pub status_ms: Option<u64>,
    /// Other 7501 records by id (programs reporting on several things).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub records: BTreeMap<String, crate::status::Status>,
    /// Goes up each time the status leaves `working`.
    #[serde(default)]
    pub turn: u64,
    /// Whether the program ever sent a 7501 report.
    #[serde(default)]
    pub reported: bool,
    #[serde(default)]
    pub title: String,
    /// The terminal that started it (`t3`), set once at start.
    #[serde(default)]
    pub parent: Option<String>,
    /// Its parent gets notes about it (see `--no-notify`).
    #[serde(default)]
    pub notify: bool,
    #[serde(default)]
    pub die_with_parent: bool,
    /// Removed right after it exits (once a waiting parent has the exit code).
    #[serde(default)]
    pub rm: bool,
    /// Notable events so far: turns that ended, blocks, the exit.
    #[serde(default)]
    pub news: u64,
    /// How many of those its parent has seen (read, get, wait, send, keys, kill).
    #[serde(default)]
    pub seen: u64,
    /// How many of those a note already told its parent about.
    #[serde(default)]
    pub noted: u64,
    /// It messaged its parent during this turn: the turn's end tells nothing new.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub told_parent: bool,
    /// Lines it printed, at least: more than its history holds means some
    /// were dropped, so a read reaching back that far is not complete.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub printed: u64,
    /// It used the full screen, whose history the app keeps to itself: once
    /// it has exited, nobody can scroll it back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub full_screen: bool,
    /// The program asked for win32-input-mode (ConPTY does, at start): a new
    /// viewer must hear it too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub win32_input: bool,
    /// The `HC_SCOPE` token everything it started carries, to end them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub seen_ms: Option<u64>,
    /// The template that started it.
    #[serde(default)]
    pub template: Option<String>,
    /// The holder (its instance) that runs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// Template overrides: the keys that submit a message, the stop key, the pause
    /// between paste and submit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paste_gap_ms: Option<u64>,
    /// What runs in it right now and where (null once it has finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<crate::foreground::Foreground>,
    /// Machines (core ids) it started work on: they may call back to it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub afar: std::collections::BTreeSet<String>,
}

impl Terminal {
    pub fn id(&self) -> u32 {
        parse_id(&self.address).expect("addresses are t<number>")
    }
    pub fn is_live(&self) -> bool {
        self.state == "live"
    }

    pub(crate) fn program_lost(&self) -> bool {
        self.is_live() && self.pid.is_none() && self.reason.as_deref().is_some_and(|r| r != "spawn_pending")
    }

    pub fn check_program(&self) -> Result<(), RpcError> {
        if self.is_live() && self.pid.is_none() && self.reason.is_some() {
            return Err(RpcError::new(
                "unavailable",
                format!("{} is lost, waiting for its new shell", self.address),
                "retry after the restarted event",
            ));
        }
        Ok(())
    }
}

/// After `t` and a first digit 1-9, ids count in these 30: no vowels (no
/// words), no `l` (reads as 1).
const SYMBOLS: &[u8; 30] = b"0123456789bcdfghjkmnpqrstvwxyz";

/// Counter `n` (from 1) as written: t1..t9, t10..t9z, t100..t9zz, ...
pub fn address(n: u32) -> String {
    let (mut rest, mut size, mut len) = (u64::from(n) - 1, 9u64, 0);
    while rest >= size {
        rest -= size;
        size *= 30;
        len += 1;
    }
    let mut tail = vec![0u8; len];
    for b in tail.iter_mut().rev() {
        *b = SYMBOLS[(rest % 30) as usize];
        rest /= 30;
    }
    format!("t{}{}", rest + 1, String::from_utf8(tail).expect("ascii"))
}

/// Shaped like an id, whether or not the number fits: names may not be.
pub fn id_shaped(local: &str) -> bool {
    let Some(rest) = local.strip_prefix('t') else { return false };
    rest.starts_with(|c: char| matches!(c, '1'..='9')) && rest.bytes().all(|b| SYMBOLS.contains(&b))
}

pub fn parse_id(local: &str) -> Option<u32> {
    if !id_shaped(local) {
        return None;
    }
    let mut symbols = local[1..].bytes().map(|b| SYMBOLS.iter().position(|&s| s == b).unwrap() as u64);
    let mut rest = symbols.next()? - 1;
    let (mut before, mut size) = (0u64, 9u64);
    for v in symbols {
        rest = rest.checked_mul(30)?.checked_add(v)?;
        before = before.checked_add(size)?;
        size = size.checked_mul(30)?;
    }
    u32::try_from(before.checked_add(rest)? + 1).ok()
}

#[derive(Serialize, Deserialize, Default)]
pub struct Registry {
    pub next_id: u32,
    /// The holder these terminals run under.
    pub holder: Option<String>,
    pub terminals: BTreeMap<u32, Terminal>,
}

#[derive(Default)]
pub struct NewTerminal {
    pub template: Option<crate::templates::Template>,
    pub parent: Option<String>,
    pub notify: bool,
    pub die_with_parent: bool,
    pub rm: bool,
    pub keep: bool,
    pub name: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub identity: String,
}

impl Registry {
    /// Validates and records a new live terminal (pid set once the holder spawned it).
    pub fn create(&mut self, new: NewTerminal, now_ms: u64) -> Result<u32, RpcError> {
        if let Some(name) = &new.name {
            crate::addr::check_name(name)?;
            if self.live_named(name).is_some() {
                return Err(RpcError::new(
                    "name_taken",
                    format!("a live terminal is already named {name:?}"),
                    "pick another name, or kill the one holding it",
                ));
            }
        }
        for key in new.labels.keys() {
            check_label_key(key)?;
        }
        if new.argv.is_empty() {
            return Err(RpcError::invalid_params("nothing to run"));
        }
        let id = self.next_id.max(1);
        self.next_id = id.checked_add(1).ok_or_else(|| {
            RpcError::new("unavailable", "this core has used every terminal id", "wipe its state to start over")
        })?;
        self.terminals.insert(
            id,
            Terminal {
                address: address(id),
                name: new.name,
                labels: new.labels.into_iter().filter(|(_, v)| !v.is_empty()).collect(),
                argv: new.argv,
                cwd: new.cwd,
                keep: new.keep,
                last_cwd: None,
                restart_env: BTreeMap::new(),
                restart_after_ms: 0,
                restart_delay_ms: 0,
                program_started_ms: None,
                state: "live".into(),
                pid: None,
                exit_code: None,
                signal: None,
                reason: None,
                created_ms: now_ms,
                finished_ms: None,
                screen: "exact".into(),
                cols: new.cols,
                rows: new.rows,
                identity: new.identity,
                status: None,
                status_ms: None,
                records: BTreeMap::new(),
                turn: 0,
                reported: false,
                title: String::new(),
                parent: new.parent,
                notify: new.notify,
                die_with_parent: new.die_with_parent,
                rm: new.rm,
                news: 0,
                seen: 0,
                noted: 0,
                told_parent: false,
                scope: None,
                printed: 0,
                full_screen: false,
                win32_input: false,
                seen_ms: None,
                submit: new.template.as_ref().and_then(|t| t.submit.clone()),
                stop: new.template.as_ref().and_then(|t| t.stop.clone()),
                paste_gap_ms: new.template.as_ref().and_then(|t| t.paste_gap_ms),
                foreground: None,
                afar: Default::default(),
                template: new.template.map(|t| t.name),
                holder: None,
            },
        );
        Ok(id)
    }

    fn live_named(&self, name: &str) -> Option<&Terminal> {
        self.terminals.values().find(|t| t.is_live() && t.name.as_deref() == Some(name))
    }

    /// An id means that terminal forever; a name means whoever holds it now,
    /// else the most recent finished terminal that had it.
    pub fn resolve(&self, local: &str) -> Result<u32, RpcError> {
        let found = match parse_id(local) {
            Some(id) => self.terminals.get(&id),
            None => self
                .live_named(local)
                .or_else(|| self.terminals.values().rev().find(|t| t.name.as_deref() == Some(local))),
        };
        found
            .map(Terminal::id)
            .ok_or_else(|| RpcError::new("not_found", format!("no terminal {local:?}"), "see `hc ls --all`"))
    }

    pub fn finish(&mut self, id: u32, code: Option<i32>, signal: Option<i32>, reason: &str, now_ms: u64) {
        if let Some(t) = self.terminals.get_mut(&id).filter(|t| t.is_live()) {
            let pending = t.keep
                && matches!(
                    reason,
                    "holder_lost" | "stopped_by_restart" | "server_stop" | "host_shutdown" | "signaled"
                );
            t.state = if pending { "live" } else { "finished" }.into();
            if let Some(f) = &t.foreground {
                t.last_cwd = Some(f.cwd.clone());
            }
            if pending {
                t.schedule_restart(now_ms);
                t.pid = None;
                t.holder = None;
            }
            t.exit_code = code;
            t.signal = signal;
            t.reason = Some(reason.into());
            t.finished_ms = (!pending).then_some(now_ms);
            t.foreground = None;
        }
    }

    /// Applies `key=value` changes; an empty value removes the key.
    pub fn label(&mut self, id: u32, changes: &BTreeMap<String, String>) -> Result<(), RpcError> {
        for key in changes.keys() {
            check_label_key(key)?;
        }
        let t = self.terminals.get_mut(&id).expect("resolved id");
        for (k, v) in changes {
            if v.is_empty() {
                t.labels.remove(k);
            } else {
                t.labels.insert(k.clone(), v.clone());
            }
        }
        Ok(())
    }

    /// Finished terminals past retention, for the caller to remove.
    /// Kept a day after its live parent has seen the result (no live parent:
    /// a day after it finished); kept while a live parent hasn't seen it.
    /// At most MAX_FINISHED, oldest first, whatever the rest says.
    pub fn expired(&self, now_ms: u64) -> Vec<u32> {
        let parent_live = |t: &Terminal| {
            let id = t.parent.as_deref().and_then(parse_id);
            id.and_then(|id| self.terminals.get(&id)).is_some_and(Terminal::is_live)
        };
        let mut finished: Vec<(u64, u32, Option<u64>)> = self
            .terminals
            .values()
            .filter_map(|t| {
                let at = t.finished_ms?;
                let clock = match parent_live(t) {
                    true if t.seen < t.news => None,
                    true => Some(t.seen_ms.unwrap_or(at).max(at)),
                    false => Some(at),
                };
                Some((at, t.id(), clock))
            })
            .collect();
        finished.sort();
        let over = finished.len().saturating_sub(MAX_FINISHED);
        finished
            .iter()
            .enumerate()
            .filter(|(i, (_, _, clock))| *i < over || clock.is_some_and(|c| now_ms.saturating_sub(c) > retain_ms()))
            .map(|(_, (_, id, _))| *id)
            .collect()
    }
}

/// The core's own bookkeeping in the record: saved with it, never shown.
const PRIVATE: &[&str] = &[
    "news",
    "seen",
    "noted",
    "seen_ms",
    "told_parent",
    "scope",
    "holder",
    "printed",
    "full_screen",
    "win32_input",
    "afar",
    "last_cwd",
    "restart_env",
    "restart_after_ms",
    "restart_delay_ms",
    "program_started_ms",
];

/// A terminal as clients see it (API results, events): its facts, without
/// the core's bookkeeping, which would only make it harder to keep stable.
#[derive(Clone)]
pub struct Public(pub Terminal);

impl Serialize for Public {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(&self.0).map_err(serde::ser::Error::custom)?;
        if let Some(fields) = value.as_object_mut() {
            PRIVATE.iter().for_each(|k| drop(fields.remove(*k)));
        }
        value.serialize(s)
    }
}

impl JsonSchema for Public {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Terminal".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = Terminal::json_schema(generator);
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            PRIVATE.iter().for_each(|k| drop(props.remove(*k)));
        }
        if let Some(required) = schema.get_mut("required").and_then(|r| r.as_array_mut()) {
            required.retain(|k| !k.as_str().is_some_and(|k| PRIVATE.contains(&k)));
        }
        schema
    }
}

/// `RETAIN_MS`, or `HC_RETAIN_MS` (tests can't wait a day).
fn retain_ms() -> u64 {
    std::env::var("HC_RETAIN_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(RETAIN_MS)
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn check_label_key(key: &str) -> Result<(), RpcError> {
    if key.is_empty() || key.contains('=') {
        return Err(RpcError::invalid_params(format!("bad label key {key:?}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(name: Option<&str>) -> NewTerminal {
        NewTerminal { name: name.map(Into::into), argv: vec!["true".into()], cwd: "/".into(), ..Default::default() }
    }

    #[test]
    fn keep_preserves_losses_without_retention_or_rm_but_exit_and_kill_are_final() {
        for reason in
            ["holder_lost", "stopped_by_restart", "host_shutdown", "server_stop", "signaled", "exited", "killed"]
        {
            for keep in [false, true] {
                let mut r = Registry::default();
                let id = r.create(NewTerminal { keep, rm: true, ..new(None) }, 0).unwrap();
                r.finish(id, Some(3), None, reason, 5);
                let pending = keep && !matches!(reason, "exited" | "killed");
                assert_eq!(r.terminals[&id].is_live(), pending, "{reason}, keep={keep}");
                assert_eq!(r.expired(RETAIN_MS * 5).is_empty(), pending);
                assert_eq!(r.terminals[&id].finished_ms.is_none(), pending);
            }
        }
    }

    #[test]
    fn a_child_is_kept_until_its_live_parent_has_seen_it() {
        let mut r = Registry::default();
        let parent = r.create(new(None), 0).unwrap();
        let child = r.create(NewTerminal { parent: Some(address(parent)), ..new(None) }, 0).unwrap();
        r.finish(child, Some(0), None, "exited", 0);
        r.terminals.get_mut(&child).unwrap().news = 2;
        assert!(r.expired(RETAIN_MS * 5).is_empty(), "unseen by a live parent");
        let c = r.terminals.get_mut(&child).unwrap();
        c.seen = 2;
        c.seen_ms = Some(RETAIN_MS * 5);
        assert!(r.expired(RETAIN_MS * 5 + 10).is_empty(), "a day after it was seen");
        assert_eq!(r.expired(RETAIN_MS * 6 + 10), vec![child]);
        // Without a live parent, the day runs from the finish.
        r.terminals.get_mut(&child).unwrap().seen = 0;
        r.finish(parent, Some(0), None, "exited", 0);
        assert!(r.expired(RETAIN_MS + 10).contains(&child));
    }

    #[test]
    fn ids_are_never_reused_and_names_follow_their_live_holder() {
        let mut r = Registry::default();
        let a = r.create(new(Some("w")), 0).unwrap();
        assert_eq!(r.create(new(Some("w")), 0).unwrap_err().kind, "name_taken");
        r.finish(a, Some(0), None, "exited", 1);
        assert_eq!(r.resolve("w").unwrap(), a);
        let b = r.create(new(Some("w")), 2).unwrap();
        assert_eq!(r.resolve("w").unwrap(), b);
        r.terminals.remove(&b);
        assert_eq!(r.create(new(None), 3).unwrap(), b + 1);
        assert_eq!(r.resolve("t99").unwrap_err().kind, "not_found");
    }

    #[test]
    fn ids_count_with_a_digit_then_digits_and_consonants() {
        for (n, a) in
            [(1, "t1"), (9, "t9"), (10, "t10"), (19, "t19"), (20, "t1b"), (39, "t1z"), (40, "t20"), (279, "t9z")]
        {
            assert_eq!(address(n), a);
        }
        for (n, a) in [(280, "t100"), (8379, "t9zz"), (8380, "t1000"), (148377, "t65jk")] {
            assert_eq!(address(n), a);
            assert_eq!(parse_id(a), Some(n), "{a}");
        }
        assert_eq!(parse_id(&address(u32::MAX)), Some(u32::MAX));
        for not_id in ["t", "t0", "t01", "tb", "t1l", "t1a", "T1", "t1B", "x1", "tmp", "t3po"] {
            assert_eq!(parse_id(not_id), None, "{not_id}");
        }
        assert!(id_shaped("t9zzzzzzzzzz") && parse_id("t9zzzzzzzzzz").is_none(), "too big is still id-shaped");
        let mut r = Registry { next_id: u32::MAX - 1, ..Default::default() };
        r.create(new(None), 0).unwrap();
        assert_eq!(r.create(new(None), 0).unwrap_err().kind, "unavailable");
    }

    #[test]
    fn retention_keeps_a_day_and_at_most_500_finished() {
        let mut r = Registry::default();
        for i in 0..502u64 {
            let id = r.create(new(None), i).unwrap();
            r.finish(id, Some(0), None, "exited", i);
        }
        let live = r.create(new(None), 0).unwrap();
        assert_eq!(r.expired(1000), vec![1, 2]);
        // Over the cap, plus finished at 0..=99 now over a day old.
        let old = r.expired(RETAIN_MS + 100);
        assert_eq!(old.len(), 100);
        assert!(!old.contains(&live));
    }
}
