//! How results look to a person: rows, tables, one-line summaries.

use super::*;

/// The program's own status word, else live or how it ended, and its dot.
pub(super) fn status(t: &Value) -> (String, &'static str) {
    const HOLLOW: &str = "\x1b[2m\u{25cb}\x1b[22m";
    if t["state"] != "live" {
        let word = match (t["exit_code"].as_i64(), t["signal"].as_i64()) {
            (Some(code), _) => format!("exit {code}"),
            (_, Some(sig)) => format!("signal {sig}"),
            _ => "ended".into(),
        };
        return (word, HOLLOW);
    }
    let Some(word) = t["status"]["state"].as_str() else { return ("running".into(), HOLLOW) };
    let dot = match word {
        "working" => "\x1b[33m\u{25cf}\x1b[39m",
        "blocked" | "error" => "\x1b[31m\u{25cf}\x1b[39m",
        "done" => "\x1b[32m\u{25cf}\x1b[39m",
        _ => "\u{25cf}",
    };
    (word.into(), dot)
}

/// Who runs in a terminal: the app it reports, else its program's name.
pub(super) fn agent(t: &Value) -> String {
    let program = t["argv"][0].as_str().unwrap_or_default();
    t["status"]["app"].as_str().unwrap_or_else(|| program.rsplit('/').next().unwrap_or_default()).into()
}

/// The last folder of a terminal's cwd, `~` for home.
pub(super) fn folder(t: &Value) -> String {
    let cwd = t["cwd"].as_str().unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && cwd.trim_end_matches('/') == home.trim_end_matches('/') {
        return "~".into();
    }
    cwd.trim_end_matches('/').rsplit('/').next().filter(|f| !f.is_empty()).unwrap_or(cwd).into()
}

/// The picker's orders, Tab cycles them: last status change first (a program
/// that never reported counts from its start), what needs you first, by name.
pub(super) fn orders(terminals: &[Value]) -> Vec<(&'static str, Vec<Value>)> {
    fn recent(t: &Value) -> std::cmp::Reverse<u64> {
        std::cmp::Reverse(t["status_ms"].as_u64().or(t["created_ms"].as_u64()).unwrap_or(0))
    }
    fn need(t: &Value) -> u8 {
        match status(t).0.as_str() {
            "blocked" | "error" => 0,
            "working" => 1,
            // Done stays done after you looked (hc can't tell): no more urgent than idle.
            "done" | "idle" => 2,
            _ => 3,
        }
    }
    fn label(t: &Value) -> String {
        let name = t["name"].as_str().filter(|n| !n.is_empty()).map(String::from);
        name.or_else(|| place(t)).unwrap_or_else(|| folder(t)).to_lowercase()
    }
    let by = |order: fn(&Value, &Value) -> std::cmp::Ordering| {
        let mut sorted = terminals.to_vec();
        sorted.sort_by(order);
        sorted
    };
    vec![
        ("recent", by(|a, b| recent(a).cmp(&recent(b)))),
        ("status", by(|a, b| need(a).cmp(&need(b)).then(recent(a).cmp(&recent(b))))),
        ("name", by(|a, b| label(a).cmp(&label(b)))),
    ]
}

/// The `where` label: where a viewer put it, in words ("herdr-core / review").
pub(super) fn place(t: &Value) -> Option<String> {
    t["labels"]["where"].as_str().filter(|w| !w.is_empty()).map(Into::into)
}

/// One row per terminal, children under their parent: a status dot, the
/// address, then name, agent, template and where (else folder) lined up, and
/// the status. With `header`, a dim title row goes first (for `hc ls`; the
/// picker's rows speak for themselves).
pub(super) fn items(terminals: &[Value], header: bool) -> Vec<picker::Item> {
    const TITLES: [&str; 4] = ["NAME", "AGENT", "TEMPLATE", "WHERE"];
    let cells: Vec<[String; 4]> = terminals
        .iter()
        .map(|t| {
            let place = place(t).unwrap_or_else(|| folder(t));
            [
                t["name"].as_str().unwrap_or_default().into(),
                agent(t),
                t["template"].as_str().unwrap_or_default().into(),
                place,
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..4)
        .map(|c| match cells.iter().map(|row| row[c].chars().count()).max().unwrap_or(0) {
            0 => 0,
            w if header => w.max(TITLES[c].len()),
            w => w,
        })
        .collect();
    // Template and folder are context: dimmed.
    let line = |row: &[String], dim: [bool; 4], last: &str| {
        let mut rest = String::new();
        for (c, cell) in row.iter().enumerate().filter(|(c, _)| widths[*c] > 0) {
            let pad = " ".repeat(widths[c] - cell.chars().count());
            let (on, off) = if dim[c] { ("\x1b[2m", "\x1b[22m") } else { ("", "") };
            rest.push_str(&format!("{on}{cell}{off}{pad}  "));
        }
        rest + &format!("\x1b[2m{last}\x1b[22m")
    };
    // Only built with a header: the widths fit the titles only then.
    let title = (header && !terminals.is_empty()).then(|| picker::Item {
        value: String::new(),
        mark: " ".into(),
        head: String::new(),
        rest: line(&TITLES.map(String::from), [true; 4], "STATUS"),
        short: String::new(),
        tail: "\x1b[2mSTATUS\x1b[22m".into(),
        parent: None,
    });
    let rows = terminals.iter().zip(cells).map(|(t, row)| {
        let (word, dot) = status(t);
        let place = place(t).unwrap_or_else(|| format!("{} \u{b7} {}", row[1], row[3]));
        let short =
            [row[0].as_str(), &place].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{b7} ");
        picker::Item {
            value: t["address"].as_str().unwrap_or_default().into(),
            mark: dot.into(),
            head: t["address"].as_str().unwrap_or("?").into(),
            rest: line(&row, [false, false, true, false], &word),
            short,
            tail: format!("\x1b[2m{word}\x1b[22m"),
            parent: t["parent"].as_str().map(Into::into),
        }
    });
    title.into_iter().chain(rows).collect()
}

/// The facts an agent acts on; `hc get` has the full record.
pub(super) fn short(t: &Value) -> Value {
    let keep = ["address", "name", "template", "parent", "state", "turn", "exit_code", "signal", "reason", "cwd"];
    let mut out: serde_json::Map<String, Value> =
        keep.iter().filter_map(|k| t.get(*k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect();
    if let Some(word) = t["status"]["state"].as_str() {
        out.insert("status".into(), word.into());
    }
    // What and where, as a person's ls shows them: agents find each other this way.
    out.insert("agent".into(), agent(t).into());
    if let Some(place) = place(t) {
        out.insert("where".into(), place.into());
    }
    Value::Object(out)
}

pub(super) fn describe_wait(r: &Value) -> String {
    let t = r["terminal"].as_str().unwrap_or("?");
    let mut lines: Vec<String> = r["children"].as_array().into_iter().flatten().map(describe_wait).collect();
    if r["reason"] == "message" || !r["from"].is_null() {
        lines.insert(0, format!("a message from {} (already in your input)", r["from"].as_str().unwrap_or("a person")));
        // A wait on one terminal, cut short: say where that one stands.
        if r["ended"] == false && lines.len() == 1 {
            let word = r["status"].as_str().unwrap_or("no status");
            lines.push(format!("{t} still running: {word}, turn {} (wait again to keep waiting)", r["turn"]));
        }
    }
    if r["reason"] == "children" && lines.is_empty() {
        return "no children are working".into();
    }
    if !lines.is_empty() {
        return lines.join("\n");
    }
    let word = r["status"].as_str().unwrap_or("no status");
    match r["reason"].as_str() {
        Some("exit") => format!("{t} exited (exit {}, turn {})", r["exit_code"], r["turn"]),
        Some("timeout") => format!("{t} still running: {word}, turn {} (wait again to keep waiting)", r["turn"]),
        Some("output") => format!("{t} printed it ({word}, turn {})", r["turn"]),
        Some("not_started") => format!("{t} hasn't started on it ({word}): hc read {t}, then hc wait {t}"),
        Some("running") => format!("{t} still running: {word}, turn {}", r["turn"]),
        _ => format!("{t} {word}, turn {}. Read: hc read {t}", r["turn"]),
    }
}

pub(super) fn summary(t: &Value) -> String {
    match t["state"].as_str() {
        Some("live") => match t["status"]["state"].as_str() {
            Some(word) => format!("live, {word} (turn {})", t["turn"]),
            None => "live".into(),
        },
        _ => {
            let reason = t["reason"].as_str().unwrap_or("finished");
            match (t["exit_code"].as_i64(), t["signal"].as_i64()) {
                (Some(code), _) => format!("finished ({reason}, exit {code})"),
                (_, Some(sig)) => format!("finished ({reason}, signal {sig})"),
                _ => format!("finished ({reason})"),
            }
        }
    }
}

pub(super) fn command(t: &Value) -> String {
    let argv: Vec<&str> = t["argv"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    argv.join(" ")
}

pub(super) fn table(r: &Value) -> String {
    let mut out = String::new();
    let cols = if std::io::stdout().is_terminal() { crate::attach::window_size().0 as usize } else { usize::MAX };
    for line in picker::table(&items(r["terminals"].as_array().map(Vec::as_slice).unwrap_or_default(), true), cols) {
        out.push_str(&line);
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("no live terminals\n");
    }
    for m in r["machines"].as_array().into_iter().flatten().filter(|m| m["state"] != "connected") {
        out.push_str(&format!("{}: {}\n", m["name"].as_str().unwrap_or("?"), line_state(m)));
    }
    match r["hidden_finished"].as_u64() {
        Some(n) if n > 0 => out.push_str(&format!("{n} finished hidden (--all)")),
        _ => {
            out.pop();
        }
    }
    out
}

/// How a machine's line is: "connected", "unreachable since 14:02 (why)", ...
pub(super) fn line_state(m: &Value) -> String {
    let since = m["since_ms"].as_u64().map(|ms| {
        let ago = crate::runtime::now_ms().saturating_sub(ms) / 1000;
        match ago {
            0..60 => format!(" for {ago}s"),
            60..3600 => format!(" for {}m", ago / 60),
            _ => format!(" for {}h", ago / 3600),
        }
    });
    let why = m["why"].as_str().map(|w| format!(" ({w})")).unwrap_or_default();
    match m["state"].as_str() {
        Some("needs_login") => {
            let keys = if cfg!(windows) { "set up its SSH keys, then " } else { "" };
            format!(
                "needs login{}: {keys}hc machine login {}",
                since.unwrap_or_default(),
                m["name"].as_str().unwrap_or("?")
            )
        }
        Some("needs_update") => format!("needs an hc update: hc machine update {}", m["name"].as_str().unwrap_or("?")),
        Some(state) => format!("{state}{}{why}", since.unwrap_or_default()),
        None => "?".into(),
    }
}

pub(super) fn describe(t: &Value) -> String {
    let labels: Vec<String> = t["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
        .collect();
    format!(
        "{} {}\n  name: {}\n  parent: {}\n  command: {}\n  cwd: {}\n  pid: {}\n  labels: {}",
        t["address"].as_str().unwrap_or("?"),
        summary(t),
        t["name"].as_str().unwrap_or("-"),
        t["parent"].as_str().unwrap_or("-"),
        command(t),
        t["cwd"].as_str().unwrap_or("-"),
        t["pid"],
        if labels.is_empty() { "-".into() } else { labels.join(" ") },
    )
}

/// An exec's output as the program wrote it, then its exit code as ours.
pub(super) fn exec_out(r: &Value) -> ! {
    use std::io::Write;
    let bytes = |key: &str| crate::put::decode(r[key].as_str().unwrap_or_default()).unwrap_or_default();
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let _ = out.write_all(&bytes("stdout")).and_then(|_| out.flush());
    let _ = err.write_all(&bytes("stderr"));
    if r["truncated"] == true {
        let _ = writeln!(err, "[hc] output cut: kept the first 8 MB of stdout, 1 MB of stderr");
    }
    let code = match (r["timed_out"].as_bool(), r["code"].as_i64(), r["signal"].as_i64()) {
        (Some(true), _, _) => {
            let _ = writeln!(err, "[hc] timed out after {} ms: ended", r["ms"]);
            124
        }
        (_, Some(code), _) => code as i32,
        (_, _, Some(signal)) => 128 + signal as i32,
        _ => 1,
    };
    let _ = err.flush();
    std::process::exit(code)
}
