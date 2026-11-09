//! `~/.config/herdr-core/config.toml`. A missing file means defaults.

use crate::rpc::RpcError;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Default)]
pub struct Config {
    /// Executable and arguments for new shells (default: this machine's shell).
    pub shell: Option<Vec<String>>,
    /// The chord that detaches `hc attach`: `ctrl+<key>`, default `ctrl+\`.
    pub detach_key: Option<String>,
    /// Seconds a `wait` runs before it checks in (default 180).
    pub wait_timeout: Option<f64>,
    /// Notes to parent agents and `<hc>` tags on messages between agents, on
    /// or off (default on). Off: hc types nothing on its own.
    pub notes: Option<bool>,
    /// Seconds of quiet before a note (default 3); after Enter or a clear it goes in at once.
    pub notes_quiet: Option<f64>,
    /// Size of a terminal nobody is typing in, `COLSxROWS` (default 200x120).
    pub headless: Option<String>,
    /// Scrollback kept per live terminal, in MB (default 5).
    pub history_mb: Option<f64>,
    /// Where the core listens (default under $XDG_RUNTIME_DIR). `HC_SOCKET` wins.
    pub socket: Option<String>,
    /// Seconds another machine gets to answer a call (a wait gets its own
    /// timeout on top), and to send anything at all before its line is
    /// given up and dialed again (default 30).
    pub machine_timeout: Option<f64>,
    /// How the machine's own shell starts: "login" makes it a login shell (it
    /// reads the profile files too), "non_login" keeps it from being one,
    /// "auto" (the default) leaves it to the platform.
    pub shell_mode: Option<String>,
}

/// How the machine's own shell starts.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ShellMode {
    /// The platform's way: on macOS the own shell is a login shell.
    Auto,
    /// A login shell: it reads the profile files too.
    Login,
    /// Not a login shell.
    NonLogin,
}

impl Config {
    pub fn shell(&self) -> Result<Vec<String>, RpcError> {
        match &self.shell {
            None => Ok(crate::platform::own_shell(self.login_shell()?)),
            Some(argv) if argv.first().is_some_and(|exe| !exe.is_empty()) => Ok(argv.clone()),
            Some(_) => Err(invalid("shell: give an executable and optional arguments, like [\"nu\"]".into())),
        }
    }

    /// The setting, as it is written.
    pub fn shell_mode(&self) -> Result<ShellMode, RpcError> {
        match self.shell_mode.as_deref() {
            None | Some("auto") => Ok(ShellMode::Auto),
            Some("login") => Ok(ShellMode::Login),
            Some("non_login") => Ok(ShellMode::NonLogin),
            Some(other) => Err(invalid(format!("shell_mode {other:?}: use \"auto\", \"login\" or \"non_login\""))),
        }
    }

    /// Whether the machine's own shell is a login shell. "auto" leaves it to
    /// the platform.
    pub fn login_shell(&self) -> Result<bool, RpcError> {
        Ok(match self.shell_mode()? {
            ShellMode::Auto => crate::platform::login_shell_by_default(),
            ShellMode::Login => true,
            ShellMode::NonLogin => false,
        })
    }

    /// Whether a shell this core starts is a login shell because the setting
    /// says so. "auto" leaves it to the platform.
    pub fn login_by_setting(&self) -> Result<bool, RpcError> {
        Ok(self.shell_mode()? == ShellMode::Login)
    }

    pub fn headless(&self) -> Result<(u16, u16), RpcError> {
        let Some(spec) = &self.headless else { return Ok((200, 120)) };
        spec.split_once('x')
            .and_then(|(c, r)| Some((c.trim().parse().ok()?, r.trim().parse().ok()?)))
            .filter(|(c, r): &(u16, u16)| *c > 0 && *r > 0)
            .ok_or_else(|| invalid(format!("headless {spec:?}: use COLSxROWS, like \"200x120\"")))
    }

    pub fn machine_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.machine_timeout.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(30.0))
    }

    pub fn history_bytes(&self) -> usize {
        (self.history_mb.unwrap_or(5.0).max(0.0) * 1024.0 * 1024.0) as usize
    }
}

fn invalid(message: String) -> RpcError {
    RpcError::new("config", message, "fix ~/.config/herdr-core/config.toml")
}

pub fn load(path: &Path) -> Result<Config, RpcError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| invalid(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(invalid(format!("{}: {e}", path.display()))),
    }
}

/// A detach chord: the control byte it sends, and its key code for the
/// kitty and modifyOtherKeys encodings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetachKey {
    pub byte: u8,
    pub code: u32,
}

impl Config {
    pub fn detach_key(&self) -> Result<DetachKey, RpcError> {
        let spec = self.detach_key.as_deref().unwrap_or("ctrl+\\");
        let key = spec.strip_prefix("ctrl+").and_then(|k| {
            let mut chars = k.chars();
            let c = chars.next()?.to_ascii_lowercase();
            (chars.next().is_none() && (c.is_ascii_lowercase() || "@[\\]^_".contains(c))).then_some(c)
        });
        let c = key.ok_or_else(|| {
            invalid(format!("detach_key {spec:?}: use a ctrl chord like \"ctrl+\\\\\" or \"ctrl+]\""))
        })?;
        Ok(DetachKey { byte: c as u8 & 0x1f, code: c as u32 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &str) -> Result<DetachKey, RpcError> {
        Config { detach_key: Some(spec.into()), ..Default::default() }.detach_key()
    }

    #[test]
    fn shell_mode_says_whether_the_machine_shell_is_a_login_one() {
        let platform = crate::platform::login_shell_by_default();
        assert_eq!(Config::default().shell_mode().unwrap(), ShellMode::Auto);
        assert_eq!(Config::default().login_shell().unwrap(), platform);
        assert_eq!(Config { shell_mode: Some("auto".into()), ..Default::default() }.login_shell().unwrap(), platform);
        assert!(Config { shell_mode: Some("login".into()), ..Default::default() }.login_shell().unwrap());
        assert!(!Config { shell_mode: Some("non_login".into()), ..Default::default() }.login_shell().unwrap());
        // "auto" leaves it to the platform; "login" asks for it.
        assert!(!Config::default().login_by_setting().unwrap());
        assert!(Config { shell_mode: Some("login".into()), ..Default::default() }.login_by_setting().unwrap());
        assert!(!Config { shell_mode: Some("non_login".into()), ..Default::default() }.login_by_setting().unwrap());
        assert_eq!(
            Config { shell_mode: Some("yes".into()), ..Default::default() }.shell_mode().unwrap_err().kind,
            "config"
        );
    }

    #[test]
    fn detach_keys_are_ctrl_chords_only() {
        assert_eq!(Config::default().detach_key().unwrap(), DetachKey { byte: 0x1c, code: 92 });
        assert_eq!(key("ctrl+]").unwrap(), DetachKey { byte: 0x1d, code: 93 });
        assert_eq!(key("ctrl+A").unwrap(), DetachKey { byte: 0x01, code: 97 });
        for bad in ["alt+x", "ctrl+", "ctrl+ab", "ctrl+1", "x"] {
            assert_eq!(key(bad).unwrap_err().kind, "config", "{bad}");
        }
    }
}
