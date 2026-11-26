//! `hc run …`: flags after a template's task, and what a run that waited prints.

use super::*;

/// A template's task is plain text, so `--name x` after it is hc's, not part
/// of the task. A program's arguments stay its own.
/// Flags given before the program stay; flags after the task are added.
/// Also says whether those flags asked for --json.
pub(super) fn flags_after_task(paths: &Paths, mut p: Box<RunParams>) -> Result<(Box<RunParams>, bool), RpcError> {
    let mut json = false;
    if p.argv.len() >= 2
        && matches!(crate::templates::load(paths, &p.argv[0]), Ok(Some(_)))
        && let Some((q, asked)) = trailing_flags(&p.argv, |flag| !is_override(flag))?
    {
        json = asked;
        *p = RunParams {
            argv: q.argv,
            name: q.name.or(p.name),
            cwd: q.cwd.or(p.cwd),
            on: q.on.or(p.on),
            labels: [p.labels, q.labels].concat(),
            parent: q.parent.or(p.parent),
            no_parent: p.no_parent || q.no_parent,
            no_notify: p.no_notify || q.no_notify,
            outlive_parent: p.outlive_parent || q.outlive_parent,
            rm: p.rm || q.rm,
            keep: p.keep || q.keep,
            bg: p.bg || q.bg,
            timeout: q.timeout.or(p.timeout),
            ..*p
        };
        if p.parent.is_some() && p.no_parent {
            return Err(RpcError::new("usage", "--parent and --no-parent together", "see `hc run --help`"));
        }
    }
    p.parse_overrides = p.argv.iter().skip(1).filter_map(|arg| long_flag(arg)).any(is_override);
    Ok((p, json))
}

/// The destination found a template and can distinguish its overrides from program arguments.
pub(crate) fn apply_trailing_overrides(p: &mut RunParams) -> Result<(), RpcError> {
    if let Some((q, _)) = trailing_flags(&p.argv, is_override)? {
        p.argv = q.argv;
        p.model = q.model.or(p.model.take());
        p.command = q.command.or(p.command.take());
    }
    Ok(())
}

fn long_flag(arg: &str) -> Option<&str> {
    arg.strip_prefix("--").map(|flag| flag.split_once('=').map_or(flag, |(name, _)| name))
}

fn is_override(flag: &str) -> bool {
    matches!(flag, "model" | "command")
}

/// The task with hc's flags moved out of it, parsed on their own (behind a
/// `--`, so a flag missing its value can't take the template's name).
fn trailing_flags(argv: &[String], select: impl Fn(&str) -> bool) -> Result<Option<(RunParams, bool)>, RpcError> {
    use clap::CommandFactory;
    let cli = Cli::command();
    let Some(run) = cli.find_subcommand("run") else { return Ok(None) };
    let (mut flags, mut task) = (Vec::new(), Vec::new());
    let mut rest = argv[1..].iter();
    while let Some(a) = rest.next() {
        match long_flag(a)
            .and_then(|l| run.get_arguments().chain(cli.get_arguments()).find(|arg| arg.get_long() == Some(l)))
        {
            Some(arg) => {
                let out = if arg.get_long().is_some_and(&select) { &mut flags } else { &mut task };
                out.push(a.clone());
                if arg.get_action().takes_values() && !a.contains('=') {
                    out.extend(rest.next().cloned());
                }
            }
            None => task.push(a.clone()),
        }
    }
    if flags.is_empty() {
        return Ok(None);
    }
    let args =
        ["hc".to_string(), "run".into()].into_iter().chain(flags).chain(["--".into(), argv[0].clone()]).chain(task);
    match Cli::try_parse_from(args) {
        Ok(Cli { cmd: Some(Cmd::Run(q)), json, .. }) => Ok(Some((*q, json))),
        Ok(_) => Ok(None),
        Err(e) => Err(RpcError::new("usage", usage_message(&e), "see `hc run --help`")),
    }
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
