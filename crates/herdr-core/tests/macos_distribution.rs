//! macOS release orchestration, without production identities or credentials.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/var/tmp/hc-macos-distribution-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self { root }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stub(s: &Scratch, name: &str, body: &str) {
    let path = s.root.join("tools").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' \"{name} $*\" >> \"$CALLS\"\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn script(s: &Scratch, name: &str) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg(format!("{ROOT}/scripts/{name}"))
        .arg(s.root.join("herdr-core"))
        .env("PATH", format!("{}:{}", s.root.join("tools").display(), std::env::var("PATH").unwrap()))
        .env("CALLS", s.root.join("calls"))
        .env_remove("APPLE_CERTIFICATE_P12")
        .env_remove("APPLE_CERTIFICATE_PASSWORD");
    cmd
}

#[test]
fn mac_sign_uses_runtime_and_entitlements_even_for_ad_hoc_builds() {
    let s = Scratch::new();
    stub(&s, "codesign", "exit 0");
    let out = script(&s, "mac-sign").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
    assert!(calls.contains("--options runtime"), "{calls}");
    assert!(calls.contains("--entitlements ") && calls.contains("macos-entitlements.plist"), "{calls}");
    assert!(calls.contains("--identifier dev.herdr.core"), "{calls}");
    assert!(calls.contains("--verify --strict"), "{calls}");
}

#[test]
fn notarization_requires_acceptance_before_gatekeeper_assessment() {
    for accepted in [true, false] {
        let s = Scratch::new();
        stub(&s, "codesign", "case $* in *-dvv*) echo 'Authority=Developer ID Application: Test' >&2 ;; esac");
        stub(&s, "ditto", "exit 0");
        stub(&s, "spctl", "exit 0");
        stub(
            &s,
            "xcrun",
            "case $2 in submit) printf '{\"id\":\"test-id\",\"status\":\"%s\"}\\n' \"$NOTARY_STATUS\" ;; log) for last; do :; done; echo '{}' > \"$last\" ;; esac",
        );
        let out = script(&s, "mac-notarize")
            .env("APPLE_API_KEY", "test-private-key")
            .env("APPLE_API_KEY_ID", "test-key-id")
            .env("APPLE_API_ISSUER", "test-issuer")
            .env("NOTARY_STATUS", if accepted { "Accepted" } else { "Invalid" })
            .output()
            .unwrap();
        assert_eq!(out.status.success(), accepted, "{out:?}");
        let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
        assert!(calls.contains("notarytool submit") && calls.contains("--wait"), "{calls}");
        assert_eq!(calls.contains("spctl --assess"), accepted, "{calls}");
        assert_eq!(calls.contains("notarytool log test-id"), !accepted, "{calls}");
    }
}
