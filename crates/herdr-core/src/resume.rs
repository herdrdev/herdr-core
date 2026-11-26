//! Private, per-PTY program resume state. A report is never a launch by itself.
mod launch;
mod protocol;

pub(crate) use launch::{helper, remove_launch};
use protocol::Stream;

use crate::platform::{self, ProcessState};
use crate::runtime::{Core, State};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub(crate) const TOKEN: &str = "OSC7577_TOKEN";
const EXIT_GRACE_MS: u64 = 2000;
const RAPID_LOSS_MS: u64 = 10_000;

pub(crate) fn prepare_state(state: &std::path::Path, registry: &mut crate::terminals::Registry) {
    if platform::secure_state_dir(state).is_ok() {
        launch::remove_abandoned(state, |name| {
            registry.terminals.values().any(|t| t.resume.as_ref().and_then(|r| r.launch.as_deref()) == Some(name))
        });
    } else {
        for terminal in registry.terminals.values_mut() {
            terminal.resume = None;
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Identity {
    pub pid: u32,
    pub start: u64,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub(crate) struct Resume {
    token: String,
    user: String,
    root: Option<Identity>,
    words: BTreeMap<u32, Word>,
    #[serde(skip)]
    stream: Stream,
    pub launch: Option<String>,
    pub rearmed: bool,
}

impl fmt::Debug for Resume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Resume([private])")
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Word {
    time: u64,
    offset: u64,
    record: Option<Record>,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
struct Record {
    owner: Identity,
    ancestors: Vec<Identity>,
    argv: Vec<String>,
    reported_cwd: Option<String>,
    owner_cwd: Option<String>,
    gone_since: Option<u64>,
}

impl Resume {
    pub(crate) fn new(state: &std::path::Path) -> Option<Self> {
        let user = platform::resume_user()?;
        platform::secure_state_dir(state).ok()?;
        Some(Self {
            token: platform::random_hex(),
            user,
            root: None,
            words: BTreeMap::new(),
            stream: Stream::default(),
            launch: None,
            rearmed: false,
        })
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn set_root(&mut self, pid: u32) {
        if self.root.is_none()
            && let ProcessState::Alive(p) = platform::resume_process(pid)
            && p.user == self.user
        {
            self.root = Some(p.identity);
        }
    }

    pub(crate) fn adopt(&mut self, pid: u32, raw_offset: u64) {
        self.set_root(pid);
        self.stream = Stream::at(raw_offset);
    }

    fn owner(&self, pid: u32) -> Option<(Identity, Vec<Identity>, Option<String>)> {
        let root = self.root?;
        let ProcessState::Alive(owner) = platform::resume_process(pid) else { return None };
        if owner.user != self.user {
            return None;
        }
        let mut p = &owner;
        let mut parent;
        let mut ancestors = Vec::new();
        while p.identity != root {
            if p.identity.pid == root.pid || p.parent == 0 || ancestors.len() == 128 {
                return None;
            }
            let ProcessState::Alive(next) = platform::resume_process(p.parent) else { return None };
            if next.identity.start > p.identity.start || next.identity.pid == p.identity.pid {
                return None;
            }
            ancestors.push(next.identity);
            parent = next;
            p = &parent;
        }
        let cwd = crate::foreground::process_cwd(pid as i32);
        Some((owner.identity, ancestors, cwd))
    }

    fn apply(
        &mut self,
        offset: u64,
        report: protocol::Report,
        now: u64,
        find_owner: impl FnOnce(&Self, u32) -> Option<(Identity, Vec<Identity>, Option<String>)>,
    ) -> bool {
        let len = self.words.len();
        self.words.retain(|_, word| word.record.is_some() || now.saturating_sub(word.time) <= 60);
        let pruned = self.words.len() != len;
        let previous = self.words.get(&report.pid);
        if previous.map_or(self.words.len() >= 256, |word| offset <= word.offset || report.time < word.time) {
            return pruned;
        }
        let clear = matches!(report.action, Some(protocol::Action::Clear));
        let record = match report.action {
            Some(protocol::Action::Set { argv, cwd }) => {
                find_owner(self, report.pid).map(|(owner, ancestors, owner_cwd)| Record {
                    owner,
                    ancestors,
                    argv,
                    reported_cwd: cwd,
                    owner_cwd,
                    gone_since: None,
                })
            }
            _ => None,
        };
        // Rejecting is a removal, but only accepted reports advance the time gate.
        let accepted = record.is_some() || clear;
        let time = if accepted { report.time } else { previous.map_or(0, |word| word.time) };
        self.words.insert(report.pid, Word { time, offset, record });
        true
    }

    pub(crate) fn lost(&mut self, started: Option<u64>, now: u64, root_signaled: bool) {
        if self.rearmed && started.is_some_and(|ms| now.saturating_sub(ms) < RAPID_LOSS_MS) {
            self.words.clear();
        } else if root_signaled && let Some(root) = self.root {
            self.words.remove(&root.pid);
        }
    }

    fn choice(&self) -> Option<&Record> {
        let records = || self.words.values().filter_map(|w| Some((w.offset, w.record.as_ref()?)));
        records()
            .filter(|(_, r)| !records().any(|(_, ancestor)| r.ancestors.contains(&ancestor.owner)))
            .max_by_key(|(offset, _)| *offset)
            .map(|(_, record)| record)
    }

    pub(crate) fn prepare(
        &self,
        state: &std::path::Path,
        id: u32,
        shell: &[String],
        cwd: &str,
    ) -> Option<(String, Vec<String>, String)> {
        if platform::resume_user().as_ref() != Some(&self.user) {
            return None;
        }
        let record = self.choice()?;
        launch::prepare(state, id, shell, cwd, record).ok()
    }

    fn owners(&self) -> impl Iterator<Item = Identity> + '_ {
        self.words.values().filter_map(|w| w.record.as_ref().map(|r| r.owner))
    }

    fn check_exit(&mut self, owner: Identity, gone: bool, now: u64) -> bool {
        let Some(word) = self.words.get_mut(&owner.pid) else { return false };
        let Some(record) = word.record.as_mut().filter(|r| r.owner == owner) else { return false };
        match record.gone_since {
            Some(ms) if gone && now.saturating_sub(ms) >= EXIT_GRACE_MS => word.record = None,
            Some(_) if !gone => record.gone_since = None,
            None if gone => record.gone_since = Some(now),
            _ => return false,
        }
        true
    }
}

impl Core {
    pub(crate) fn resume_output(&self, st: &mut State, id: u32, bytes: &[u8], now: u64) {
        let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.is_live() && !st.killing.contains(&id)) else {
            return;
        };
        let Some(resume) = &mut t.resume else { return };
        let seconds = now / 1000;
        let mut changed = false;
        for (offset, bytes) in resume.stream.feed(bytes) {
            if let Some(report) = protocol::authenticate(&bytes, resume.token(), seconds) {
                changed |= resume.apply(offset, report, seconds, Resume::owner);
            }
        }
        if changed {
            self.save(st);
        }
    }

    pub(crate) fn reconcile_resume(&self, id: u32) {
        let healthy = |st: &State| {
            !st.stopping
                && !st.killing.contains(&id)
                && st.link(id).is_some()
                && !self.restart.load(std::sync::atomic::Ordering::SeqCst)
                && !platform::host_shutdown_in_progress()
        };
        let (scope, owners) = {
            let st = self.lock();
            let Some(t) = st.reg.terminals.get(&id).filter(|_| healthy(&st)) else { return };
            let Some(resume) = &t.resume else { return };
            (t.scope.clone(), resume.owners().collect::<Vec<_>>())
        };
        let observations = owners
            .into_iter()
            .filter_map(|owner| {
                let gone = match platform::resume_process(owner.pid) {
                    ProcessState::Alive(p) => p.identity != owner,
                    ProcessState::Gone => true,
                    ProcessState::Unknown => return None,
                };
                Some((owner, gone))
            })
            .collect::<Vec<_>>();
        if observations.is_empty() {
            return;
        }
        let mut st = self.lock();
        if !healthy(&st) {
            return;
        }
        let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.scope == scope) else { return };
        let Some(resume) = &mut t.resume else { return };
        let now = crate::runtime::now_ms();
        let mut changed = false;
        for (owner, gone) in observations {
            changed |= resume.check_exit(owner, gone, now);
        }
        if changed {
            self.save(&st);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Action, Report};

    fn state() -> Resume {
        Resume {
            token: "private-token".into(),
            user: "user".into(),
            root: None,
            words: BTreeMap::new(),
            stream: Stream::default(),
            launch: None,
            rearmed: false,
        }
    }

    fn owner(pid: u32, start: u64) -> Identity {
        Identity { pid, start }
    }

    fn set(state: &mut Resume, offset: u64, time: u64, owner: Identity, ancestors: &[Identity]) -> bool {
        let action = Some(Action::Set { argv: vec!["private-command".into()], cwd: None });
        state.apply(offset, Report { pid: owner.pid, time, action }, 60, |_, _| Some((owner, ancestors.to_vec(), None)))
    }

    #[test]
    fn replay_preserves_the_last_word_and_related_owners_prefer_an_ancestor() {
        let mut s = state();
        let (parent, child, other) = (owner(1, 1), owner(2, 2), owner(3, 3));
        let clear = || Report { pid: 1, time: 60, action: Some(Action::Clear) };
        assert!(set(&mut s, 10, 60, parent, &[]));
        assert!(set(&mut s, 20, 60, child, &[parent]));
        assert_eq!(s.choice().unwrap().owner, parent);
        assert!(set(&mut s, 30, 60, other, &[]));
        assert_eq!(s.choice().unwrap().owner, other);
        assert!(s.apply(40, Report { pid: 3, time: 60, action: None }, 60, |_, _| None));
        assert_eq!(s.choice().unwrap().owner, parent);
        assert!(s.apply(50, clear(), 60, |_, _| None));
        assert!(!set(&mut s, 10, 60, parent, &[]), "the tail repeats an already consumed report");
        assert!(!s.apply(50, clear(), 60, |_, _| None), "it repeats the clear too");
        assert!(!set(&mut s, 60, 59, parent, &[]), "older time is ignored even when emitted later");
        assert_eq!(s.choice().unwrap().owner, child);
        let reused = owner(1, 70);
        assert!(set(&mut s, 70, 60, reused, &[]), "new reports in the same second still apply");
        assert!(set(&mut s, 80, 60, child, &[parent]));
        assert_eq!(s.choice().unwrap().owner, child, "a reused PID is not its ancestor");
    }

    #[test]
    fn owner_exit_gets_grace_and_rapid_loss_clears_rearmed_records() {
        let mut s = state();
        let owner = owner(1, 1);
        set(&mut s, 10, 60, owner, &[]);
        assert!(s.check_exit(owner, true, 60_000));
        assert!(!s.check_exit(owner, true, 61_999));
        assert!(s.choice().is_some(), "loss during the grace can resume");
        assert!(s.check_exit(owner, true, 62_000));
        assert!(s.choice().is_none());

        set(&mut s, 11, 60, owner, &[]);
        s.rearmed = true;
        s.lost(Some(60_000), 70_000, false);
        assert!(s.choice().is_some());
        s.lost(Some(60_000), 69_999, false);
        assert!(s.choice().is_none());
    }

    #[test]
    fn terminal_projection_never_serializes_resume_data() {
        let mut s = state();
        set(&mut s, 10, 60, owner(1, 1), &[]);
        let mut reg = crate::terminals::Registry::default();
        let id =
            reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap();
        reg.terminals.get_mut(&id).unwrap().resume = Some(s);
        let saved = serde_json::to_string(&reg).unwrap();
        let reg: crate::terminals::Registry = serde_json::from_str(&saved).unwrap();
        assert!(saved.contains("private-token") && saved.contains("private-command"));
        let t = &reg.terminals[&id];
        assert!(!format!("{t:?}").contains("private-token"));
        let public = serde_json::to_string(&crate::terminals::Public(t.clone())).unwrap();
        assert!(!public.contains("resume") && !public.contains("private-command") && !public.contains("private-token"));
    }
}
