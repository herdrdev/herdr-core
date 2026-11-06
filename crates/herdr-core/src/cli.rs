//! `hc`: one API call per command. Tables for people, JSON when piped or `--json`.

use crate::api::{
    AttachParams, ExecParams, InterruptParams, KeysParams, LabelParams, ListParams, PutParams, ReadParams, RunParams,
    SendParams, SubscribeParams, TerminalParams, WaitParams,
};
use crate::client;
use crate::paths::Paths;
use crate::picker;
use crate::rpc::RpcError;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::io::{IsTerminal, Read};

mod file;
mod machine;
mod output;
mod update;
use file::*;
use machine::*;
use output::*;
use update::*;

#[derive(Parser)]
#[command(name = "hc", version, about = "herdr-core: a terminal runtime for coding agents")]
struct Cli {
    /// JSON output even in a terminal.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a program, or a template by name with its task: hc run reviewer "review src/x.rs".
    Run(RunParams),
    /// List live terminals.
    Ls(ListParams),
    /// Show one terminal.
    Get(TerminalParams),
    /// End a terminal's program (hangup, then escalate).
    Kill(TerminalParams),
    /// Remove a finished terminal.
    Rm(TerminalParams),
    /// Set labels (key=value; key= removes).
    Label(LabelParams),
    /// Keep a live terminal after loss (--off disables).
    Keep(crate::api::KeepParams),
    /// Paste text into a terminal and press Enter (waits for a person to pause).
    Send(SendParams),
    /// Wait for your children (--children any) or one terminal; a message to you also ends it.
    Wait(WaitParams),
    /// Press named keys (enter, esc, up, ctrl+c, alt+x, f5, a...).
    Keys(KeysParams),
    /// Press the stop key and report what happened.
    Interrupt(InterruptParams),
    /// Put this terminal on a core terminal (detach: ctrl+\).
    Attach(AttachParams),
    /// Print a terminal's screen and history.
    Read(ReadParams),
    /// Send a local file beside a terminal (on its machine); prints where it landed.
    Put(PutParams),
    /// Run one program without a terminal, here or on a machine: exactly what it wrote, and its exit code.
    Exec(ExecParams),
    /// Read, write or edit a file here or on a machine: an agent's hands there.
    File {
        #[command(subcommand)]
        action: FileCmd,
    },
    /// Start, stop or inspect the core.
    Server {
        #[command(subcommand)]
        action: ServerCmd,
    },
    /// Other machines, reached over ssh.
    Machine {
        #[command(subcommand)]
        action: MachineCmd,
    },
    /// Follow fact changes as they happen (one JSON line each when piped).
    Events(SubscribeParams),
    /// Templates you can run by name (~/.config/herdr-core/templates).
    Templates,
    /// Swap this hc for another build and restart (terminals keep running).
    Update {
        /// A herdr-core binary to use (default: download the latest release).
        #[arg(long, value_name = "FILE", conflicts_with = "version")]
        from: Option<std::path::PathBuf>,
        /// At least this release (`0.1.4`): nothing changes when this hc is
        /// already that or newer.
        #[arg(long, value_name = "X.Y.Z", value_parser = release_version)]
        version: Option<String>,
        /// Allow an older version (saved state only moves forward), or a
        /// release of another protocol generation.
        #[arg(long)]
        force: bool,
    },
    /// Every API method with its JSON Schema, for app builders.
    Schema,
    /// Speak the protocol over stdin/stdout (`ssh box hc connect`).
    Connect {
        /// The hc session to reach, like HC_SESSION (one command that reads
        /// the same in sh, cmd and PowerShell).
        #[arg(long)]
        session: Option<String>,
    },
    #[command(hide = true)]
    Serve,
    #[command(hide = true)]
    Hold {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
enum ServerCmd {
    Start,
    Stop,
    Status,
    /// Restart the core (e.g. after an update); programs keep running.
    Restart,
}

const HELP: &str = include_str!("help.txt");
const SKILL: &str = include_str!("SKILL.md");

/// Every command a person or agent can type (for checking the help names them all).
pub fn command_names() -> Vec<String> {
    use clap::CommandFactory;
    Cli::command()
        .get_subcommands()
        .filter(|c| !c.is_hide_set() && c.get_name() != "help")
        .map(|c| c.get_name().to_string())
        .collect()
}

pub fn main() -> i32 {
    let daemon = crate::platform::daemon_startup();
    client::remember_exe();
    let args: Vec<_> = if daemon { vec!["serve".into()] } else { std::env::args_os().skip(1).collect() };
    match args.iter().map(|a| a.to_str()).collect::<Vec<_>>().as_slice() {
        [Some("--help" | "-h" | "help")] => {
            print!("{HELP}");
            return 0;
        }
        [Some("--skill")] => {
            print!("{SKILL}");
            return 0;
        }
        [Some("--licenses")] => {
            print!("{}", include_str!("../THIRD-PARTY-NOTICES"));
            return 0;
        }
        _ => {}
    }
    // Before parsing we can only guess (for how to print a parse error).
    let piped = !std::io::stdout().is_terminal();
    let cli = match Cli::try_parse_from(std::env::args_os().take(1).chain(args.iter().cloned())) {
        Ok(cli) => cli,
        Err(e) if !e.use_stderr() || !(piped || args.iter().any(|a| a == "--json")) => e.exit(),
        Err(e) => return fail(&RpcError::new("usage", usage_message(&e), "see `hc --help`"), true),
    };
    // A template's trailing flags may include --json: parse them before choosing how to print.
    let (cmd, asked) = match cli.cmd {
        Some(Cmd::Run(p)) => match Paths::from_env().and_then(|paths| flags_after_task(&paths, p)) {
            Ok((p, trailing)) => (Some(Cmd::Run(p)), cli.json || trailing),
            Err(err) => return fail(&err, cli.json || piped),
        },
        cmd => (cmd, cli.json),
    };
    let json = asked || piped;
    match run(cmd, json, asked) {
        Ok(Some(out)) => {
            print(&out, json);
            0
        }
        Ok(None) => 0,
        Err(err) => fail(&err, json),
    }
}

/// Clap's diagnostic without its usage footer, on one line: what is wrong and
/// what is missing, not just that something is.
fn usage_message(e: &clap::Error) -> String {
    let text = e.to_string();
    let lines = text.lines().take_while(|l| !l.starts_with("Usage:") && !l.starts_with("For more information"));
    let words: Vec<&str> = lines.map(str::trim).filter(|l| !l.is_empty()).collect();
    words.join(" ").trim_start_matches("error: ").to_string()
}

/// What a command printed: its JSON result and the line a person reads.
struct Out {
    json: Value,
    human: String,
}

fn print(out: &Out, json: bool) {
    if json {
        println!("{}", out.json);
    } else {
        println!("{}", out.human);
    }
}

fn fail(err: &RpcError, json: bool) -> i32 {
    if err.kind == "cancelled" {
        // Esc in the picker: nothing to report.
    } else if json {
        let body = json!({"error": {"kind": err.kind, "message": err.message, "hint": err.hint}});
        eprintln!("{body}");
    } else {
        eprintln!("error: {}", err.message);
        if !err.hint.is_empty() {
            eprintln!("hint: {}", err.hint);
        }
    }
    err.exit_code()
}

fn interactive(json: bool) -> bool {
    !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn call(paths: &Paths, method: &str, params: Value) -> Result<Value, RpcError> {
    let stream = client::connect_or_start(paths)?;
    client::call(&stream, method, params).map_err(|e| client::explain_closed(paths, e))
}

/// The given target, else a picker in a terminal, else an error naming the
/// candidates: live terminals, or with `finished` (for rm) finished ones.
fn target(paths: &Paths, given: Option<String>, json: bool, finished: bool) -> Result<String, RpcError> {
    if let Some(t) = given {
        return Ok(t);
    }
    let list = call(paths, "terminal.list", json!({"all": finished}))?;
    let state = if finished { "finished" } else { "live" };
    let live: Vec<&Value> =
        list["terminals"].as_array().into_iter().flatten().filter(|t| t["state"] == state).collect();
    if interactive(json) {
        let live: Vec<Value> = live.into_iter().cloned().collect();
        let orders: Vec<_> = orders(&live).into_iter().map(|(name, ts)| (name, items(&ts, false))).collect();
        return picker::pick("which terminal?", &orders).ok_or_else(cancelled);
    }
    let names: Vec<String> = live
        .iter()
        .map(|t| match t["name"].as_str() {
            Some(name) => format!("{} ({name})", t["address"].as_str().unwrap_or("?")),
            None => t["address"].as_str().unwrap_or("?").to_string(),
        })
        .collect();
    let message = if names.is_empty() {
        format!("no terminal given, and none are {state}")
    } else {
        format!("which terminal? {}", names.join(", "))
    };
    Err(RpcError::new("needs_target", message, "pass an address like t7 or a name"))
}

fn cancelled() -> RpcError {
    RpcError::new("cancelled", "", "")
}

/// Bare `hc`: a picker with "new" on top in a terminal, an overview when piped.
fn bare(paths: &Paths, json: bool) -> Result<Option<Out>, RpcError> {
    let list = call(paths, "terminal.list", json!({}))?;
    if !interactive(json) {
        let mut r = list;
        r["commands"] = json!(["hc run <cmd>", "hc ls", "hc read <t>", "hc attach <t>", "hc --skill"]);
        let human = table(&r);
        return Ok(Some(Out { json: r, human }));
    }
    let mut choices = vec![picker::Item {
        value: String::new(),
        mark: "\x1b[36m+\x1b[39m".into(),
        head: "new".into(),
        rest: "\x1b[2ma shell here\x1b[22m".into(),
        short: "a shell here".into(),
        tail: String::new(),
        parent: None,
    }];
    // A shell on each connected machine too (`+name`: no address starts with +),
    // but not on a temporary one: a sandbox is an agent's, not a place to go.
    let connected = list["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["state"] == "connected" && m["temporary"] != true);
    choices.extend(connected.filter_map(|m| m["name"].as_str()).map(|name| picker::Item {
        value: format!("+{name}"),
        mark: "\x1b[36m+\x1b[39m".into(),
        head: format!("new on {name}"),
        rest: "\x1b[2ma shell there, in its home\x1b[22m".into(),
        short: format!("a shell on {name}"),
        tail: String::new(),
        parent: None,
    }));
    let terminals = list["terminals"].as_array().map(Vec::as_slice).unwrap_or_default();
    let orders: Vec<_> = orders(terminals)
        .into_iter()
        .map(|(name, ts)| (name, choices.iter().cloned().chain(items(&ts, false)).collect()))
        .collect();
    let chosen = picker::pick("herdr-core", &orders).ok_or_else(cancelled)?;
    let address = if chosen.is_empty() {
        // Our shell if we have one, else the machine's own (Windows has no $SHELL).
        let argv: Vec<String> = std::env::var("SHELL").into_iter().filter(|s| !s.is_empty()).collect();
        let r = call(paths, "terminal.run", json!({"argv": argv, "cwd": here(), "env": identity_env()}))?;
        r["address"].as_str().unwrap_or_default().to_string()
    } else if let Some(machine) = chosen.strip_prefix('+') {
        // No program: its own shell, whatever that is there.
        let r = call(paths, "terminal.run", json!({"on": machine, "env": identity_env()}))?;
        r["address"].as_str().unwrap_or_default().to_string()
    } else {
        chosen
    };
    eprintln!("{}", crate::attach::run(paths, &address, false, 0)?);
    Ok(None)
}

/// Only our terminal's identity travels; programs get the core's environment.
fn identity_env() -> std::collections::BTreeMap<String, String> {
    std::env::vars().filter(|(k, _)| crate::api::IDENTITY_VARS.contains(&k.as_str())).collect()
}

/// `json`: print JSON (asked, or piped). `asked`: `--json` itself, not a
/// program's argument that happens to read `--json`.
fn run(cmd: Option<Cmd>, json: bool, asked: bool) -> Result<Option<Out>, RpcError> {
    let paths = Paths::from_env()?;
    if !matches!(&cmd, Some(Cmd::Serve | Cmd::Hold { .. } | Cmd::Connect { .. })) {
        crate::platform::legacy_bundle_notice(&paths.state);
    }
    let Some(cmd) = cmd else {
        return bare(&paths, json);
    };
    let call = |method: &str, params: Value| call(&paths, method, params);
    let target = |given: Option<String>| target(&paths, given, json, false);
    // Commands that can open the picker take an empty target to mean "pick".
    let pick = |given: String| target((!given.is_empty()).then_some(given));
    let pick_finished = |given: String| crate::cli::target(&paths, (!given.is_empty()).then_some(given), json, true);
    Ok(Some(match cmd {
        Cmd::Serve => {
            crate::platform::prepare_core();
            crate::server::serve().map_err(|e| RpcError::internal(e.to_string()))?;
            return Ok(None);
        }
        Cmd::Hold { socket } => {
            crate::holder::run(socket).map_err(|e| RpcError::internal(e.to_string()))?;
            return Ok(None);
        }
        Cmd::Attach(p) => {
            let message = crate::attach::run(&paths, &pick(p.terminal)?, p.observe, p.history)?;
            eprintln!("{message}");
            return Ok(None);
        }
        Cmd::Wait(mut p) => {
            if p.children.is_none() {
                p.terminal = Some(target(p.terminal)?);
            }
            p.caller = Some(caller());
            let r = call("terminal.wait", to(&p))?;
            let human = describe_wait(&r);
            Out { json: r, human }
        }
        Cmd::Send(mut p) => {
            p.caller = Some(caller());
            let r = call("terminal.send", to(&p))?;
            let started = match r["started"].as_bool() {
                Some(true) => "it started working",
                Some(false) => "it did not start working",
                None => "it doesn't report status",
            };
            let mut human =
                format!("sent at turn {} ({}); {started}", r["turn"], r["status"].as_str().unwrap_or("no status"));
            if !r["wait"].is_null() {
                human = format!("{human}\n{}", describe_wait(&r["wait"]));
            }
            Out { json: r, human }
        }
        Cmd::Keys(mut p) => {
            p.caller = Some(caller());
            let r = call("terminal.keys", to(&p))?;
            Out { json: r, human: "keys sent".into() }
        }
        Cmd::Interrupt(mut p) => {
            p.terminal = pick(p.terminal)?;
            let r = call("terminal.interrupt", to(&p))?;
            let human = format!("sent {}: {}", r["key"].as_str().unwrap_or("?"), r["outcome"].as_str().unwrap_or("?"));
            Out { json: r, human }
        }
        Cmd::Run(mut p) => {
            if p.on.is_none() {
                p.cwd = from_here(p.cwd); // our folder means nothing on another machine
            }
            p.env = identity_env();
            p.caller = Some(caller());
            if let Some(line) = shell_line(&p.argv) {
                (p.line, p.argv) = (Some(line), Vec::new());
            }
            p.wait = !p.bg;
            p.die_with_parent = !p.outlive_parent; // what an older core reads
            let r = call("terminal.run", to(&p))?;
            if p.wait && !asked {
                return ran(&r);
            }
            let address = r["address"].as_str().unwrap_or("?");
            let mut human = format!("{address} started");
            if let Some(first) = r.get("first_message") {
                match first["sent"].as_bool() {
                    Some(true) => human.push_str("; first message sent"),
                    _ => human
                        .push_str(&format!("; first message NOT sent: {}", first["reason"].as_str().unwrap_or("?"))),
                }
            }
            let mut json = short(&r);
            for key in ["first_message", "wait", "output"] {
                if let Some(v) = r.get(key) {
                    json[key] = v.clone();
                }
            }
            Out { json, human }
        }
        Cmd::Ls(p) => {
            let r = call("terminal.list", to(&p))?;
            let human = table(&r);
            let rows: Vec<Value> = r["terminals"].as_array().into_iter().flatten().map(short).collect();
            let mut json = json!({"terminals": rows, "hidden_finished": r["hidden_finished"]});
            if let Some(machines) = r["machines"].as_array() {
                let keep = |m: &Value| -> Value {
                    let fields = ["name", "state", "since_ms", "why"].into_iter();
                    fields
                        .filter_map(|k| m.get(k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone())))
                        .collect::<serde_json::Map<_, _>>()
                        .into()
                };
                json["machines"] = machines.iter().map(keep).collect();
            }
            Out { json, human }
        }
        Cmd::Get(mut p) => {
            (p.terminal, p.caller) = (pick(p.terminal)?, Some(caller()));
            let r = call("terminal.get", to(&p))?;
            let human = describe(&r);
            Out { json: r, human }
        }
        Cmd::Kill(mut p) => {
            (p.terminal, p.caller) = (pick(p.terminal)?, Some(caller()));
            let r = call("terminal.kill", to(&p))?;
            let human = describe(&r);
            Out { json: r, human }
        }
        Cmd::Rm(mut p) => {
            p.terminal = pick_finished(p.terminal)?;
            let r = call("terminal.rm", to(&p))?;
            let human = format!("removed {}", r["removed"].as_str().unwrap_or("?"));
            Out { json: r, human }
        }
        Cmd::Keep(p) => {
            let r = call("terminal.keep", to(&p))?;
            let human = describe(&r);
            Out { json: r, human }
        }
        Cmd::Label(p) => {
            let r = call("terminal.label", to(&p))?;
            let human = describe(&r);
            Out { json: r, human }
        }
        Cmd::Read(mut p) => {
            p.terminal = pick(p.terminal)?;
            p.caller = Some(caller());
            let r = call("terminal.read", to(&p))?;
            let mut human = r["text"].as_str().unwrap_or_default().to_string();
            if r["why"] == "working" {
                human.push_str("\n[hc] only its screen: it is working; read again once it stops for its history");
            } else if let Some(state @ ("blocked" | "error")) = r["why"].as_str() {
                human
                    .push_str(&format!("\n[hc] only its screen: it is {state}; history is not scrolled in this state"));
            } else if r["complete"] == false {
                human.push_str("\n[hc] incomplete: older output may be missing");
            }
            // Its output is text: plain even when piped, JSON only when asked.
            if !asked {
                println!("{human}");
                return Ok(None);
            }
            Out { json: r, human }
        }
        Cmd::Put(mut p) => {
            let file = std::fs::File::open(&p.name)
                .map_err(|e| RpcError::new("usage", format!("cannot read {}: {e}", p.name), ""))?;
            // Staged on the terminal's machine: `machine/t7` names it.
            let on = p.terminal.split_once('/').map(|(machine, _)| machine.to_string());
            match payload(file, on.as_deref(), &call)? {
                Payload::Inline(bytes) => p.data = crate::put::encode(&bytes),
                Payload::Upload(id) => p.upload = Some(id),
            }
            let r = call("terminal.put", to(&p))?;
            let human = r["path"].as_str().unwrap_or_default().to_string();
            Out { json: r, human }
        }
        Cmd::Exec(mut p) => {
            if p.on.is_none() {
                p.cwd = from_here(p.cwd);
            }
            if let Some(line) = shell_line(&p.argv) {
                (p.line, p.argv) = (Some(line), Vec::new());
            }
            if p.stdin {
                p.input = Some(crate::put::encode(&piped(crate::exec::MAX_STDIN)?));
            }
            let r = call("machine.exec", to(&p))?;
            if !asked {
                exec_out(&r);
            }
            Out { json: r, human: String::new() }
        }
        Cmd::Connect { session } => {
            let paths = match session {
                Some(s) => {
                    // SAFETY: no other thread runs yet; a core this starts inherits it.
                    unsafe { std::env::set_var("HC_SESSION", s) };
                    Paths::from_env()?
                }
                None => paths,
            };
            connect(&paths)?;
            return Ok(None);
        }
        Cmd::Events(p) => {
            follow_events(&paths, to(&p), json)?;
            return Ok(None);
        }
        Cmd::Templates => {
            let r = call("templates.list", json!({}))?;
            let rows: Vec<String> = r["templates"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| match t["error"].as_str() {
                    Some(e) => format!("{:<16} (broken: {e})", t["name"].as_str().unwrap_or("?")),
                    None => {
                        format!("{:<16} {}", t["name"].as_str().unwrap_or("?"), t["description"].as_str().unwrap_or(""))
                    }
                })
                .collect();
            let human = if rows.is_empty() {
                format!("no templates yet: add markdown files to {}", crate::templates::dir(&paths).display())
            } else {
                rows.join("\n")
            };
            Out { json: r, human }
        }
        Cmd::Schema => {
            let r = call("rpc.discover", json!({}))?;
            let human = serde_json::to_string_pretty(&r).unwrap_or_default();
            Out { json: r, human }
        }
        Cmd::Server { action: action @ (ServerCmd::Start | ServerCmd::Status | ServerCmd::Restart) } => {
            if matches!(action, ServerCmd::Restart)
                && let Some(stream) = client::connect_existing(&paths)
            {
                client::call(&stream, "server.restart", json!({}))?;
                wait_closed(&stream);
            }
            let r = if crate::platform::status_may_start_core() {
                call("server.status", json!({}))?
            } else {
                let stream = if matches!(action, ServerCmd::Status) {
                    client::require_existing(&paths)?
                } else {
                    client::connect_or_start_explicit(&paths)?
                };
                client::call(&stream, "server.status", json!({}))?
            };
            let id = r["core"]["id"].as_str().unwrap_or("?");
            let human = format!(
                "core {} ({}) running, pid {}, up {}s",
                r["core"]["name"].as_str().unwrap_or("?"),
                id.get(..8).unwrap_or(id),
                r["pid"],
                r["uptime_ms"].as_u64().unwrap_or(0) / 1000
            );
            Out { json: r, human }
        }
        Cmd::Server { action: ServerCmd::Stop } => stop(&paths)?,
        Cmd::Update { from: None, version, force } => {
            let this = format!("hc {}", env!("CARGO_PKG_VERSION"));
            if let Some(v) = &version
                && !force
                && !older(&this, &format!("hc {v}"))
                && !crate::platform::needs_bundle_update()
            {
                let human = format!("{this} is already {v} or newer");
                return Ok(Some(up_to_date(&paths, v, human)?));
            }
            // Only when asked: hc never checks for updates on its own.
            let scratch = crate::releases::Scratch::new()?;
            let requested = version
                .as_deref()
                .map(|v| if !force && !older(&this, &format!("hc {v}")) { env!("CARGO_PKG_VERSION") } else { v });
            let m = crate::releases::manifest(requested, &scratch.0)?;
            // Apps and scripts built for this generation would break on another one.
            if m.generation != crate::api::GENERATION && !force {
                return Err(RpcError::new(
                    "refused",
                    format!(
                        "hc {} speaks protocol generation {}, this hc {}: apps built for it would stop working",
                        m.version,
                        m.generation,
                        crate::api::GENERATION
                    ),
                    "update the apps that use hc first, then hc update --force",
                ));
            }
            if version.is_none()
                && !force
                && !older(&this, &format!("hc {}", m.version))
                && !crate::platform::needs_bundle_update()
            {
                let human = format!("{this} is up to date (latest release: {})", m.version);
                up_to_date(&paths, &m.version, human)?
            } else {
                let file = crate::releases::download(
                    &m,
                    &crate::releases::this_target(),
                    &scratch.0,
                    crate::platform::prefer_release_bundle(),
                )?;
                let mut out = update(&paths, &file, force)?;
                out.json["release"] = m.version.into();
                out
            }
        }
        Cmd::Update { from: Some(from), force, .. } => update(&paths, &from, force)?,
        Cmd::Machine { action } => machine(&paths, action, call)?,
        Cmd::File { action } => match file(action, asked, call)? {
            Some(out) => out,
            None => return Ok(None),
        },
    }))
}

/// A template's task is plain text, so `--name x` after it is hc's, not part
/// of the task. A program's arguments stay its own.
/// Flags given before the program stay; flags after the task are added.
/// Also says whether those flags asked for --json.
fn flags_after_task(paths: &Paths, p: RunParams) -> Result<(RunParams, bool), RpcError> {
    let Some((q, json)) = trailing_flags(paths, &p.argv)? else { return Ok((p, false)) };
    let merged = RunParams {
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
        ..p
    };
    if merged.parent.is_some() && merged.no_parent {
        return Err(RpcError::new("usage", "--parent and --no-parent together", "see `hc run --help`"));
    }
    Ok((merged, json))
}

/// The task with hc's flags moved out of it, parsed on their own (behind a
/// `--`, so a flag missing its value can't take the template's name).
fn trailing_flags(paths: &Paths, argv: &[String]) -> Result<Option<(RunParams, bool)>, RpcError> {
    use clap::CommandFactory;
    if argv.len() < 2 || !matches!(crate::templates::load(paths, &argv[0]), Ok(Some(_))) {
        return Ok(None);
    }
    let cli = Cli::command();
    let Some(run) = cli.find_subcommand("run") else { return Ok(None) };
    let (mut flags, mut task) = (Vec::new(), Vec::new());
    let mut rest = argv[1..].iter();
    while let Some(a) = rest.next() {
        let long = a.strip_prefix("--").map(|l| l.split('=').next().unwrap_or(l));
        match long.and_then(|l| run.get_arguments().chain(cli.get_arguments()).find(|arg| arg.get_long() == Some(l))) {
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
    let args =
        ["hc".to_string(), "run".into()].into_iter().chain(flags).chain(["--".into(), argv[0].clone()]).chain(task);
    match Cli::try_parse_from(args) {
        Ok(Cli { cmd: Some(Cmd::Run(q)), json, .. }) => Ok(Some((q, json))),
        Ok(_) => Ok(None),
        Err(e) => Err(RpcError::new("usage", usage_message(&e), "see `hc run --help`")),
    }
}

/// A request as the JSON the API takes.
fn to(params: &impl serde::Serialize) -> Value {
    serde_json::to_value(params).unwrap_or_default()
}

/// One string with spaces or shell signs is a shell line, as in a bash tool.
fn shell_line(argv: &[String]) -> Option<String> {
    let shell_sign = |c: char| c.is_whitespace() || ";|&<>$`*?(){}~'\"".contains(c);
    match argv {
        [line] if line.contains(shell_sign) => Some(line.clone()),
        _ => None,
    }
}

/// The directory we run in, as the shell names it ($PWD keeps symlinks like
/// macOS /var/tmp), else as the OS resolves it.
fn here() -> Option<String> {
    let real = std::env::current_dir().ok()?;
    let logical = std::env::var("PWD").ok().filter(|p| std::fs::canonicalize(p).ok() == real.canonicalize().ok());
    Some(logical.unwrap_or_else(|| real.display().to_string()))
}

/// A folder here as the caller means it: relative ones start where hc runs,
/// not where the core does.
fn from_here(cwd: Option<String>) -> Option<String> {
    match (cwd, here()) {
        (Some(cwd), Some(here)) => Some(std::path::Path::new(&here).join(cwd).display().to_string()),
        (cwd, here) => cwd.or(here),
    }
}

/// Who is asking: this hc terminal, else the shell or script that ran us.
fn caller() -> String {
    client::terminal_id().unwrap_or_else(|| format!("ppid:{}", crate::platform::parent_pid()))
}

/// Prints the snapshot, then every notification until the core goes away.
fn follow_events(paths: &Paths, params: Value, json: bool) -> Result<(), RpcError> {
    use std::io::BufRead;
    let stream = client::connect_or_start(paths)?;
    let snapshot = client::call(&stream, "events.subscribe", params)?;
    if json {
        println!("{snapshot}");
    } else {
        println!("{}", table(&json!({"terminals": snapshot["terminals"]})));
    }
    for line in std::io::BufReader::new(&stream).lines() {
        let Ok(line) = line else { break };
        if json {
            println!("{line}");
            continue;
        }
        let msg: Value = serde_json::from_str(&line).unwrap_or_default();
        let p = &msg["params"];
        match msg["method"].as_str() {
            Some("event") if p["type"] == "doc" => {
                let name = p["data"]["name"].as_str().unwrap_or("?");
                let machine = p["machine"].as_str().map(|m| format!("{m}/")).unwrap_or_default();
                println!("{machine}{name}  doc  version {}", p["data"]["version"]);
            }
            Some("event") => println!(
                "{:<6} {:<10} {}",
                p["terminal"].as_str().unwrap_or("?"),
                p["type"].as_str().unwrap_or("?"),
                p["data"]
            ),
            Some("missed") => {
                println!("[missed some events; {} terminals now]", p["terminals"].as_array().map_or(0, Vec::len))
            }
            _ => {}
        }
    }
    Ok(())
}

/// Stops the core and returns once it has exited (its connection closes).
fn stop(paths: &Paths) -> Result<Out, RpcError> {
    let Some(stream) = client::connect_existing(paths) else {
        return Ok(Out { json: json!({"stopped": false}), human: "no core running".into() });
    };
    let r = client::call(&stream, "server.stop", json!({}))?;
    wait_closed(&stream);
    Ok(Out { json: r, human: "core stopped".into() })
}

/// A run that waited: its output as text (like read), a last line when there
/// is more to say, and the program's exit code as hc's (124: still running).
fn ran(r: &Value) -> Result<Option<Out>, RpcError> {
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

/// The core closes every connection when its process exits.
fn wait_closed(stream: &crate::platform::Stream) {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
    let _ = (&*stream).read_to_end(&mut Vec::new());
}

/// Copies bytes between stdin/stdout and the core: plain or framed, untouched.
fn connect(paths: &Paths) -> Result<(), RpcError> {
    let stream = client::connect_or_start(paths)?;
    let mut to_core = stream.try_clone().map_err(|e| RpcError::internal(e.to_string()))?;
    std::thread::spawn(move || {
        pump(&mut *crate::platform::console::stdin(), &mut to_core);
        let _ = to_core.shutdown(std::net::Shutdown::Write);
    });
    // Unbuffered: std's stdout holds bytes until a newline, and frames have none.
    pump(&mut &stream, &mut *crate::platform::console::stdout());
    Ok(())
}

fn pump(from: &mut impl Read, to: &mut impl std::io::Write) {
    let mut buf = [0u8; 64 * 1024];
    while let Ok(n @ 1..) = from.read(&mut buf) {
        if to.write_all(&buf[..n]).is_err() {
            return;
        }
    }
}

/// Which API method each command calls. Methods missing here are API-only.
pub const COMMANDS: &[(&str, &str)] = &[
    ("run", "terminal.run"),
    ("ls", "terminal.list"),
    ("get", "terminal.get"),
    ("kill", "terminal.kill"),
    ("rm", "terminal.rm"),
    ("label", "terminal.label"),
    ("keep", "terminal.keep"),
    ("send", "terminal.send"),
    ("wait", "terminal.wait"),
    ("keys", "terminal.keys"),
    ("interrupt", "terminal.interrupt"),
    ("attach", "terminal.attach"),
    ("read", "terminal.read"),
    ("put", "terminal.put"),
    ("exec", "machine.exec"),
    ("file read", "file.read"),
    ("file write", "file.write"),
    ("file edit", "file.edit"),
    ("events", "events.subscribe"),
    ("templates", "templates.list"),
    ("schema", "rpc.discover"),
    ("server start", "server.status"),
    ("server status", "server.status"),
    ("server stop", "server.stop"),
    ("server restart", "server.restart"),
    ("machine add", "machine.add"),
    ("machine ls", "machine.list"),
    ("machine login", "machine.connect"),
    ("machine rename", "machine.rename"),
    ("machine rm", "machine.rm"),
];
/// Client-only methods: handshake and opaque document storage.
pub const API_ONLY: &[&str] = &["hello", "doc.get", "doc.set", "file.upload"];

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parsed(args: &[&str]) -> Cmd {
        Cli::try_parse_from([&["hc"], args].concat()).unwrap().cmd.unwrap()
    }

    #[test]
    fn the_picker_orders_by_last_change_by_what_needs_you_and_by_name() {
        let t = |address: &str, name: &str, state: Option<&str>, ms: u64| {
            let status = state.map(|s| serde_json::json!({"state": s}));
            serde_json::json!({"address": address, "name": name, "state": "live", "created_ms": 1,
                "status": status, "status_ms": state.map(|_| ms)})
        };
        let all = [
            t("t1", "web", Some("working"), 30),
            t("t2", "api", Some("done"), 20),
            t("t3", "shell", None, 0),
            t("t4", "Docs", Some("blocked"), 10),
            t("t5", "cli", Some("idle"), 40),
        ];
        let orders: Vec<(&str, String)> = orders(&all)
            .into_iter()
            .map(|(name, ts)| (name, ts.iter().map(|t| t["address"].as_str().unwrap()).collect::<Vec<_>>().join(" ")))
            .collect();
        assert_eq!(
            orders,
            [
                ("recent", "t5 t1 t2 t4 t3".into()),
                ("status", "t4 t1 t5 t2 t3".into()),
                ("name", "t2 t5 t4 t3 t1".into()),
            ]
        );
    }

    #[test]
    fn picker_rows_narrower_than_the_headings_list_without_a_header() {
        let t = serde_json::json!({"address": "t1", "argv": ["pi"], "cwd": "/a"});
        assert_eq!(items(&[t], false).len(), 1);
    }

    #[test]
    fn rows_say_where_a_terminal_lives_else_its_folder_and_no_other_labels() {
        let t = |extra: Value| {
            let mut t = serde_json::json!({"address": "t1", "argv": ["pi"], "cwd": "/a/bliss", "state": "live"});
            t.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            let item = items(&[t], false).remove(0);
            (picker::plain(&item.rest), item.short)
        };
        let labels = serde_json::json!({"where": "herdr-core / review", "herdr.tab": "tab3", "ws": "x9"});
        let (rest, short) = t(serde_json::json!({"labels": labels, "name": "rev"}));
        assert_eq!(short, "rev \u{b7} herdr-core / review");
        assert!(
            rest.contains("herdr-core / review")
                && !rest.contains("bliss")
                && !rest.contains("x9")
                && !rest.contains("tab3"),
            "{rest}"
        );
        assert_eq!(t(serde_json::json!({"labels": {"ws": "x9"}})).1, "pi \u{b7} bliss");
    }

    #[test]
    fn human_folders_follow_the_foreground_but_json_keeps_the_launch_folder() {
        for foreground in [json!({"cwd": "/current/work"}), json!({"cwd": null}), Value::Null] {
            let t = json!({"address": "t1", "argv": ["bash"], "cwd": "/launch/start", "foreground": foreground});
            let original = t.clone();
            let cwd = if foreground["cwd"].is_string() { "/current/work" } else { "/launch/start" };
            assert_eq!(folder(&t), cwd.rsplit('/').next().unwrap());
            assert!(picker::plain(&items(std::slice::from_ref(&t), false)[0].rest).contains(&folder(&t)));
            assert!(describe(&t).contains(&format!("  cwd: {cwd}\n")));
            assert_eq!(short(&t), json!({"address": "t1", "cwd": "/launch/start", "agent": "bash"}));
            assert_eq!(t, original, "formatting must not change the full get record");
        }
    }

    #[test]
    fn get_keeps_an_unsent_first_message_visible_to_people() {
        let mut t = json!({"address": "t1", "argv": ["pi"], "state": "live"});
        assert!(!describe(&t).contains("first message"));
        t["first_message"] = json!({"sent": false, "reason": "output did not settle"});
        assert!(describe(&t).contains("first message NOT sent: output did not settle"));
    }

    #[test]
    fn every_api_method_has_a_command_or_is_api_only() {
        Cli::command().debug_assert();
        for method in crate::api::METHOD_NAMES {
            let covered = COMMANDS.iter().any(|(_, m)| m == method) || API_ONLY.contains(method);
            assert!(covered, "{method} has no command and is not marked API-only");
        }
        for (command, _) in COMMANDS {
            let mut cmd = Cli::command();
            for word in command.split(' ') {
                cmd = cmd.find_subcommand(word).unwrap_or_else(|| panic!("no command `{command}`")).clone();
            }
        }
    }

    #[test]
    fn a_command_line_becomes_the_api_request() {
        let Cmd::Send(p) = parsed(&["send", "t3", "hi there", "--if", "idle", "--wait", "--timeout", "2.5"]) else {
            panic!()
        };
        let want = json!({"terminal": "t3", "text": "hi there", "if": "idle", "force": false, "wait": true, "timeout": 2.5, "caller": null});
        assert_eq!(to(&p), want);
        assert!(serde_json::from_value::<SendParams>(want).is_ok(), "and the API reads it back");

        let Cmd::Wait(p) = parsed(&["wait", "t3", "--output", "ready"]) else { panic!() };
        assert_eq!(to(&p)["output"], "ready");

        let Cmd::Run(p) = parsed(&["run", "--label", "a=1", "--label", "b=x=y", "--label", "a=2", "sh", "-c", "true"])
        else {
            panic!()
        };
        assert_eq!(
            to(&p)["labels"],
            json!({"a": "2", "b": "x=y"}),
            "an object; values may hold =; the last repeat wins"
        );
        assert_eq!(to(&p)["argv"], json!(["sh", "-c", "true"]));
    }

    #[test]
    fn a_wait_prints_what_happened() {
        let one = |reason: &str| json!({"terminal": "t1", "ended": reason != "timeout", "reason": reason, "status": "working", "turn": 2, "exit_code": 42, "children": []});
        assert!(describe_wait(&one("timeout")).starts_with("t1 still running: working"));
        assert!(describe_wait(&one("exit")).starts_with("t1 exited (exit 42"));
        let all = json!({"terminal": "t0", "reason": "children", "children": [one("exit")]});
        assert!(describe_wait(&all).starts_with("t1 exited"));
        let none = json!({"terminal": "t0", "reason": "children", "children": []});
        assert_eq!(describe_wait(&none), "no children are working");
        let message = json!({"terminal": "t0", "reason": "message", "from": "t2", "children": []});
        assert!(describe_wait(&message).contains("t2"), "{}", describe_wait(&message));
        let mut interrupted = one("message");
        interrupted["ended"] = false.into();
        interrupted["from"] = "t1".into();
        assert_eq!(
            describe_wait(&interrupted),
            "a message from t1 (already in your input)\nt1 still running: working, turn 2 (wait again to keep waiting)"
        );
    }

    #[test]
    fn older_compares_versions_as_numbers() {
        assert!(older("hc 0.1.9", "hc 0.2.0") && older("hc 0.2.0", "hc 0.10.0"));
        assert!(!older("hc 0.2.0", "hc 0.2.0") && !older("hc 0.10.0", "hc 0.9.9") && !older("hc dev", "hc 0.2.0"));
    }

    #[test]
    fn labels_need_a_key() {
        assert!(Cli::try_parse_from(["hc", "label", "t1", "nokey"]).is_err());
        assert!(Cli::try_parse_from(["hc", "label", "t1", "=v"]).is_err());
        let Cmd::Label(p) = parsed(&["label", "t1", "gone="]) else { panic!() };
        assert_eq!(to(&p)["labels"], json!({"gone": ""}), "an empty value removes it");
    }
}
