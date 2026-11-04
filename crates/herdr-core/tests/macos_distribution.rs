//! macOS release orchestration, without production identities or credentials.
#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");

#[test]
fn calendar_and_reminder_descriptions_cover_macos_13_and_14() {
    let plist = include_str!("../../../distribution/macos-Info.plist");
    for service in ["Calendars", "Reminders"] {
        for access in ["", "FullAccess"] {
            assert!(plist.contains(&format!("<key>NS{service}{access}UsageDescription</key>")));
        }
    }
}

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

fn script(s: &Scratch, name: &str, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Command {
    let mut cmd = Command::new("bash");
    cmd.arg(format!("{ROOT}/scripts/{name}"))
        .args(args)
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
    std::fs::create_dir_all(s.root.join("herdr-core/Contents/MacOS")).unwrap();
    let out = script(&s, "mac-sign", [s.root.join("herdr-core")]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
    assert!(calls.contains("--options runtime"), "{calls}");
    assert!(calls.contains("--entitlements ") && calls.contains("macos-entitlements.plist"), "{calls}");
    assert!(calls.contains("--identifier dev.herdr.core"), "{calls}");
    assert!(calls.contains("--verify --strict"), "{calls}");
    let lines: Vec<_> = calls.lines().collect();
    assert!(lines[0].ends_with("/Contents/MacOS/herdr-core"), "nested executable first: {calls}");
    assert!(lines[1].ends_with("/herdr-core"), "then bundle: {calls}");
}

#[test]
fn release_archive_binaries_verify_independently() {
    let s = Scratch::new();
    let bin = s.root.join("herdr-core");
    let app = s.root.join("Herdr Core.app");
    std::fs::copy(env!("CARGO_BIN_EXE_herdr-core"), &bin).unwrap();
    if !cfg!(target_os = "macos") {
        // Model resource sealing: an executable copied out of a signed bundle
        // fails verification until signed again as a standalone target.
        stub(
            &s,
            "codesign",
            "for last; do :; done\ncase $1 in --force) if [ -d \"$last/Contents\" ]; then echo sealed > \"$last/Contents/MacOS/herdr-core\"; else echo standalone > \"$last\"; fi ;; --verify) if [ -f \"$last\" ]; then grep -q standalone \"$last\"; fi ;; esac",
        );
    }
    let archive = s.root.join("release.tar.gz");
    for (name, args) in [
        ("mac-bundle", vec![bin.clone(), app.clone()]),
        ("mac-sign", vec![app.clone()]),
        ("mac-sign", vec![bin]),
        ("package", vec![app, archive.clone()]),
    ] {
        let out = script(&s, name, args).output().unwrap();
        assert!(out.status.success(), "{name}: {out:?}");
    }
    let extracted = s.root.join("extracted");
    std::fs::create_dir(&extracted).unwrap();
    assert!(Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&extracted).status().unwrap().success());
    let standalone = s.root.join("bare-herdr-core");
    std::fs::rename(extracted.join("herdr-core"), &standalone).unwrap();
    for path in [extracted.join("Herdr Core.app"), standalone] {
        let out = Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(&path)
            .env("PATH", format!("{}:{}", s.root.join("tools").display(), std::env::var("PATH").unwrap()))
            .env("CALLS", s.root.join("calls"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}: {out:?}", path.display());
    }
}

#[test]
fn production_signing_covers_both_release_targets() {
    let workflow = std::fs::read_to_string(format!("{ROOT}/.github/workflows/release.yml")).unwrap();
    for target in ["Herdr Core.app", "herdr-core"] {
        for script in ["mac-sign", "mac-notarize"] {
            assert!(workflow.contains(&format!("scripts/{script} 'target/${{{{ matrix.target }}}}/release/{target}'")));
        }
    }
    for bundle in [false, true] {
        let s = Scratch::new();
        if bundle {
            std::fs::create_dir_all(s.root.join("herdr-core/Contents/MacOS")).unwrap();
        } else {
            std::fs::write(s.root.join("herdr-core"), "standalone").unwrap();
        }
        stub(&s, "codesign", "exit 0");
        stub(
            &s,
            "security",
            "case $1 in list-keychains) if [ $# = 3 ]; then echo '\"/test/login.keychain-db\"'; fi ;; find-identity) echo '1) ABCDEF \"Developer ID Application: Test\"' ;; esac",
        );
        let out = script(&s, "mac-sign", [s.root.join("herdr-core")])
            .env("APPLE_CERTIFICATE_P12", "dGVzdA==")
            .env("APPLE_CERTIFICATE_PASSWORD", "test-password")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
        let signs: Vec<_> = calls.lines().filter(|line| line.starts_with("codesign --force")).collect();
        assert_eq!(signs.len(), if bundle { 2 } else { 1 }, "{calls}");
        for sign in signs {
            for option in
                ["--timestamp", "--options runtime", "--entitlements", "--identifier dev.herdr.core", "--sign ABCDEF"]
            {
                assert!(sign.contains(option), "{sign}");
            }
        }
        assert!(calls.contains("codesign --verify --strict"), "{calls}");
        assert!(calls.contains("security delete-keychain"), "{calls}");
    }
}

#[test]
fn advertised_personal_information_has_runtime_entitlements() {
    let entitlements = std::fs::read_to_string(format!("{ROOT}/distribution/macos-entitlements.plist")).unwrap();
    for key in ["addressbook", "calendars", "photos-library"] {
        assert!(entitlements.contains(&format!("com.apple.security.personal-information.{key}</key><true/>")));
    }
}

#[test]
fn notarization_requires_acceptance_before_gatekeeper_assessment() {
    for (accepted, bundle) in [(true, true), (false, true), (true, false), (false, false)] {
        let s = Scratch::new();
        if bundle {
            std::fs::create_dir_all(s.root.join("herdr-core/Contents")).unwrap();
        } else {
            std::fs::write(s.root.join("herdr-core"), "standalone").unwrap();
        }
        stub(&s, "codesign", "case $* in *-dvv*) echo 'Authority=Developer ID Application: Test' >&2 ;; esac");
        stub(&s, "ditto", "exit 0");
        stub(&s, "spctl", "exit 0");
        stub(
            &s,
            "xcrun",
            "case $2 in submit) printf '{\"id\":\"test-id\",\"status\":\"%s\"}\\n' \"$NOTARY_STATUS\" ;; log) for last; do :; done; echo '{}' > \"$last\" ;; esac",
        );
        let out = script(&s, "mac-notarize", [s.root.join("herdr-core")])
            .env("APPLE_API_KEY", "test-private-key")
            .env("APPLE_API_KEY_ID", "test-key-id")
            .env("APPLE_API_ISSUER", "test-issuer")
            .env("NOTARY_STATUS", if accepted { "Accepted" } else { "Invalid" })
            .output()
            .unwrap();
        assert_eq!(out.status.success(), accepted, "{out:?}");
        let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
        assert!(calls.contains("notarytool submit") && calls.contains("--wait"), "{calls}");
        assert_eq!(calls.contains("spctl --assess --type execute"), accepted && bundle, "{calls}");
        assert_eq!(
            calls.contains("spctl --assess --type open --context context:primary-signature"),
            accepted && !bundle,
            "{calls}"
        );
        assert_eq!(calls.contains("stapler staple"), accepted && bundle, "{calls}");
        assert_eq!(calls.contains("stapler validate"), accepted && bundle, "{calls}");
        assert_eq!(calls.contains("notarytool log test-id"), !accepted, "{calls}");
    }
}

#[test]
fn gatekeeper_assessment_waits_for_a_fresh_ticket() {
    for (passes_on, success) in [(3, true), (99, false)] {
        let s = Scratch::new();
        std::fs::write(s.root.join("herdr-core"), "standalone").unwrap();
        stub(&s, "codesign", "case $* in *-dvv*) echo 'Authority=Developer ID Application: Test' >&2 ;; esac");
        stub(&s, "ditto", "exit 0");
        stub(&s, "sleep", "exit 0");
        stub(&s, "spctl", &format!("[ \"$(grep -c '^spctl' \"$CALLS\")\" -ge {passes_on} ]"));
        stub(&s, "xcrun", "printf '{\"id\":\"test-id\",\"status\":\"Accepted\"}\\n'");
        let out = script(&s, "mac-notarize", [s.root.join("herdr-core")])
            .env("APPLE_API_KEY", "test-private-key")
            .env("APPLE_API_KEY_ID", "test-key-id")
            .env("APPLE_API_ISSUER", "test-issuer")
            .output()
            .unwrap();
        assert_eq!(out.status.success(), success, "{out:?}");
        let calls = std::fs::read_to_string(s.root.join("calls")).unwrap();
        assert_eq!(calls.lines().filter(|l| l.starts_with("spctl")).count(), if success { 3 } else { 40 }, "{calls}");
    }
}
