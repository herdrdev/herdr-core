#![cfg(unix)]
mod common;

use common::*;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const REPORTER: &str = r#"
#!/bin/sh
set -eu

encode() { printf '%s' "$1" | base64 | tr -d '\r\n'; }
report() {
    body="pid=$$:time=$(date +%s):$1"
    mac=$(printf '%s:%s' "$body" "$OSC7577_TOKEN" | {
        if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi
    })
    printf '\033]7577;%s:mac=%s\033\\' "$body" "${mac%% *}"
}
arm() {
    name=fixture-resume
    if [ "$1" = missing-exe ]; then name=fixture-missing; fi
    report "argv=$(encode "$name"),$(encode resumed),$(encode "two words"),$(encode "a'\"b"),$(encode '$(echo unexpected)'),:cwd=$(encode "$RESUME_CWD")"
}

case "$1" in
    resumed)
        shift
        if [ -r /proc/$$/status ]; then
            while read -r key value; do
                if [ "$key" = SigIgn: ]; then printf '%s\n' "$value" > "$RESUME_PROOF.sigign"; break; fi
            done < /proc/$$/status
        fi
        printf '%s\0' "$(pwd -P)" "$OSC7577_TOKEN" "$@" > "$RESUME_PROOF"
        arm normal
        echo RESUME_REARMED
        read -r line || :
        ;;
    *)
        printf '%s' "$OSC7577_TOKEN" > "$RESUME_INITIAL"
        arm "$1"
        if [ "$1" = clear ]; then report clear=1; fi
        echo REPORT_WRITTEN
        if [ "$1" = replay-clear ]; then
            while [ ! -f "$RESUME_TRIGGER" ]; do sleep 0.03; done
            report clear=1
            touch "$RESUME_CLEARED"
        fi
        if [ "$1" = survive ]; then
            exec env -u HC_SCOPE sh -c 'trap "" HUP; touch "$RESUME_TRIGGER"; while [ ! -f "$RESUME_CLEARED" ]; do sleep 0.03; done' </dev/null >/dev/null 2>&1
        fi
        read -r line || :
        ;;
esac
"#;

struct Fixture {
    session: Session,
    reporter: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn new(shell: &Path) -> Self {
        let mut session = Session::new();
        let root = &session.root;
        let bin = root.join("bin");
        let cwd = root.join("saved-cwd");
        let elsewhere = root.join("profile-cwd");
        for dir in [&bin, &cwd, &elsewhere] {
            fs::create_dir(dir).unwrap();
        }
        let reporter = bin.join("fixture-resume");
        fs::write(&reporter, REPORTER.trim_start()).unwrap();
        fs::set_permissions(&reporter, fs::Permissions::from_mode(0o700)).unwrap();
        for (key, value) in [
            ("SHELL", shell.to_path_buf()),
            ("RESUME_CWD", cwd.clone()),
            ("RESUME_PROOF", root.join("resumed")),
            ("RESUME_INITIAL", root.join("initial")),
            ("RESUME_TRIGGER", root.join("trigger")),
            ("RESUME_CLEARED", root.join("cleared")),
            ("ENV", root.join("sh-profile")),
            ("ZDOTDIR", root.clone()),
            ("HC_FOREGROUND_MS", PathBuf::from("30")),
            ("OSC7577_TOKEN", PathBuf::from("inherited-token")),
        ] {
            session.env.push((key.into(), value.to_str().unwrap().into()));
        }

        let profile = format!("export PATH='{}':\"$PATH\"\ncd '{}'\n", bin.display(), elsewhere.display());
        for profile_name in [".bashrc", ".bash_profile", ".zshrc", ".profile", "sh-profile"] {
            fs::write(root.join(profile_name), &profile).unwrap();
        }
        let fish = root.join("config/fish");
        fs::create_dir(&fish).unwrap();
        fs::write(
            fish.join("config.fish"),
            format!("set -gx PATH '{}' $PATH\ncd '{}'\n", bin.display(), elsewhere.display()),
        )
        .unwrap();
        Self { session, reporter, cwd }
    }

    fn start(&self, mode: &str, direct: bool) -> String {
        let s = &self.session;
        let argv = if direct {
            vec![self.reporter.to_str().unwrap(), mode]
        } else {
            vec![
                "/bin/sh",
                "-c",
                "\"$@\"; echo CHILD_FINISHED; exec /bin/sh -i",
                "sh",
                self.reporter.to_str().unwrap(),
                mode,
            ]
        };
        let t = run(s, &[["--keep", "--cwd", s.root.to_str().unwrap(), "--"].as_slice(), &argv].concat());
        wait_until("report reached core", || text(s, &t).contains("REPORT_WRITTEN"));
        t
    }

    fn reg_record(&self, t: &str) -> Value {
        let registry: Value = serde_json::from_slice(&fs::read(self.state().join("terminals.json")).unwrap()).unwrap();
        registry["terminals"][herdr_core::terminals::parse_id(t).unwrap().to_string()].clone()
    }

    fn state(&self) -> PathBuf {
        self.session.root.join("state/herdr-core/test")
    }

    fn has_word(&self, t: &str) -> bool {
        self.reg_record(t)["resume"]["words"].as_object().unwrap().values().any(|w| w["record"].is_object())
    }

    fn holder_loss(&self, t: &str) {
        let s = &self.session;
        let pid = get(s, t)["pid"].as_i64().unwrap();
        let holder = s.json(&["server", "status"])["holder"]["pid"].as_i64().unwrap();
        assert_eq!(unsafe { libc::kill(holder as i32, libc::SIGKILL) }, 0);
        wait_until("replacement terminal process", || get(s, t)["pid"].as_i64().is_some_and(|p| p != pid));
    }

    fn verify_resume(&self, t: &str) {
        let s = &self.session;
        wait_until("resumed program rearmed", || text(s, t).contains("RESUME_REARMED"));
        if cfg!(target_os = "linux") {
            let ignored = fs::read_to_string(s.root.join("resumed.sigign")).unwrap();
            let ignored = u64::from_str_radix(ignored.trim(), 16).unwrap();
            assert_eq!(ignored & (1 << (libc::SIGPIPE - 1)), 0, "the resumed program ignores SIGPIPE");
        }
        let proof = fs::read(s.root.join("resumed")).unwrap();
        let parts: Vec<_> = proof.split(|b| *b == 0).collect();
        let physical_cwd = fs::canonicalize(&self.cwd).unwrap();
        assert_eq!(parts[0], physical_cwd.as_os_str().as_encoded_bytes());
        assert_eq!(parts[1].len(), 32);
        assert_ne!(parts[1], fs::read(s.root.join("initial")).unwrap());
        assert_eq!(&parts[2..], [b"two words".as_slice(), b"a'\"b", b"$(echo unexpected)", b"", b""]);
        assert_eq!(get(s, t)["cwd"], self.cwd.to_str().unwrap());
    }

    fn verify_shell(&self, t: &str) {
        let s = &self.session;
        let mut viewer = framed::Viewer::attach(s, t, 80, 24, serde_json::json!({}));
        s.json(&["send", t, "printf 'SHELL_%s_READY\\n' FIXTURE"]);
        viewer.shows("SHELL_FIXTURE_READY");
    }
}

fn can_resume() -> bool {
    unsafe { libc::geteuid() != 0 }
}

fn bash() -> &'static Path {
    Path::new("/bin/bash")
}

#[test]
fn a_shared_state_directory_disables_resume_without_changing_its_mode() {
    let f = Fixture::new(bash());
    let s = &f.session;
    let existing = can_resume().then(|| {
        let t = f.start("replay-clear", true);
        wait_until("saved command", || f.has_word(&t));
        t
    });
    let state = f.state();
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o750)).unwrap();
    s.json(&["server", if existing.is_some() { "restart" } else { "start" }]);
    if existing.is_some() {
        fs::write(s.root.join("trigger"), b"").unwrap();
        wait_until("the existing pane sent a report", || s.root.join("cleared").exists());
    }
    run(s, &["sh", "-c", "printf '%s\\n' \"${OSC7577_TOKEN-unset}\" > \"$HOME/token\""]);
    wait_until("probe records the token", || fs::read(s.root.join("token")).is_ok_and(|bytes| !bytes.is_empty()));
    assert_eq!(fs::read(s.root.join("token")).unwrap(), b"unset\n");
    assert_eq!(fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o750);
    if let Some(t) = existing {
        assert!(f.reg_record(&t)["resume"].is_null());
    }
    let log = fs::read_to_string(state.join("core.log")).unwrap();
    assert_eq!(log.matches("OSC 7577 resume disabled").count(), 1, "{log}");
}

#[test]
fn a_pane_discards_its_resume_record_when_state_becomes_shared() {
    if !can_resume() {
        return;
    }
    let f = Fixture::new(bash());
    let s = &f.session;
    let t = f.start("replay-clear", true);
    wait_until("saved command", || f.has_word(&t));
    let core = s.json(&["server", "status"])["pid"].clone();
    fs::set_permissions(f.state(), fs::Permissions::from_mode(0o750)).unwrap();
    fs::write(s.root.join("trigger"), b"").unwrap();
    wait_until("the core discarded the pane's resume state", || f.reg_record(&t)["resume"].is_null());
    fs::set_permissions(f.state(), fs::Permissions::from_mode(0o700)).unwrap();
    f.holder_loss(&t);
    f.verify_shell(&t);
    assert!(!s.root.join("resumed").exists());
    assert_eq!(s.json(&["server", "status"])["pid"], core);
}

#[cfg(target_os = "linux")]
#[test]
fn a_surviving_owner_is_not_started_a_second_time() {
    if !can_resume() {
        return;
    }
    let f = Fixture::new(bash());
    let s = &f.session;
    let t = f.start("survive", true);
    wait_until("owner can survive holder loss", || s.root.join("trigger").exists() && f.has_word(&t));
    let owner = get(s, &t)["pid"].as_i64().unwrap();
    f.holder_loss(&t);
    let survived = pid_alive(owner);
    let resumed = s.root.join("resumed").exists();
    fs::write(s.root.join("cleared"), b"").unwrap();
    wait_until("the original owner exits", || !pid_alive(owner));
    assert!(survived);
    assert!(!resumed);
    f.verify_shell(&t);
}

#[test]
fn a_first_process_resume_survives_shutdown_signals_only() {
    if !can_resume() {
        return;
    }
    std::thread::scope(|scope| {
        for signal in [libc::SIGKILL, libc::SIGTERM, libc::SIGHUP] {
            scope.spawn(move || {
                let f = Fixture::new(bash());
                let s = &f.session;
                let t = f.start("normal", true);
                wait_until("saved command", || f.has_word(&t));
                let pid = get(s, &t)["pid"].as_i64().unwrap();
                assert_eq!(unsafe { libc::kill(pid as i32, signal) }, 0);
                wait_until("replacement terminal process", || get(s, &t)["pid"].as_i64().is_some_and(|p| p != pid));
                if signal == libc::SIGKILL {
                    f.verify_shell(&t);
                    assert!(!s.root.join("resumed").exists());
                } else {
                    f.verify_resume(&t);
                }
            });
        }
    });
}

#[test]
fn shell_profiles_supply_path_and_the_helper_restores_cwd_and_exact_arguments() {
    if !can_resume() {
        return;
    }
    let path = std::env::var_os("PATH").expect("PATH");
    std::thread::scope(|scope| {
        for name in ["bash", "zsh", "sh", "dash", "fish"] {
            let Some(shell) = std::env::split_paths(&path).map(|dir| dir.join(name)).find(|exe| exe.is_file()) else {
                continue;
            };
            scope.spawn(move || {
                let f = Fixture::new(&shell);
                let t = f.start("normal", false);
                wait_until("saved command", || f.has_word(&t));
                f.holder_loss(&t);
                f.verify_resume(&t);
                f.session.json(&["keys", &t, "enter"]);
                f.verify_shell(&t);
            });
        }
    });
}

#[test]
fn restart_preserves_the_program_and_stop_restores_a_first_process() {
    if !can_resume() {
        return;
    }
    let f = Fixture::new(bash());
    let s = &f.session;
    let t = f.start("normal", true);
    wait_until("saved command", || f.has_word(&t));
    let initial = fs::read_to_string(s.root.join("initial")).unwrap();
    assert_ne!(initial, "inherited-token");
    let pid = get(s, &t)["pid"].clone();
    s.json(&["server", "restart"]);
    assert_eq!(get(s, &t)["pid"], pid);
    assert_eq!(f.reg_record(&t)["resume"]["token"], initial);
    let exec = s.hc(&["exec", "--", "sh", "-c", "printf '%s' \"${OSC7577_TOKEN-unset}\""]);
    assert!(exec.status.success());
    assert_eq!(exec.stdout, b"unset");
    s.json(&["server", "stop"]);
    f.verify_resume(&t);
    s.json(&["keys", &t, "enter"]);
    wait_until("direct program's ordinary exit ends the terminal", || get(s, &t)["state"] == "finished");
}

#[test]
fn explicit_clear_and_unseen_clear_on_adoption_remove_the_resume_command() {
    if !can_resume() {
        return;
    }
    for mode in ["clear", "replay-clear"] {
        let f = Fixture::new(bash());
        let s = &f.session;
        let t = f.start(mode, false);
        if mode == "replay-clear" {
            wait_until("saved command", || f.has_word(&t));
            let core = s.json(&["server", "status"])["pid"].as_i64().unwrap();
            assert_eq!(unsafe { libc::kill(core as i32, libc::SIGKILL) }, 0);
            wait_until("core gone", || !s.connect_ok());
            fs::write(s.root.join("trigger"), b"").unwrap();
            wait_until("clear written while core is down", || s.root.join("cleared").exists());
        }
        f.holder_loss(&t);
        f.verify_shell(&t);
        assert!(!s.root.join("resumed").exists());
    }
}

#[test]
fn owner_exit_grace_and_rapid_resume_loss() {
    if !can_resume() {
        return;
    }
    let f = Fixture::new(bash());
    let s = &f.session;
    let t = f.start("normal", false);
    wait_until("saved command", || f.has_word(&t));
    s.json(&["keys", &t, "enter"]);
    wait_until("owner exited", || text(s, &t).contains("CHILD_FINISHED"));
    f.holder_loss(&t);
    f.verify_resume(&t);
    wait_until("resumed program has reported again", || f.has_word(&t));
    fs::remove_file(s.root.join("resumed")).unwrap();
    f.holder_loss(&t);
    f.verify_shell(&t);
    assert!(!s.root.join("resumed").exists());
}

#[test]
fn long_gone_owner_missing_cwd_missing_executable_and_kill() {
    if !can_resume() {
        return;
    }
    std::thread::scope(|scope| {
        for mode in ["long-gone", "missing-cwd", "missing-exe", "kill"] {
            scope.spawn(move || {
                let f = Fixture::new(bash());
                let s = &f.session;
                let t = f.start(mode, false);
                wait_until("saved command", || f.has_word(&t));
                match mode {
                    "long-gone" => {
                        s.json(&["keys", &t, "enter"]);
                        wait_until("owner's grace expires", || !f.has_word(&t));
                    }
                    "missing-cwd" => fs::remove_dir(&f.cwd).unwrap(),
                    "kill" => {
                        assert_eq!(s.json(&["kill", &t])["reason"], "killed");
                        assert!(f.reg_record(&t)["resume"].is_null());
                        return;
                    }
                    _ => {}
                }
                f.holder_loss(&t);
                f.verify_shell(&t);
                assert!(!s.root.join("resumed").exists());
            });
        }
    });
}
