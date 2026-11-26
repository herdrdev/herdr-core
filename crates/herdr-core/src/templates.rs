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
    /// Default shell command line that starts the agent.
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

/// Apply a per-run model to the effective template command. The caller also exports HC_MODEL.
pub(crate) fn with_model(mut command: String, model: &str) -> Result<String, RpcError> {
    if model.is_empty() || !model.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:/@-".contains(&b)) {
        return Err(RpcError::invalid_params("--model must match [A-Za-z0-9._:/@-]+"));
    }
    let words = shell_words(&command)?;
    if references_model(&words)? {
        return Ok(command);
    }
    let Some((at, value)) = model_argument(&words)? else {
        return Err(RpcError::invalid_params(
            "--model requires HC_MODEL or an existing -m or --model value in the final command or pipeline",
        ));
    };
    command.replace_range(at..at + value.len(), model);
    Ok(command)
}

const COMMAND_END: [char; 6] = ['\n', ';', '|', '&', '(', ')'];
type ShellWord<'a> = (usize, &'a str, bool);

fn references_model(words: &[ShellWord<'_>]) -> Result<bool, RpcError> {
    let mut words = words.iter().copied().peekable();
    let mut program = true;
    while let Some((at, raw, expands_model)) = words.next() {
        if expands_model {
            return Ok(true);
        }
        if raw.starts_with(COMMAND_END) {
            program = true;
            continue;
        }
        let (_, word) = unquote((at, raw));
        if !program || matches!(word, "exec" | "env" | "command") || word.contains('=') {
            continue;
        }
        program = false;
        let name = word.rsplit('/').next().unwrap_or_default();
        let next = words.peek().map(|&(at, word, _)| unquote((at, word)).1);
        if name == "printenv" && next == Some("HC_MODEL") {
            return Ok(true);
        }
        // A shell's quoted script is evaluated by the child shell.
        if matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash") {
            while let Some(&(at, raw, expands_model)) = words.peek() {
                let (_, flag) = unquote((at, raw));
                if !flag.starts_with(['-', '+']) || matches!(flag, "-" | "+" | "--") {
                    break;
                }
                words.next();
                if expands_model {
                    return Ok(true);
                }
                if let Some(short) = flag.strip_prefix('-')
                    && !short.starts_with('-')
                    && short.contains('c')
                {
                    if let Some((_, script, expands_model)) = words.next()
                        && (expands_model || script_references_model(script)?)
                    {
                        return Ok(true);
                    }
                    break;
                }
                if matches!(flag, "-o" | "+o" | "-O" | "+O" | "--rcfile" | "--init-file")
                    && words.next().is_some_and(|(_, _, expands_model)| expands_model)
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn script_references_model(word: &str) -> Result<bool, RpcError> {
    let (at, script) = unquote((0, word));
    if at == 0 || !word.starts_with('"') {
        return references_model(&shell_words(script)?);
    }
    // The outer shell removes these escapes before passing the script to its child.
    let mut chars = script.chars().peekable();
    let mut decoded = String::with_capacity(script.len());
    while let Some(mut c) = chars.next() {
        if c == '\\'
            && let Some(&next @ ('$' | '`' | '"' | '\\' | '\n')) = chars.peek()
        {
            chars.next();
            if next == '\n' {
                continue;
            }
            c = next;
        }
        decoded.push(c);
    }
    references_model(&shell_words(&decoded)?)
}

fn model_argument<'a>(words: &[ShellWord<'a>]) -> Result<Option<(usize, &'a str)>, RpcError> {
    let mut words = words.iter().copied().peekable();
    let mut options = true;
    let mut first = true;
    let mut new_pipeline = true;
    let mut model = None;
    let mut ambiguous = false;
    let mut groups = Vec::new();
    while let Some((start, raw, _)) = words.next() {
        if raw == "(" {
            groups.push(if new_pipeline { (None, false) } else { (model, ambiguous) });
            model = None;
            ambiguous = false;
            options = true;
            first = true;
            new_pipeline = true;
            continue;
        } else if raw == ")" {
            let Some((outer, outer_ambiguous)) = groups.pop() else {
                return Err(RpcError::invalid_params("--model could not match a shell command group"));
            };
            ambiguous |= outer_ambiguous || (outer.is_some() && model.is_some());
            model = outer.or(model);
            options = false;
            first = false;
            continue;
        } else if raw.starts_with(COMMAND_END) {
            options = true;
            if raw != "\n" || !first {
                new_pipeline = !matches!(raw, "|" | "|&");
            }
            first = true;
            continue;
        } else if raw.starts_with(['<', '>']) {
            words.next(); // the redirection's operand
            continue;
        } else if first {
            // The launch pipeline follows any shell setup commands.
            if new_pipeline {
                model = None;
                ambiguous = false;
            }
            first = false;
            continue;
        }
        let (at, arg) = unquote((start, raw));
        let flag = if raw.starts_with('\'') || !arg.contains("\\\n") {
            std::borrow::Cow::Borrowed(arg)
        } else {
            std::borrow::Cow::Owned(arg.replace("\\\n", ""))
        };
        if flag == "--" {
            options = false;
        } else if options {
            let candidate = if flag.starts_with("--model=")
                && let Some((prefix, value)) = arg.split_once('=')
            {
                let value = (at + prefix.len() + 1, value);
                Some(if at == start { unquote(value) } else { value })
            } else if matches!(flag.as_ref(), "-m" | "--model")
                && let Some(&(at, next, _)) = words.peek()
                && !next.starts_with(['\n', ';', '|', '&', '(', ')', '<', '>'])
            {
                let value = unquote((at, next));
                if !value.1.starts_with('-') {
                    words.next();
                    Some(value)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(value) = candidate {
                ambiguous |= model.replace(value).is_some();
            }
        }
    }
    if !groups.is_empty() {
        return Err(RpcError::invalid_params("--model could not match a shell command group"));
    }
    if ambiguous {
        return Err(RpcError::invalid_params(
            "--model found multiple model arguments in the final command or pipeline. Use HC_MODEL to select the model",
        ));
    }
    Ok(model)
}

fn unquote((at, word): (usize, &str)) -> (usize, &str) {
    for quote in ['\'', '"'] {
        if let Some(inner) = word.strip_prefix(quote).and_then(|s| s.strip_suffix(quote)) {
            return (at + 1, inner);
        }
    }
    (at, word)
}

fn model_parameter(after_dollar: &str) -> bool {
    let parameter = after_dollar.strip_prefix('{').map_or(after_dollar, |p| p.strip_prefix('#').unwrap_or(p));
    parameter
        .strip_prefix("HC_MODEL")
        .is_some_and(|suffix| !suffix.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
}

fn here_doc_delimiter(word: &str) -> (String, bool) {
    let mut delimiter = String::new();
    let mut quote = None;
    let mut quoted = false;
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            _ if quote == Some(c) => quote = None,
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                quoted = true;
            }
            '\\' if quote != Some('\'') => {
                match chars.next_if(|&next| quote.is_none() || matches!(next, '$' | '`' | '"' | '\\' | '\n')) {
                    Some('\n') => {}
                    Some(next) => {
                        quoted = true;
                        delimiter.push(next);
                    }
                    None => delimiter.push(c),
                }
            }
            _ => delimiter.push(c),
        }
    }
    (delimiter, !quoted)
}

fn skip_here_doc<'a>(
    mut rest: &'a str,
    delimiter: &str,
    tabs: bool,
    expand: bool,
) -> Result<(&'a str, bool), RpcError> {
    let mut body = String::new();
    let mut joined = String::new();
    for raw in rest.split_inclusive('\n') {
        rest = &rest[raw.len()..];
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = if tabs { line.trim_start_matches('\t') } else { line };
        if expand && raw.ends_with('\n') && line.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1 {
            joined.push_str(&line[..line.len() - 1]);
            continue;
        }
        let line = if joined.is_empty() {
            line
        } else {
            joined.push_str(line);
            &joined
        };
        if delimiter == line {
            return Ok((rest, expand && scan_shell_word(&body, true).1));
        }
        if expand {
            body.push_str(line);
            body.push('\n');
        }
        joined.clear();
    }
    Err(RpcError::invalid_params("--model could not find the end of a here-document"))
}

fn shell_operator(c: char) -> bool {
    COMMAND_END.contains(&c) || matches!(c, '<' | '>')
}

fn scan_shell_word(source: &str, here_doc: bool) -> (usize, bool) {
    // Quotes in a here-document are literal until a substitution enters shell syntax.
    let mut quote = here_doc.then_some('\0');
    let mut escaped = false;
    let mut expands_model = false;
    let mut expansion = Vec::new();
    let mut dollar = false;
    let mut comment = false;
    let mut word_start = true;
    for (at, c) in source.char_indices() {
        if comment && c != '\n' {
            continue;
        }
        comment = false;
        match c {
            _ if escaped => {
                escaped = false;
                continue;
            }
            '\\' if quote != Some('\'') => escaped = true,
            '$' if quote != Some('\'') && !dollar => expands_model |= model_parameter(&source[at + 1..]),
            '(' | '{' if dollar && quote != Some('\'') => {
                expansion.push((if c == '(' { ')' } else { '}' }, quote.take()));
            }
            _ if quote == Some(c) => quote = None,
            '\'' | '"' if quote.is_none() => quote = Some(c),
            _ if quote.is_none() && expansion.last().is_some_and(|&(close, _)| c == close) => {
                quote = expansion.pop().and_then(|(_, outer_quote)| outer_quote);
            }
            '`' if quote != Some('\'') => expansion.push(('`', quote.take())),
            _ if quote.is_none() => {
                if let Some(&(close, _)) = expansion.last() {
                    if c == '(' && close == ')' {
                        expansion.push((')', None));
                    } else if c == '#' && word_start && matches!(close, ')' | '`') {
                        comment = true;
                    }
                } else if shell_operator(c) || matches!(c, ' ' | '\t') {
                    return (at, expands_model);
                }
            }
            _ => {}
        }
        dollar = c == '$' && !dollar;
        word_start = quote.is_none() && (shell_operator(c) || matches!(c, ' ' | '\t'));
    }
    (source.len(), expands_model)
}

/// Find shell word spans without changing spacing, quotes, or escapes in the command.
fn shell_words(command: &str) -> Result<Vec<ShellWord<'_>>, RpcError> {
    let mut rest = command;
    let mut document = None;
    let mut documents = Vec::new();
    let mut words = Vec::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        while let Some(next) = rest.strip_prefix("\\\n") {
            rest = next.trim_start_matches([' ', '\t']);
        }
        if rest.starts_with('#') {
            rest = rest.find('\n').map_or("", |at| &rest[at..]);
        }
        let Some(first) = rest.chars().next() else { break };
        let operator = shell_operator(first);
        let (end, mut expands_model) = if operator {
            let end = match rest.as_bytes() {
                [b'<', b'<', b'<', ..] => 3,
                [b'<', b'<', b'-', ..] => {
                    document = Some(true);
                    3
                }
                [b'<', b'<', ..] => {
                    document = Some(false);
                    2
                }
                [b'>', b'>' | b'&' | b'|', ..]
                | [b'<', b'>' | b'&', ..]
                | [b'|', b'|' | b'&', ..]
                | [b'&', b'&', ..] => 2,
                _ => 1,
            };
            (end, false)
        } else {
            scan_shell_word(rest, false)
        };
        let at = command.len() - rest.len();
        let word = &rest[..end];
        rest = &rest[end..];
        if !operator && let Some(tabs) = document.take() {
            let (delimiter, expand) = here_doc_delimiter(word);
            documents.push((delimiter, tabs, expand));
            expands_model = false;
        }
        if first == '\n' {
            for (delimiter, tabs, expand) in documents.drain(..) {
                let (next, reads_model) = skip_here_doc(rest, &delimiter, tabs, expand)?;
                rest = next;
                expands_model |= reads_model;
            }
        }
        words.push((at, word, expands_model));
    }
    if document.is_some() || !documents.is_empty() {
        return Err(RpcError::invalid_params("--model could not find the body of a here-document"));
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_overrides_replace_only_the_model_value() {
        for (command, expected) in [
            ("MODE=dev codex\t-m 'old model' --flag", "MODE=dev codex\t-m 'new/model' --flag"),
            (r#"claude --model "old" --flag"#, r#"claude --model "new/model" --flag"#),
            ("pi --model='old' --thinking high", "pi --model='new/model' --thinking high"),
            (r#"pi "--model=old" --flag"#, r#"pi "--model=new/model" --flag"#),
            ("pi --model=$(pick model) --flag", "pi --model=new/model --flag"),
            ("pi --model `pick model` --flag", "pi --model new/model --flag"),
            ("pi --model \\\n  'old' --flag", "pi --model \\\n  'new/model' --flag"),
            ("pi --mo\\\ndel old --flag", "pi --mo\\\ndel new/model --flag"),
            ("pi --mo\\\ndel=\"old\"", "pi --mo\\\ndel=\"new/model\""),
            (r#"pi --prompt '$HC_MODEL' -m old"#, r#"pi --prompt '$HC_MODEL' -m new/model"#),
            (r#"pi --prompt \$HC_MODEL -m old"#, r#"pi --prompt \$HC_MODEL -m new/model"#),
            ("pi --prompt HC_MODEL -m old", "pi --prompt HC_MODEL -m new/model"),
            ("mkdir -m 700 dir && pi --model old", "mkdir -m 700 dir && pi --model new/model"),
            ("mkdir -m 700 dir && pi -m old", "mkdir -m 700 dir && pi -m new/model"),
            ("pi --model old | tee output", "pi --model new/model | tee output"),
            ("pi --model old |\n  tee output", "pi --model new/model |\n  tee output"),
            ("pi --model old | (cat)", "pi --model new/model | (cat)"),
            ("(pi --model old) | cat", "(pi --model new/model) | cat"),
            (
                "cat <<'END'\n--model example\nEND\npi --model old",
                "cat <<'END'\n--model example\nEND\npi --model new/model",
            ),
            (
                "printf --model early <<E\"OF\"\nignored\nEOF\npi --model old",
                "printf --model early <<E\"OF\"\nignored\nEOF\npi --model new/model",
            ),
            (
                "cat <<EOF\n$(printf '%s' '$HC_MODEL')\nEOF\npi --model old",
                "cat <<EOF\n$(printf '%s' '$HC_MODEL')\nEOF\npi --model new/model",
            ),
            (
                r#"cd app && pi --prompt 'use -m old' --model old\ model"#,
                "cd app && pi --prompt 'use -m old' --model new/model",
            ),
        ] {
            assert_eq!(with_model(command.into(), "new/model").unwrap(), expected);
        }
    }

    #[test]
    fn a_model_reference_takes_precedence_over_a_model_flag() {
        for command in [
            r#"launch "$HC_MODEL" -m old"#,
            "launch ${HC_MODEL:-default}",
            r#"echo "${#HC_MODEL}""#,
            "printenv HC_MODEL",
            r#"sh -c 'launch "$HC_MODEL"' -m old"#,
            r#"sh -l -c 'launch "$HC_MODEL"'"#,
            r#"sh -c "launch \"\$HC_MODEL\"" -m old"#,
            "cat <<END\nmodel: '$HC_MODEL'\nEND",
            "cat <<-END\n\tmodel: $HC_MO\\\nDEL\n\tEND",
            "cat <<END\n$(printf '%s' \"$HC_MODEL\")\nEND",
        ] {
            assert_eq!(with_model(command.into(), "provider/model:v1@rc-2.0_a").unwrap(), command);
        }
    }

    #[test]
    fn model_overrides_reject_ambiguous_flags() {
        for command in ["pi -m old --model=other", "pi -m old | other --model=old", "pi -m old | (other --model=old)"] {
            let error = with_model(command.into(), "new").unwrap_err();
            assert_eq!(error.kind, "invalid_params");
            assert!(error.message.contains("multiple model arguments"));
        }
    }

    #[test]
    fn model_overrides_reject_an_unterminated_here_document_before_using_an_earlier_match() {
        let error = with_model("printf --model early <<E\"OF\"\nignored\npi --model old".into(), "new").unwrap_err();
        assert_eq!(error.kind, "invalid_params");
        assert!(error.message.contains("end of a here-document"));
    }

    #[test]
    fn model_overrides_require_a_model_argument_or_reference() {
        for command in [
            "launch $HC_MODEL_DEFAULT",
            "launch --message 'use -m old'",
            "launch --message $(echo -m old)",
            "launch -- --model old",
            "launch --model --other",
            "launch --model; echo old",
            "launch # --model old",
            "mkdir -m 700 dir && pi",
            "cat <<'END'\n--model example\nEND",
            "cat <<'END'\nmodel: $HC_MODEL\nEND",
            "cat <<END\nmodel: \\$HC_MODEL\nEND",
        ] {
            let error = with_model(command.into(), "new").unwrap_err();
            assert_eq!(error.kind, "invalid_params");
            assert!(error.message.contains("HC_MODEL"), "{command}: {error:?}");
        }
    }

    #[test]
    fn model_names_exclude_shell_syntax_and_empty_values() {
        for model in ["", "a b", "a\nb", "a\rb", "a\0b", "a;id", "$(id)", "a'b", "a\"b", "a\\b", "à"] {
            let error = with_model("launch $HC_MODEL".into(), model).unwrap_err();
            assert_eq!(error.kind, "invalid_params", "{model:?}");
            assert!(error.message.contains("--model"));
        }
    }

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
