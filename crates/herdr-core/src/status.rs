//! Status facts from OSC 7501, kept per terminal exactly as reported.
//! A report replaces its record; `clear` removes a record and its children.
//! The turn goes up each time the root record leaves `working`, for any reason.

use crate::terminals::Terminal;
use ghostty_vt::StatusReport;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_RECORDS: usize = 256;

/// One 7501 record. Words are the program's own; nothing is translated.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Status {
    /// idle, working, done, blocked or error.
    pub state: String,
    /// permission, question or auth (blocked only, when the program said).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// 0..100 (working or blocked, when the program said).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<u8>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub app: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub msg: String,
}

impl Status {
    fn from(r: &StatusReport) -> Status {
        Status {
            state: r.state.into(),
            kind: r.kind.map(Into::into),
            progress: r.progress,
            app: r.app.clone(),
            title: r.title.clone(),
            msg: r.message.clone(),
        }
    }

    /// The report again as OSC 7501, for a new viewer's first paint.
    pub fn encode(&self, id: &str) -> String {
        let mut fields = vec![format!("state={}", self.state)];
        let mut push = |key: &str, value: &str| {
            if !value.is_empty() {
                fields.push(format!("{key}={value}"));
            }
        };
        push("id", id);
        push("kind", self.kind.as_deref().unwrap_or(""));
        push("progress", &self.progress.map(|p| p.to_string()).unwrap_or_default());
        push("app", &self.app);
        push("title", &base64(self.title.as_bytes()));
        push("msg", &base64(self.msg.as_bytes()));
        format!("\x1b]7501;{}\x1b\\", fields.join(":"))
    }
}

/// What a report changed, for events and waiters.
#[derive(Default, Debug, PartialEq)]
pub struct Change {
    pub status: bool,
    /// A fresh root report, not a child record or an unchanged recovery replay.
    pub root_report: bool,
    pub turn_ended: bool,
    /// It just got stuck: blocked on the user or errored (wasn't that before).
    pub stuck: bool,
}

fn working(t: &Terminal) -> bool {
    t.status.as_ref().is_some_and(|s| s.state == "working")
}

fn end_turn(t: &mut Terminal, was_working: bool) -> bool {
    let ended = was_working && !working(t);
    if ended {
        t.turn += 1;
    }
    ended
}

fn stuck(t: &Terminal) -> Option<&str> {
    t.status.as_ref().map(|s| s.state.as_str()).filter(|w| *w == "blocked" || *w == "error")
}

fn word(t: &Terminal) -> Option<String> {
    t.status.as_ref().map(|s| s.state.clone())
}

fn stamp(t: &mut Terminal, was: Option<String>) {
    if word(t) != was {
        t.status_ms = Some(crate::runtime::now_ms());
    }
}

pub fn apply(t: &mut Terminal, r: &StatusReport) -> Change {
    let (was_working, was_word) = (working(t), word(t));
    let was = stuck(t).map(String::from);
    t.reported = true;
    if r.state == "clear" {
        if r.id.is_empty() {
            t.status = None;
            t.records.clear();
        } else {
            let child = format!("{}/", r.id);
            t.records.retain(|id, _| *id != r.id && !id.starts_with(&child));
        }
    } else if r.id.is_empty() {
        t.status = Some(Status::from(r));
    } else {
        // Over the limit, the record updated longest ago goes (approximated by first id).
        if !t.records.contains_key(&r.id) && t.records.len() >= MAX_RECORDS - 1 {
            let oldest = t.records.keys().next().cloned().expect("non-empty");
            t.records.remove(&oldest);
        }
        t.records.insert(r.id.clone(), Status::from(r));
    }
    stamp(t, was_word);
    Change {
        status: true,
        root_report: r.id.is_empty(),
        turn_ended: end_turn(t, was_working),
        stuck: stuck(t).is_some() && stuck(t) != was.as_deref(),
    }
}

/// The program ended: its status facts reset.
pub fn reset(t: &mut Terminal) -> Change {
    let (was_working, was_word) = (working(t), word(t));
    let had = t.status.is_some() || !t.records.is_empty();
    t.status = None;
    t.records.clear();
    stamp(t, was_word);
    Change { status: had, turn_ended: end_turn(t, was_working), ..Default::default() }
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { TABLE[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::{NewTerminal, Registry};

    fn terminal() -> Terminal {
        let mut r = Registry::default();
        let new = NewTerminal { argv: vec!["x".into()], cwd: "/".into(), ..Default::default() };
        let id = r.create(new, 0).unwrap();
        r.terminals.remove(&id).unwrap()
    }

    fn report(state: &'static str, id: &str) -> StatusReport {
        StatusReport {
            state,
            kind: None,
            progress: None,
            id: id.into(),
            app: String::new(),
            title: String::new(),
            message: String::new(),
        }
    }

    #[test]
    fn the_turn_counts_every_way_of_leaving_working() {
        let mut t = terminal();
        assert!(!apply(&mut t, &report("idle", "")).turn_ended);
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("done", "")).turn_ended); // finished
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("idle", "")).turn_ended); // interrupted
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("blocked", "")).turn_ended); // asks permission
        apply(&mut t, &report("working", ""));
        assert!(apply(&mut t, &report("clear", "")).turn_ended); // cleared
        apply(&mut t, &report("working", ""));
        assert!(!apply(&mut t, &report("working", "sub")).turn_ended, "child records don't count");
        assert!(reset(&mut t).turn_ended); // exited
        assert_eq!(t.turn, 5);
        assert!(t.status.is_none() && t.reported);
    }

    #[test]
    fn the_status_time_moves_only_when_the_word_changes() {
        let mut t = terminal();
        assert_eq!(t.status_ms, None, "never said anything");
        apply(&mut t, &report("working", ""));
        let since = t.status_ms.expect("its first word");
        t.status_ms = Some(1);
        apply(&mut t, &report("working", ""));
        apply(&mut t, &report("done", "sub"));
        assert_eq!(t.status_ms, Some(1), "the same word again, or a child record");
        apply(&mut t, &report("done", ""));
        assert!(t.status_ms >= Some(since));
        t.status_ms = Some(1);
        reset(&mut t);
        assert!(t.status_ms > Some(1), "exiting clears the word");
    }

    #[test]
    fn getting_stuck_is_news_even_outside_a_turn() {
        let mut t = terminal();
        apply(&mut t, &report("idle", ""));
        assert!(apply(&mut t, &report("error", "")).stuck, "e.g. it failed while starting");
        assert!(!apply(&mut t, &report("error", "")).stuck, "once");
        assert!(apply(&mut t, &report("blocked", "")).stuck);
        assert!(!apply(&mut t, &report("idle", "")).stuck);
    }

    #[test]
    fn clear_removes_a_record_and_its_children() {
        let mut t = terminal();
        for id in ["build", "build/test", "buildx", "deploy"] {
            apply(&mut t, &report("working", id));
        }
        apply(&mut t, &report("clear", "build"));
        assert_eq!(t.records.keys().collect::<Vec<_>>(), ["buildx", "deploy"]);
    }

    #[test]
    fn reports_encode_back_to_what_the_parser_reads() {
        let mut screen = ghostty_vt::Terminal::new(80, 24, 1 << 20);
        let sent = StatusReport {
            state: "blocked",
            kind: Some("permission"),
            progress: Some(40),
            id: "a/b".into(),
            app: "claude-code".into(),
            title: "Run tests".into(),
            message: "Apply these changes?".into(),
        };
        screen.write(Status::from(&sent).encode("a/b").as_bytes());
        assert_eq!(screen.take_status_reports(), vec![sent]);
        assert_eq!(base64(b"Apply?"), "QXBwbHk/");
        assert_eq!(base64(b"ab"), "YWI=");
    }
}
