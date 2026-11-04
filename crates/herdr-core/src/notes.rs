//! Parents and children: news about a child is typed into its parent agent
//! as a marked note, only when the parent can take it and hasn't seen it.

use crate::input::{Step, paste};
use crate::rpc::RpcError;
use crate::runtime::{Core, State, now_ms};
use crate::terminals::{Terminal, parse_id};
use std::sync::Arc;
use std::time::Duration;

/// Work that can't happen on the thread that noticed it.
pub enum Job {
    /// Try to type pending notes into this parent.
    Notes(u32),
    /// The parent finished: end children that asked to die with it.
    Orphaned(u32),
    /// News for a parent on a core that dialed in (`desktop/t3`).
    NotesAfar(String),
}

fn parent_id(t: &Terminal) -> Option<u32> {
    t.parent.as_deref().and_then(parse_id)
}

fn says(t: &Terminal, word: &str) -> bool {
    t.is_live() && t.status.as_ref().is_some_and(|s| s.state == word)
}

fn blocked(t: &Terminal) -> bool {
    says(t, "blocked")
}

/// Blocked or errored: stuck in a way it can't report itself.
fn stuck(t: &Terminal) -> bool {
    blocked(t) || says(t, "error")
}

/// The last lines a plain program wrote, for its exit note: blanks dropped,
/// long lines cut, no control characters (the note is typed in).
fn last_lines(text: &str) -> Vec<String> {
    const LINES: usize = 20;
    const WIDTH: usize = 300;
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .map(|l| l.chars().filter(|c| !c.is_control() || *c == '\t').take(WIDTH).collect::<String>())
        .filter(|l| !l.trim().is_empty())
        .take(LINES)
        .collect();
    lines.reverse();
    lines
}

/// A parent can take a note: a live agent not on a dialog (typing could answer
/// it). A working agent takes typed text as a message, like any steer.
fn can_take_note(t: &Terminal) -> bool {
    t.is_live() && t.reported && !blocked(t)
}

/// `t7 reviewer`: address, then name or template.
pub fn who(t: &Terminal) -> String {
    match t.name.as_ref().or(t.template.as_ref()) {
        Some(name) => format!("{} {name}", t.address),
        None => t.address.clone(),
    }
}

/// What a child can't say itself: it is blocked, it hit an error, or it is
/// gone. Everything else it reports with hc send. `address`: the child as
/// its parent calls it (`rohan/t7` from another machine). `last`: the last
/// lines of a plain program that ended, shown before where to read more.
fn note(t: &Terminal, address: &str, last: &[String]) -> String {
    let status = t.status.as_ref();
    let (what, look) = if blocked(t) {
        let kind = status.and_then(|s| s.kind.as_deref()).unwrap_or("waiting");
        (format!("is blocked ({kind})"), "See")
    } else if stuck(t) {
        match status.and_then(|s| s.msg.lines().next()).map(|m| m.trim_end_matches('.')).filter(|m| !m.is_empty()) {
            Some(msg) => (format!("hit an error: {}", msg.chars().take(200).collect::<String>()), "See"),
            None => ("hit an error".into(), "See"),
        }
    } else {
        let how = match (t.exit_code, t.signal) {
            (Some(code), _) => format!("exit {code}"),
            (_, Some(sig)) => format!("signal {sig}"),
            _ => t.reason.clone().unwrap_or_else(|| "ended".into()),
        };
        (format!("exited ({how})"), "Read")
    };
    let name = t.name.as_ref().or(t.template.as_ref()).map(|n| format!(" {n}")).unwrap_or_default();
    let gap = match last.is_empty() {
        true => ". ".to_string(),
        false => format!(". Last lines:\n{}\n", last.join("\n")),
    };
    format!("<hc>{address}{name} {what}{gap}{look}: hc read {address}</hc>")
}

impl Core {
    /// The note about child `id`, called `address` by its parent.
    fn note_for(&self, st: &State, id: u32, address: &str) -> String {
        let t = &st.reg.terminals[&id];
        // Any end the parent hasn't seen is news, a success too: a background
        // build that worked is what it waits to hear. A plain program's shows its last lines.
        let last = match !t.is_live() && !t.reported {
            true => last_lines(&self.screen_text(st, id).unwrap_or_default()),
            false => Vec::new(),
        };
        note(t, address, &last)
    }

    /// Something notable happened to `id` (a turn ended, it got stuck, it exited).
    pub fn news(&self, st: &mut State, id: u32) {
        let Some(t) = st.reg.terminals.get_mut(&id) else { return };
        t.news += 1;
        if std::mem::take(&mut t.told_parent) && !stuck(t) && t.is_live() {
            t.seen = t.news; // a turn that ended after reporting to the parent
        }
        match (parent_id(t), &t.parent) {
            (Some(p), _) if t.notify => drop(self.jobs.send(Job::Notes(p))),
            (None, Some(afar)) if t.notify => drop(self.jobs.send(Job::NotesAfar(afar.clone()))),
            _ => {}
        }
    }

    /// The caller looked at `id`; if it is the parent, that news is seen.
    pub fn saw(&self, caller: Option<&str>, id: u32) {
        let mut st = self.lock();
        if let Some(t) = st.reg.terminals.get_mut(&id)
            && caller.is_some()
            && t.parent.as_deref() == caller
            && t.seen < t.news
        {
            t.seen = t.news;
            t.seen_ms = Some(now_ms());
            self.save(&st);
        }
    }

    /// Text from the agent `from` (a caller address) to the agent `to`, wrapped in
    /// a tag that says who sent it. As is when either isn't an agent: a person,
    /// a shell, a dialog. `reach`: also say how to answer (a worker's first message).
    pub fn tagged(&self, from: Option<&str>, to: u32, text: &str, reach: bool) -> String {
        // `notes = false`: nothing hc adds on its own, tags included.
        if !self.notes_on() {
            return text.to_string();
        }
        let st = self.lock();
        let sender = from.and_then(parse_id).filter(|f| *f != to).and_then(|f| st.reg.terminals.get(&f));
        let target = st.reg.terminals.get(&to);
        // A child on another machine calling back (`rohan/t7`).
        if let Some(afar) = from.filter(|f| f.contains('/'))
            && target.is_some_and(|t| t.reported)
        {
            return format!("<hc from=\"{afar}\">{text}</hc>");
        }
        match sender.filter(|s| s.reported && target.is_some_and(|t| t.reported)) {
            Some(s) if reach => {
                format!("<hc from=\"{}\" reach=\"hc send {} when done or stuck\">{text}</hc>", who(s), s.address)
            }
            Some(s) => format!("<hc from=\"{}\">{text}</hc>", who(s)),
            None => text.to_string(),
        }
    }

    /// Children of `parent` that are stuck or gone with news it hasn't seen,
    /// been told, or isn't already waiting for.
    fn unseen(&self, st: &State, address: &str) -> Vec<u32> {
        let address = address.to_string();
        st.reg
            .terminals
            .values()
            .filter(|t| t.parent.as_deref() == Some(address.as_str()) && t.notify)
            .filter(|t| t.news > t.seen.max(t.noted) && (stuck(t) || !t.is_live()))
            .filter(|t| !st.waiting.contains_key(&(address.clone(), t.id())))
            .map(Terminal::id)
            .collect()
    }

    /// Runs jobs for as long as the core runs.
    pub async fn run_jobs(self: Arc<Self>, mut jobs: tokio::sync::mpsc::UnboundedReceiver<Job>) {
        while let Some(job) = jobs.recv().await {
            match job {
                Job::Notes(parent) => {
                    let start = self.lock().note_tasks.insert(parent);
                    if start {
                        tokio::spawn(self.clone().deliver_notes(parent));
                    }
                }
                Job::NotesAfar(parent) => {
                    let mut st = self.lock();
                    let Some(called) = crate::remote::called_there(&st, &parent) else { continue };
                    let kids = self.unseen(&st, &parent);
                    if kids.is_empty() {
                        continue;
                    }
                    let notes: Vec<String> = kids
                        .iter()
                        .map(|id| self.note_for(&st, *id, &format!("{called}/{}", crate::terminals::address(*id))))
                        .collect();
                    for id in &kids {
                        let t = st.reg.terminals.get_mut(id).expect("listed");
                        t.noted = t.noted.max(t.news);
                    }
                    self.save(&st);
                    crate::remote::tell_peer(&self, &mut st, &parent, &notes);
                }
                Job::Orphaned(parent) => {
                    let address = crate::terminals::address(parent);
                    let kids: Vec<u32> = {
                        let mut st = self.lock();
                        let kids: Vec<u32> = st
                            .reg
                            .terminals
                            .values()
                            .filter(|t| {
                                t.is_live() && t.die_with_parent && t.parent.as_deref() == Some(address.as_str())
                            })
                            .map(Terminal::id)
                            .collect();
                        st.killing.extend(&kids);
                        kids
                    };
                    let core = self.clone();
                    tokio::spawn(async move { core.kill(&kids).await });
                }
            }
        }
    }

    /// Types notes into `parent` whenever it can take them, until there are none.
    fn notes_on(&self) -> bool {
        crate::config::load(&self.paths.config).map(|c| c.notes.unwrap_or(true)).unwrap_or(true)
    }

    async fn deliver_notes(self: Arc<Self>, parent: u32) {
        let enabled = self.notes_on();
        loop {
            let ready = |st: &State| st.reg.terminals.get(&parent).is_none_or(|t| !t.is_live() || can_take_note(t));
            while !self.wait_for(Duration::from_secs(60), ready).await {}
            let alive = self.lock().reg.terminals.get(&parent).is_some_and(|t| t.is_live());
            if !alive || !enabled {
                break;
            }
            match self.type_note(parent).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(_) => continue, // it got busy while we waited for quiet: try again
            }
            let mut st = self.lock();
            if self.unseen(&st, &crate::terminals::address(parent)).is_empty() && !st.afar_notes.contains_key(&parent) {
                st.note_tasks.remove(&parent);
                return;
            }
        }
        self.lock().note_tasks.remove(&parent);
    }

    /// One batch of notes, built at typing time so stale news is dropped.
    async fn type_note(&self, parent: u32) -> Result<bool, RpcError> {
        let quiet = crate::config::load(&self.paths.config)
            .ok()
            .and_then(|c| c.notes_quiet)
            .map_or_else(crate::input::quiet, |s| Duration::from_secs_f64(s.max(0.0)));
        // Notes from other machines were decided there: type them as they are.
        let afar = self.lock().afar_notes.remove(&parent).unwrap_or_default();
        let typed = self
            .type_in(parent, quiet, |st, t| {
                if !can_take_note(t) {
                    return Err(RpcError::new("busy", "the parent got busy", ""));
                }
                let kids = self.unseen(st, &crate::terminals::address(parent));
                if kids.is_empty() && afar.is_empty() {
                    return Ok((Vec::new(), Vec::new()));
                }
                let mut text: Vec<String> =
                    kids.iter().map(|id| self.note_for(st, *id, &st.reg.terminals[id].address)).collect();
                text.extend(afar.iter().cloned());
                let bracketed = st.screens.get(&parent).is_some_and(|s| s.dec_mode(2004));
                let told = kids.iter().map(|id| (*id, st.reg.terminals[id].news)).collect();
                let steps = vec![
                    Step::Write(paste(bracketed, &text.join("\n"))),
                    Step::Pause(crate::input::PASTE_GAP),
                    Step::Write(b"\r".to_vec()),
                ];
                Ok((told, steps))
            })
            .await;
        let told: Vec<(u32, u64)> = match typed {
            Ok(told) => told,
            Err(e) => {
                if !afar.is_empty() {
                    self.lock().afar_notes.entry(parent).or_default().splice(0..0, afar);
                }
                return Err(e);
            }
        };
        let had_afar = !afar.is_empty();
        let mut st = self.lock();
        for (id, news) in &told {
            if let Some(t) = st.reg.terminals.get_mut(id) {
                t.noted = t.noted.max(*news);
            }
        }
        self.save(&st);
        Ok(!told.is_empty() || had_afar)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::{NewTerminal, Registry};

    #[test]
    fn notes_say_what_happened_and_where_to_look() {
        let mut r = Registry::default();
        let id = r
            .create(NewTerminal { name: Some("reviewer".into()), argv: vec!["x".into()], ..Default::default() }, 0)
            .unwrap();
        let t = r.terminals.get_mut(&id).unwrap();
        t.turn = 2;
        t.status = Some(crate::status::Status {
            state: "done".into(),
            kind: None,
            progress: None,
            app: String::new(),
            title: String::new(),
            msg: String::new(),
        });
        t.status.as_mut().unwrap().state = "blocked".into();
        t.status.as_mut().unwrap().kind = Some("permission".into());
        assert_eq!(note(t, "t1", &[]), "<hc>t1 reviewer is blocked (permission). See: hc read t1</hc>");
        t.status.as_mut().unwrap().state = "error".into();
        t.status.as_mut().unwrap().msg = format!("rate limited\n{}", "x".repeat(500));
        assert_eq!(
            note(t, "t1", &[]),
            "<hc>t1 reviewer hit an error: rate limited. See: hc read t1</hc>",
            "its first line"
        );
        t.status.as_mut().unwrap().msg.clear();
        assert_eq!(note(t, "t1", &[]), "<hc>t1 reviewer hit an error. See: hc read t1</hc>");
        r.finish(id, Some(3), None, "exited", 1);
        assert_eq!(
            note(&r.terminals[&id], "rohan/t1", &[]),
            "<hc>rohan/t1 reviewer exited (exit 3). Read: hc read rohan/t1</hc>"
        );
        let last = last_lines("a\n\n  \nb\x1b[201~\tc\n");
        assert_eq!(last, ["a", "b[201~\tc"], "blanks and control characters dropped");
        assert_eq!(
            note(&r.terminals[&id], "t1", &last),
            "<hc>t1 reviewer exited (exit 3). Last lines:\na\nb[201~\tc\nRead: hc read t1</hc>"
        );
    }
}
