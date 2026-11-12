//! One bounded idle-sleep assertion for local terminals that report working.

use crate::runtime::{Core, State};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Window {
    started: Option<Duration>,
    limited: bool,
}

impl Window {
    fn remaining(&mut self, enabled: bool, working: usize, max: Duration, now: Duration) -> Option<Duration> {
        if !enabled || working == 0 {
            *self = Self::default();
            return None;
        }
        let started = *self.started.get_or_insert(now);
        let left = max.saturating_sub(now.saturating_sub(started));
        self.limited |= left.is_zero();
        (!self.limited).then_some(left)
    }
}

pub(crate) struct KeepAwake {
    window: Window,
    assertion: Option<crate::platform::IdleSleep>,
    stopped: bool,
    pub status: &'static str,
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self { window: Window::default(), assertion: None, stopped: false, status: "off" }
    }
}

impl KeepAwake {
    pub(crate) fn stop(&mut self) {
        self.assertion = None;
        self.stopped = true;
        self.status = "off";
    }
}

pub(crate) fn update(core: &Core, st: &mut State) -> Duration {
    let settings = core.settings();
    let enabled = settings.keep_awake == Some(crate::config::KeepAwake::On) && !st.stopping && !st.keep_awake.stopped;
    let working = st
        .reg
        .terminals
        .values()
        .filter(|t| t.is_live() && t.pid.is_some() && t.status.as_ref().is_some_and(|s| s.state == "working"))
        .count();
    let keep = &mut st.keep_awake;
    let now = crate::platform::keep_awake_clock();
    let max = settings.keep_awake_max.unwrap_or(Duration::from_secs(7200));
    let left = keep.window.remaining(enabled, working, max, now);
    let status = if !enabled {
        "off"
    } else if !crate::platform::keep_awake_supported() {
        "unsupported"
    } else if working == 0 {
        "idle"
    } else if let Some(left) = left {
        let result = match &mut keep.assertion {
            Some(assertion) => assertion.update(working, keep.window.started.unwrap().saturating_add(max), left),
            None => crate::platform::IdleSleep::new(working, now.saturating_add(left), left)
                .map(|assertion| keep.assertion = Some(assertion)),
        };
        match result {
            Ok(()) => "active",
            Err(error) => {
                if keep.status != "error" {
                    eprintln!("core: cannot prevent idle sleep: {error}");
                }
                "error"
            }
        }
    } else {
        "limited"
    };
    if status != "active" {
        keep.assertion = None;
    }
    keep.status = status;
    left.filter(|_| status == "active").unwrap_or(Duration::from_secs(1)).min(Duration::from_secs(1))
}

/// Status changes update synchronously. This task handles quiet expiration and config edits.
pub(crate) async fn watch(core: Arc<Core>) {
    loop {
        let wait = {
            let mut st = core.lock();
            update(&core, &mut st)
        };
        tokio::select! {
            _ = core.changed.notified() => {},
            _ = tokio::time::sleep(wait) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_work_period_keeps_its_limit_until_all_work_stops_or_the_setting_is_off() {
        let mut window = Window::default();
        let s = Duration::from_secs;
        assert_eq!(window.remaining(true, 1, s(10), s(1)), Some(s(10)));
        assert_eq!(window.remaining(true, 2, s(10), s(4)), Some(s(7)));
        assert_eq!(window.remaining(true, 1, s(10), s(11)), None);
        assert_eq!(window.remaining(true, 3, s(100), s(12)), None, "an expired period stays expired");
        assert_eq!(window.remaining(true, 0, s(10), s(13)), None);
        assert_eq!(window.remaining(true, 1, s(10), s(14)), Some(s(10)));
        assert_eq!(window.remaining(false, 1, s(10), s(15)), None);
        assert_eq!(window.remaining(true, 1, s(10), s(16)), Some(s(10)));
        assert_eq!(window.remaining(true, 1, s(0), s(16)), None, "zero never permits an assertion");
    }
}
