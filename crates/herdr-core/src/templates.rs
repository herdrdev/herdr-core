//! Templates: `~/.config/herdr-core/templates/<name>.md`, read fresh on every
//! use. Frontmatter says how to start the agent; the body is its first message.

use crate::paths::Paths;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub name: String,
    pub description: String,
    /// Shell command line that starts the agent (the model is written here).
    pub command: String,
    pub body: String,
    /// `report` (default): ready at its first status report. `quiet`: ready
    /// once its output goes quiet, for agents that don't report status.
    pub ready_when_quiet: bool,
    pub submit: Option<String>,
    pub stop: Option<String>,
    pub paste_gap_ms: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
pub struct TemplateInfo {
    pub name: String,
    pub description: String,
    pub command: String,
    /// Set when the file can't be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn dir(paths: &Paths) -> PathBuf {
    paths.config.parent().expect("config file has a dir").join("templates")
}

fn parse(name: &str, text: &str) -> Result<Template, String> {
    let text = text.replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n").ok_or("no frontmatter (start the file with ---)")?;
    let end = rest.find("\n---").ok_or("frontmatter never closes (---)")?;
    let (front, body) = (&rest[..end], &rest[end + 4..]);
    let mut t = Template {
        name: name.into(),
        description: String::new(),
        command: String::new(),
        body: body.strip_prefix('\n').unwrap_or(body).trim_end().into(),
        ready_when_quiet: false,
        submit: None,
        stop: None,
        paste_gap_ms: None,
    };
    for line in front.lines().filter(|l| !l.trim().is_empty()) {
        let (key, value) = line.split_once(':').ok_or_else(|| format!("not `key: value`: {line:?}"))?;
        let value = value.trim().to_string();
        match key.trim() {
            "name" => {} // the file name is the name
            "description" => t.description = value,
            "command" => t.command = value,
            "ready" => t.ready_when_quiet = value == "quiet",
            "submit" => t.submit = Some(value),
            "stop" => t.stop = Some(value),
            "paste_gap_ms" => t.paste_gap_ms = Some(value.parse().map_err(|_| "paste_gap_ms must be a number")?),
            _ => {} // unknown keys are someone else's
        }
    }
    if t.command.is_empty() {
        return Err("no command".into());
    }
    Ok(t)
}

/// The template called `name`, if there is one.
pub fn load(paths: &Paths, name: &str) -> Result<Option<Template>, RpcError> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return Ok(None);
    }
    let path = dir(paths).join(format!("{name}.md"));
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(name, &text)
            .map(Some)
            .map_err(|e| RpcError::new("invalid_template", format!("{}: {e}", path.display()), "see hc templates")),
        Err(_) => Ok(None),
    }
}

pub fn list(paths: &Paths) -> Vec<TemplateInfo> {
    let mut names: Vec<String> = std::fs::read_dir(dir(paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".md").map(String::from))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| match load(paths, &name) {
            Ok(Some(t)) => TemplateInfo { name, description: t.description, command: t.command, error: None },
            Ok(None) => TemplateInfo {
                name,
                description: String::new(),
                command: String::new(),
                error: Some("unreadable".into()),
            },
            Err(e) => TemplateInfo { name, description: String::new(), command: String::new(), error: Some(e.message) },
        })
        .collect()
}

#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq)]
pub struct FirstMessage {
    pub sent: bool,
    /// Whether the agent started working on it (see terminal.send).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<bool>,
    /// Why it was not sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The turn when it went in (see terminal.send).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
}

pub(crate) fn ready_timeout() -> Duration {
    let ms = std::env::var("HC_READY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(60_000);
    Duration::from_millis(ms)
}

impl Core {
    /// Sends a template's first message once the agent is ready for input:
    /// its first status report (or, with `ready: quiet`, its output going quiet).
    /// Without that, nothing is typed: the screen may be a dialog.
    /// From an agent, it says who asked and how to answer.
    pub async fn first_message(&self, id: u32, quiet: bool, text: &str, from: Option<&str>) -> FirstMessage {
        let not_sent = |reason: String| FirstMessage { sent: false, started: None, reason: Some(reason), turn: None };
        let live = |st: &State| st.reg.terminals.get(&id).is_some_and(|t| t.is_live());
        let deadline = tokio::time::Instant::now() + ready_timeout();
        if quiet {
            loop {
                let quiet_since = self.lock().last_output.get(&id).map(|t| t.elapsed() >= crate::input::OUTPUT_SETTLE);
                if quiet_since == Some(true) || !live(&self.lock()) {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return not_sent(format!(
                        "its output never went quiet; check its screen (hc read {0}), then hc send {0}",
                        crate::terminals::address(id)
                    ));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } else {
            let reported = |st: &State| st.reg.terminals.get(&id).is_none_or(|t| t.reported || !t.is_live());
            let reported_now = |st: &State| st.reg.terminals.get(&id).is_some_and(|t| t.reported);
            if !self.wait_for(ready_timeout(), reported).await || !reported_now(&self.lock()) {
                if !live(&self.lock()) {
                    return not_sent("it exited before it was ready".into());
                }
                return not_sent(format!(
                    "it never reported its status, so it may be showing a dialog; check its screen (hc read {0}), then hc send {0}",
                    crate::terminals::address(id)
                ));
            }
        }
        if !live(&self.lock()) {
            return not_sent("it exited before it was ready".into());
        }
        let text = self.tagged(from, id, text, true);
        loop {
            match self.send_first_task(id, &text).await {
                Ok(sent) => {
                    return FirstMessage { sent: true, started: sent.started, reason: None, turn: Some(sent.turn) };
                }
                // Only settle failures guarantee no input was typed. Never retry a paste.
                Err(e)
                    if e.kind == "unavailable"
                        && e.message.ends_with("is still starting: its output has not settled")
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => return not_sent(format!("{}; {}", e.message, e.hint)),
            }
        }
    }
}

/// The first message: instructions, then the task.
pub fn first_message(t: &Template, prompt: &str) -> String {
    [t.body.trim(), prompt.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_frontmatter_and_body_match_lf() {
        let text = "---\nname: reviewer\ndescription: Review code\ncommand: pi\nready: quiet\n\
                    submit: enter\nstop: ctrl-c\npaste_gap_ms: 10\n---\nFirst line.\nSecond line.\n";
        let expected = parse("reviewer", text).unwrap();
        assert_eq!(expected.body, "First line.\nSecond line.");
        assert_eq!(parse("reviewer", &text.replace('\n', "\r\n")).unwrap(), expected);
        let mixed = text.replacen("---\n", "---\r\n", 1).replace("Second line.\n", "Second line.\r\n");
        assert_eq!(parse("reviewer", &mixed).unwrap(), expected);
    }

    #[test]
    fn frontmatter_and_body_parse_like_the_spec_example() {
        let text = "---\nname: reviewer\ndescription: Read-only code review. Give it file paths and scope.\n\
                    command: pi --provider openai-codex --model gpt-6-astra --thinking high\n---\n\
                    You are a focused review agent.\nReturn findings.\n";
        let t = parse("reviewer", text).unwrap();
        assert_eq!(t.command, "pi --provider openai-codex --model gpt-6-astra --thinking high");
        assert_eq!(t.description, "Read-only code review. Give it file paths and scope.");
        assert_eq!(
            first_message(&t, "review src/relay.rs"),
            "You are a focused review agent.\nReturn findings.\n\nreview src/relay.rs"
        );
        assert!(!t.ready_when_quiet);
        assert!(parse("x", "no frontmatter").is_err());
        assert!(parse("x", "---\ndescription: d\n---\n").unwrap_err().contains("no command"));
    }
}
