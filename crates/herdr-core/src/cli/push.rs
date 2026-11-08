//! `hc push …`: webhooks the core POSTs events to.

use super::*;
use crate::api::{PushAddParams, PushEventsParams, PushNameParams};

#[derive(Subcommand)]
pub(super) enum PushCmd {
    /// Add a webhook, or replace one with the same name: hc push add phone --url https://… [--format unifiedpush …].
    Add(PushAddParams),
    /// List webhooks and what their POSTs observed.
    Ls,
    /// Forget a webhook.
    Rm(PushNameParams),
    /// POST one test event now and print the HTTP status that came back.
    Test(PushNameParams),
    /// Pushed events, whole: one (--seq N --ms T), or all after one (--since SEQ:MS).
    Events(PushEventsParams),
    /// This core's VAPID public key, for UnifiedPush.register(..., vapid).
    Key,
}

pub(super) fn push(action: PushCmd, call: impl Fn(&str, Value) -> Result<Value, RpcError>) -> Result<Out, RpcError> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    Ok(match action {
        PushCmd::Add(p) => {
            let r = call("push.add", to(&p))?;
            let human = format!("added webhook {} ({}): {}", s(&r["name"]), s(&r["format"]), s(&r["url"]));
            Out { json: r, human }
        }
        PushCmd::Ls => {
            let r = call("push.list", json!({}))?;
            let rows: Vec<String> = r["webhooks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|h| {
                    let state = if h["gone"] == true {
                        "gone".to_string()
                    } else if let Some(e) = h["last_error"].as_str() {
                        format!("failing: {e}")
                    } else if h["last_ok_ms"].is_u64() {
                        "ok".into()
                    } else {
                        "nothing sent yet".into()
                    };
                    format!("{:<16} {:<11} {state}  \x1b[2m{}\x1b[22m", s(&h["name"]), s(&h["format"]), s(&h["url"]))
                })
                .collect();
            let human = if rows.is_empty() {
                "no webhooks yet: hc push add <name> --url <url>".into()
            } else {
                rows.join("\n")
            };
            Out { json: r, human }
        }
        PushCmd::Rm(p) => {
            let r = call("push.rm", to(&p))?;
            let human = format!("forgot webhook {}", s(&r["removed"]));
            Out { json: r, human }
        }
        PushCmd::Test(p) => {
            let r = call("push.test", to(&p))?;
            let status = r["status"].as_u64().map_or("no answer".into(), |c| format!("HTTP {c}"));
            let error = r["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default();
            let human = format!("{}: {status}, {}{error}", p.name, s(&r["outcome"]));
            Out { json: r, human }
        }
        PushCmd::Events(p) => {
            let r = call("push.events", to(&p))?;
            let human =
                r["events"].as_array().into_iter().flatten().map(|e| e.to_string()).collect::<Vec<_>>().join("\n");
            Out { json: r, human }
        }
        PushCmd::Key => {
            let r = call("push.key", json!({}))?;
            let human = s(&r["key"]);
            Out { json: r, human }
        }
    })
}
