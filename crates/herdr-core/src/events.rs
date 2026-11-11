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
        self.deliver_event(st, params, record, was);
    }

    /// Documents concern only unfiltered subscribers, never a terminal selection.
    pub fn doc_event(&self, st: &mut State, data: Value, machine: Option<&str>) {
        let mut params = json!({"type": "doc", "data": data});
        if let Some(machine) = machine {
            params["machine"] = machine.into();
        }
        self.deliver_event(st, params, &Value::Null, None);
    }

    fn deliver_event(&self, st: &mut State, mut params: Value, record: &Value, was: Option<&Value>) {
        st.events.seq += 1;
        params["seq"] = st.events.seq.into();
        let afar = params["machine"].is_string() || params["terminal"].as_str().is_some_and(|a| a.contains('/'));
        let plain = notification("event", params.clone());
        let with_record = || {
            let mut params = params.clone();
            params["data"]["record"] = record.clone();
            notification("event", params)
        };
        let mut subs = std::mem::take(&mut st.events.subs);
        subs.retain(|sub| {
            let (now, before) = (sub.filter.matches(record), was.is_some_and(|w| sub.filter.matches(w)));
            if (!now && !before) || (afar && sub.filter.here_only) {
                return true;
            }
            let line = if now && was.is_some() && !before { with_record() } else { plain.clone() };
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
            if !sub.filter.matches(&Value::Null) || sub.filter.here_only {
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
