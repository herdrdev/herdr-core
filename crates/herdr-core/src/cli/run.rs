//! `hc run …` / `hc start …`: flags after a template's task, and what a run prints.

use super::*;

/// A template's task is plain text, so `--name x` after it is hc's, not part
/// of the task. A program's arguments stay its own.
/// Flags given before the program stay; flags after the task are added.
/// Also says whether those flags asked for --json.
pub(super) fn flags_after_task(paths: &Paths, p: RunParams) -> Result<(RunParams, bool), RpcError> {
    let Some((Cmd::Run(q), json)) = trailing_flags(paths, "run", &["run"], &p.argv)? else { return Ok((p, false)) };
    let merged = RunParams {
        argv: q.argv,
        create: CreateParams {
            name: q.create.name.or(p.create.name),
            cwd: q.create.cwd.or(p.create.cwd),
            on: q.create.on.or(p.create.on),
            labels: [p.create.labels, q.create.labels].concat(),
            parent: q.create.parent.or(p.create.parent),
            no_parent: p.create.no_parent || q.create.no_parent,
            no_notify: p.create.no_notify || q.create.no_notify,
            outlive_parent: p.create.outlive_parent || q.create.outlive_parent,
            rm: p.create.rm || q.create.rm,
            keep: p.create.keep || q.create.keep,
            ..p.create
        },
        bg: p.bg || q.bg,
        timeout: q.timeout.or(p.timeout),
        ..p
    };
    if merged.create.parent.is_some() && merged.create.no_parent {
        return Err(RpcError::new("usage", "--parent and --no-parent together", "see `hc run --help`"));
    }
    Ok((merged, json))
}

pub(super) fn start_flags_after_task(paths: &Paths, p: StartParams) -> Result<(StartParams, bool), RpcError> {
    let Some((Cmd::Start(q), json)) = trailing_flags(paths, "start", &["start", &p.terminal], &p.argv)? else {
        return Ok((p, false));
    };
    let merged = StartParams { argv: q.argv, bg: p.bg || q.bg, timeout: q.timeout.or(p.timeout), ..p };
    Ok((merged, json))
}

/// The task with hc's flags moved out of it, parsed on their own (behind a
/// `--`, so a flag missing its value can't take the template's name).
fn trailing_flags(paths: &Paths, sub: &str, prefix: &[&str], argv: &[String]) -> Result<Option<(Cmd, bool)>, RpcError> {
    use clap::CommandFactory;
    if argv.len() < 2 || !matches!(crate::templates::load(paths, &argv[0]), Ok(Some(_))) {
        return Ok(None);
    }
    let cli = Cli::command();
    let Some(subcmd) = cli.find_subcommand(sub) else { return Ok(None) };
    let (mut flags, mut task) = (Vec::new(), Vec::new());
    let mut rest = argv[1..].iter();
    while let Some(a) = rest.next() {
        let long = a.strip_prefix("--").map(|l| l.split('=').next().unwrap_or(l));
        match long.and_then(|l| subcmd.get_arguments().chain(cli.get_arguments()).find(|arg| arg.get_long() == Some(l)))
        {
            Some(arg) => {
                flags.push(a.clone());
                if arg.get_action().takes_values() && !a.contains('=') {
                    flags.extend(rest.next().cloned());
                }
            }
            None => task.push(a.clone()),
        }
    }
    if flags.is_empty() {
        return Ok(None);
    }
    let args = ["hc".into()]
        .into_iter()
        .chain(prefix.iter().map(|s| (*s).to_string()))
        .chain(flags)
        .chain(["--".into(), argv[0].clone()])
        .chain(task);
    match Cli::try_parse_from(args) {
        Ok(Cli { cmd: Some(cmd), json, .. }) => Ok(Some((cmd, json))),
        Ok(_) => Ok(None),
        Err(e) => Err(RpcError::new("usage", usage_message(&e), format!("see `hc {sub} --help`"))),
    }
}

pub(super) fn run_result_out(r: &Value, wait: bool, asked: bool) -> Result<Option<Out>, RpcError> {
    if wait && !asked {
        return ran(r);
    }
    let address = r["address"].as_str().unwrap_or("?");
    let mut human = format!("{address} started");
    if let Some(first) = r.get("first_message") {
        match first["sent"].as_bool() {
            Some(true) => human.push_str("; first message sent"),
            _ => human.push_str(&format!("; first message NOT sent: {}", first["reason"].as_str().unwrap_or("?"))),
        }
    }
    let mut json = short(r);
    for key in ["first_message", "wait", "output"] {
        if let Some(v) = r.get(key) {
            json[key] = v.clone();
        }
    }
    Ok(Some(Out { json, human }))
}

/// A run that waited: its output as text (like read), a last line when there
/// is more to say, and the program's exit code as hc's (124: still running).
pub(super) fn ran(r: &Value) -> Result<Option<Out>, RpcError> {
    use std::io::Write;
    let t = r["address"].as_str().unwrap_or("?");
    // An older hc (on another machine) ignores the wait: it only started it.
    if r.get("wait").is_none() {
        let fix = t.split_once('/').map(|(m, _)| format!(": hc machine update {m}")).unwrap_or_default();
        println!("[hc] started as {t}, not waited on: that hc is older{fix}");
        std::process::exit(0);
    }
    let w = &r["wait"];
    let status = w["status"].as_str().unwrap_or("no status");
    let (note, code) = match w["reason"].as_str() {
        Some("timeout") => (Some(format!("still running as {t}: hc wait {t}, hc read {t}")), 124),
        Some("exit") => match (w["exit_code"].as_i64(), w["signal"].as_i64()) {
            (Some(0), _) => (None, 0),
            (Some(c), _) => (Some(format!("{t} exited (exit {c})")), c as i32),
            (_, Some(sig)) => (Some(format!("{t} ended by signal {sig}")), 128 + sig as i32),
            _ => (Some(format!("{t} ended")), 1),
        },
        _ if status == "blocked" => (Some(format!("{t} is blocked: read it, then answer with hc send or hc keys")), 0),
        _ => (Some(format!("{t} is {status}: hc send {t} <text>")), 0),
    };
    let mut text = r["output"].as_str().unwrap_or_default().trim_end().to_string();
    if let Some(first) = r.get("first_message").filter(|f| f["sent"] != true) {
        text.push_str(&format!("\n[hc] first message NOT sent: {}", first["reason"].as_str().unwrap_or("?")));
    }
    if let Some(note) = note {
        text.push_str(&format!("\n[hc] {note}"));
    }
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{}", text.trim_start_matches('\n')).and_then(|_| out.flush());
    std::process::exit(code);
}
