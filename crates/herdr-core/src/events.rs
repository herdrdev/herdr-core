//! Events: every fact change, in order, to anyone subscribed. A subscriber
//! that falls behind gets `missed` plus a fresh snapshot instead of a backlog.

use crate::runtime::{Core, State};
use crate::terminals::Terminal;
use crate::wire::Conn;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

/// Events queued for a plain-mode subscriber before it counts as behind.
pub const PLAIN_QUEUE: usize = 1024;

#[derive(Default)]
pub struct Events {
    pub seq: u64,
    pub(crate) subs: Vec<Sub>,
}

#[derive(Clone, Default)]
pub struct Filter {
    /// One terminal, by its address here (`t7`, `rohan/t7`).
    pub terminal: Option<String>,
    /// Children of this terminal (`t3`).
    pub children_of: Option<String>,
    pub labels: BTreeMap<String, String>,
    /// Documents this subscriber asked for, by name or by a prefix ending in `*`
    /// (`piher.cmd.t7`, `piher.*`). Empty hears every document event, as it always
    /// has, but only while no terminal is selected: a document concerns no terminal.
    pub docs: Vec<String>,
    /// A core that dialed in: only what is here. What it hears from us it
    /// passes on, so two machines that added each other would echo forever.
    pub here_only: bool,
}

impl Filter {
    /// `record`: a terminal as clients see it (here or on another machine).
    fn matches(&self, record: &Value) -> bool {
        self.terminal.as_ref().is_none_or(|a| record["address"] == a.as_str())
            && self.children_of.as_ref().is_none_or(|p| record["parent"] == p.as_str())
            && self.labels.iter().all(|(k, v)| record["labels"][k] == v.as_str())
    }

    /// No terminal selection at all: this subscriber asked for everything.
    fn all_terminals(&self) -> bool {
        self.terminal.is_none() && self.children_of.is_none() && self.labels.is_empty()
    }

    /// Whether this subscriber hears the document event for `name`: the names it
    /// asked for, or every one when it asked for none and selected no terminal.
    fn wants_doc(&self, name: &str) -> bool {
        if self.docs.is_empty() {
            return self.all_terminals();
        }
        self.docs.iter().any(|pattern| match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == pattern,
        })
    }

    /// Whether a lost document stream concerns it: a document reader (it named
    /// names) or a subscriber that hears everything.
    fn wants_documents(&self) -> bool {
        !self.docs.is_empty() || self.all_terminals()
    }

    /// Whether it wants terminal facts at all. Naming anything narrows, as it does
    /// in every other dimension of this filter, so naming documents and no terminal
    /// asks for documents alone; naming both asks for both.
    fn wants_terminals(&self) -> bool {
        self.docs.is_empty() || !self.all_terminals()
    }
}

/// What a delivered event is about, which is what a subscriber's filter is read
/// against: a terminal (with the record it had, when it changed) or a document.
pub(crate) enum Target<'a> {
    Terminal {
        record: &'a Value,
        was: Option<&'a Value>,
    },
    /// A document fact concerns a name, not a terminal: `record` is `Null`.
    Doc {
        name: &'a str,
    },
}

fn record(t: &Terminal) -> Value {
    json!(crate::terminals::Public(t.clone()))
}

pub enum Sink {
    Framed {
        conn: Arc<Conn>,
        channel: u32,
    },
    Plain {
        tx: mpsc::Sender<String>,
        behind: Arc<AtomicBool>,
    },
    /// The core's own waits: every event, unbounded (they live for one call).
    Waiter(mpsc::UnboundedSender<Value>),
}

pub(crate) struct Sub {
    filter: Filter,
    sink: Sink,
}

fn notification(method: &str, params: Value) -> String {
    format!("{}\n", json!({"jsonrpc": "2.0", "method": method, "params": params}))
}

impl Core {
    /// Terminals the filter matches: here, then those mirrored from machines.
    pub fn matching(&self, st: &State, filter: &Filter) -> Vec<Value> {
        let here = st.reg.terminals.values().map(record);
        let afar = st.mirrors.values().filter(|_| !filter.here_only).flat_map(|m| m.values().cloned());
        here.chain(afar).filter(|r| filter.matches(r)).collect()
    }

    /// Every terminal the filter matches, with the sequence it is current at.
    pub fn snapshot(&self, st: &State, filter: &Filter) -> Value {
        json!({"seq": st.events.seq, "terminals": self.matching(st, filter)})
    }

    pub fn subscribe(&self, st: &mut State, filter: Filter, sink: Sink) {
        st.events.subs.push(Sub { filter, sink });
    }

    pub fn unsubscribe(&self, st: &mut State, conn: &Arc<Conn>, channel: Option<u32>) {
        st.events.subs.retain(|s| match &s.sink {
            Sink::Framed { conn: c, channel: ch } => !(Arc::ptr_eq(c, conn) && channel.is_none_or(|x| x == *ch)),
            _ => true,
        });
    }

    /// A wait that is over, however it ended: nothing else would clear it from
    /// a terminal that is done or quiet.
    pub fn unsubscribe_waiter(&self, st: &mut State, tx: &mpsc::UnboundedSender<Value>) {
        st.events.subs.retain(|s| !matches!(&s.sink, Sink::Waiter(w) if w.same_channel(tx)));
    }

    pub fn subscribed(&self, st: &State, conn: &Arc<Conn>, channel: u32) -> bool {
        st.events
            .subs
            .iter()
            .any(|s| matches!(&s.sink, Sink::Framed { conn: c, channel: ch } if Arc::ptr_eq(c, conn) && *ch == channel))
    }

    /// Records one fact change about terminal `id` (call before removing it).
    pub fn emit(&self, st: &mut State, kind: &str, id: u32, data: Value) {
        self.emit_from(st, kind, id, data, None);
    }

    /// A change that may move a terminal into or out of a filter (labels), or
    /// a removal (the record is already gone): subscribers that matched it as
    /// it `was` hear about it too, or they'd keep it forever. One it newly
    /// matches gets the whole record with it, to add.
    pub fn emit_from(&self, st: &mut State, kind: &str, id: u32, data: Value, was: Option<&Terminal>) {
        let Some(t) = st.reg.terminals.get(&id).or(was).map(record) else { return };
        self.deliver(st, kind, &crate::terminals::address(id), data, &t, was.map(record).as_ref());
    }

    /// One fact change about the terminal at `address` to every subscriber it
    /// concerns: `record` is it now (or as it was, when removed), `was` before.
    pub fn deliver(&self, st: &mut State, kind: &str, address: &str, data: Value, record: &Value, was: Option<&Value>) {
        let params = json!({"type": kind, "terminal": address, "data": data});
        self.deliver_event(st, params, Target::Terminal { record, was });
    }

    /// Documents concern the subscribers that named them, every subscriber while
    /// nobody names any, and never a terminal selection on its own.
    pub fn doc_event(&self, st: &mut State, name: &str, data: Value, machine: Option<&str>) {
        let mut params = json!({"type": "doc", "data": data});
        if let Some(machine) = machine {
            params["machine"] = machine.into();
        }
        self.deliver_event(st, params, Target::Doc { name });
    }

    fn deliver_event(&self, st: &mut State, mut params: Value, target: Target<'_>) {
        st.events.seq += 1;
        params["seq"] = st.events.seq.into();
        let afar = params["machine"].is_string() || params["terminal"].as_str().is_some_and(|a| a.contains('/'));
        let plain = notification("event", params.clone());
        let with_record = |record: &Value| {
            let mut params = params.clone();
            params["data"]["record"] = record.clone();
            notification("event", params)
        };
        let mut subs = std::mem::take(&mut st.events.subs);
        subs.retain(|sub| {
            // `record` is `Some` only for a terminal the subscriber newly matches, which
            // is also the only case that carries the record along with the fact.
            let (now, before, added) = match target {
                Target::Terminal { record, was } => (
                    sub.filter.wants_terminals() && sub.filter.matches(record),
                    sub.filter.wants_terminals() && was.is_some_and(|w| sub.filter.matches(w)),
                    (was.is_some() && sub.filter.wants_terminals() && sub.filter.matches(record)).then_some(record),
                ),
                Target::Doc { name } => (sub.filter.wants_doc(name), false, None),
            };
            if !now && !before {
                return true;
            }
            if afar && sub.filter.here_only {
                return true;
            }
            let line = match added {
                Some(record) if !before => with_record(record),
                _ => plain.clone(),
            };
            match &sub.sink {
                Sink::Framed { conn, channel } => {
                    let fresh = || notification("missed", self.snapshot(st, &sub.filter));
                    conn.with(|out| {
                        if !out.data_lines(*channel, line.as_bytes()) {
                            out.data(*channel, fresh().as_bytes());
                        }
                    });
                    true
                }
                Sink::Plain { tx, behind } => match tx.try_send(line) {
                    Ok(()) => true,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        behind.store(true, Ordering::SeqCst);
                        true
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                },
                Sink::Waiter(tx) => tx.send(params.clone()).is_ok(),
            }
        });
        st.events.subs = subs;
        self.changed.notify_waiters();
    }

    /// A machine lost changes: document readers must refetch, with our sequence,
    /// not the remote stream's. Terminal mirrors have already been refreshed.
    pub fn documents_missed(&self, st: &mut State) {
        let mut subs = std::mem::take(&mut st.events.subs);
        subs.retain(|sub| {
            if !sub.filter.wants_documents() || sub.filter.here_only {
                return true;
            }
            let line = notification("missed", self.snapshot(st, &sub.filter));
            match &sub.sink {
                Sink::Framed { conn, channel } => {
                    conn.with(|out| {
                        if !out.data_lines(*channel, line.as_bytes()) {
                            out.data(*channel, line.as_bytes());
                        }
                    });
                    true
                }
                Sink::Plain { tx, behind } => match tx.try_send(line) {
                    Ok(()) => true,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        behind.store(true, Ordering::SeqCst);
                        true
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                },
                Sink::Waiter(_) => true,
            }
        });
        st.events.subs = subs;
    }

    /// The line a plain subscriber that fell behind gets instead of its backlog.
    pub fn missed_line(&self, filter: &Filter) -> String {
        notification("missed", self.snapshot(&self.lock(), filter))
    }
}

pub fn heartbeat_line(seq: u64) -> String {
    notification("heartbeat", json!({"seq": seq}))
}

#[cfg(test)]
mod tests {
    use super::Filter;

    fn filter(docs: &[&str], terminal: Option<&str>) -> Filter {
        Filter {
            terminal: terminal.map(String::from),
            docs: docs.iter().map(|p| (*p).to_string()).collect(),
            ..Filter::default()
        }
    }

    #[test]
    fn a_document_subscriber_hears_the_names_it_named() {
        let f = filter(&["app", "job.*"], None);
        assert!(f.wants_doc("app"));
        assert!(!f.wants_doc("app2"), "an exact name is not a prefix");
        assert!(f.wants_doc("job.7"), "a trailing star is a prefix");
        assert!(f.wants_doc("job."), "an empty tail still matches the prefix");
        assert!(!f.wants_doc("jobs"), "the prefix includes its dot");
    }

    #[test]
    fn no_names_means_every_document_unless_a_terminal_is_selected() {
        assert!(filter(&[], None).wants_doc("anything"));
        assert!(!filter(&[], Some("t7")).wants_doc("anything"));
        assert!(filter(&["app"], Some("t7")).wants_doc("app"), "a named document is heard beside a terminal");
    }

    #[test]
    fn naming_documents_alone_asks_for_documents_alone() {
        let docs_only = filter(&["app"], None);
        assert!(!docs_only.wants_terminals(), "nothing asked for terminals");
        assert!(docs_only.wants_doc("app"));
        let both = filter(&["app"], Some("t7"));
        assert!(both.wants_terminals(), "a terminal was named too");
        assert!(both.wants_doc("app"));
        let terminals_only = filter(&[], Some("t7"));
        assert!(terminals_only.wants_terminals());
        assert!(!terminals_only.wants_doc("app"), "it named no document");
        let everything = filter(&[], None);
        assert!(everything.wants_terminals() && everything.wants_doc("app"));
    }

    #[test]
    fn a_document_reader_cares_about_a_lost_stream() {
        assert!(filter(&[], None).wants_documents());
        assert!(!filter(&[], Some("t7")).wants_documents());
        assert!(filter(&["app"], Some("t7")).wants_documents());
    }
}
