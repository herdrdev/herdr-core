//! Lost kept records stay live without a holder until a replacement shell starts.
use crate::holder_proto as proto;
use crate::runtime::{Core, now_ms};
use crate::terminals::{KEEP_RETRY_MS, Terminal};
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Terminal {
    pub(crate) fn schedule_restart(&mut self, now: u64) {
        let stable = self.program_started_ms.is_some_and(|at| now.saturating_sub(at) >= 10_000);
        self.restart_delay_ms = if stable {
            KEEP_RETRY_MS
        } else {
            match self.restart_delay_ms {
                0 => KEEP_RETRY_MS,
                KEEP_RETRY_MS => 3000,
                3000 => 6000,
                6000 => 12_000,
                12_000 => 30_000,
                _ => 60_000,
            }
        };
        self.restart_after_ms = now + self.restart_delay_ms;
        self.program_started_ms = None;
    }
}

impl Core {
    pub(crate) fn spawn_failed(&self, st: &mut crate::runtime::State, id: u32) {
        let t = st.reg.terminals.get_mut(&id).expect("failed spawn");
        t.holder = None;
        t.pid = None;
        if st.killing.contains(&id) || !t.keep || t.reason.as_deref().is_none_or(|r| r == "spawn_pending") {
            let reason = t.reason.clone().filter(|r| r != "spawn_pending").unwrap_or_else(|| "holder_lost".into());
            self.record_finish(st, id, proto::Exit { code: None, signal: None }, &reason);
        } else {
            t.schedule_restart(now_ms());
        }
        self.save(st);
        self.changed.notify_waiters();
    }

    pub(crate) async fn recover_kept(self: Arc<Self>, startup: bool) {
        let pending = self.lock().reg.terminals.values().any(|t| t.is_live() && t.keep && t.holder.is_none());
        if !pending || crate::platform::host_shutdown_in_progress() {
            return;
        }
        if self.sweeps.load(Ordering::SeqCst) != 0 || self.lock().stopping || self.restart.load(Ordering::SeqCst) {
            return;
        }
        if let Err(e) = self.ensure_holder().await {
            eprintln!("core: kept terminals waiting for a holder: {e}");
            return;
        }
        let shell = match crate::config::load(&self.paths.config).and_then(|settings| settings.shell()) {
            Ok(shell) => shell,
            Err(e) => {
                eprintln!("core: kept terminals waiting for a shell: {}", e.message);
                return;
            }
        };
        let base_env = crate::platform::program_env().await;
        let mut st = self.lock();
        if st.stopping
            || self.restart.load(Ordering::SeqCst)
            || self.sweeps.load(Ordering::SeqCst) != 0
            || crate::platform::host_shutdown_in_progress()
        {
            return;
        }
        let ids: Vec<u32> = st
            .reg
            .terminals
            .values()
            .filter(|t| t.is_live() && t.keep && t.holder.is_none() && (startup || t.restart_after_ms <= now_ms()))
            .map(|t| t.id())
            .collect();
        for id in ids {
            let Some(holder) = st.current() else { break };
            let instance = holder.instance.clone();
            let t = st.reg.terminals.get_mut(&id).expect("pending");
            let env = crate::api::child_env(base_env.clone(), t.restart_env.clone());
            let cwd = t
                .last_cwd
                .as_ref()
                .filter(|p| std::path::Path::new(p).is_dir())
                .or_else(|| std::path::Path::new(&t.cwd).is_dir().then_some(&t.cwd))
                .cloned()
                .unwrap_or_else(|| {
                    env.get("HOME").or_else(|| env.get("USERPROFILE")).cloned().unwrap_or_else(|| "/".into())
                });
            let (env, scope) = crate::api::terminal_env(&self, id, &cwd, env);
            t.scope = Some(scope);
            t.holder = Some(instance);
            t.argv = shell.clone();
            t.cwd = cwd.clone();
            t.template = None;
            t.reported = false;
            t.title.clear();
            t.full_screen = false;
            t.win32_input = false;
            t.printed = 0;
            t.screen = "exact".into();
            t.submit = None;
            t.stop = None;
            t.paste_gap_ms = None;
            let spawn = proto::Spawn { argv: t.argv.clone(), env, cwd, cols: t.cols, rows: t.rows };
            let mut screen = ghostty_vt::Terminal::new(t.cols, t.rows, self.history_bytes());
            screen.set_xtversion(&t.identity);
            st.screens.insert(id, screen);
            // Claim and persist before sending: a slow holder never gets a second spawn.
            self.save(&st);
            if let Some(link) = st.link(id) {
                link.send(&proto::json_frame(proto::SPAWN, id, &spawn));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::terminals::{NewTerminal, Registry};

    #[test]
    fn retries_back_off_forever_and_reset_only_after_a_stable_program() {
        let mut reg = Registry::default();
        let id = reg.create(NewTerminal { argv: vec!["sh".into()], keep: true, ..Default::default() }, 0).unwrap();
        let t = reg.terminals.get_mut(&id).unwrap();
        for delay in [1500, 3000, 6000, 12_000, 30_000, 60_000, 60_000] {
            t.schedule_restart(100_000);
            assert_eq!(t.restart_delay_ms, delay);
            assert_eq!(t.restart_after_ms, 100_000 + delay);
        }
        t.program_started_ms = Some(90_001);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 60_000, "a short-lived program does not reset retries");
        t.program_started_ms = Some(90_000);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 1500);
        t.schedule_restart(100_000);
        assert_eq!(t.restart_delay_ms, 3000);
        assert!(t.is_live());
    }
}
