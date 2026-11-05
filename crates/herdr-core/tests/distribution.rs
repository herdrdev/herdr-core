//! Releases: the install script, `hc update` from the release list, and hc
//! on a machine of another OS or CPU, all against a fake releases site
//! (python3's http.server) built from this very binary.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
/// This build's version: what scripts/manifest writes, what hc says it is.
const THIS: &str = env!("CARGO_PKG_VERSION");
/// A release newer than this build (it is this build inside, dressed up).
const NEWER: &str = "9.9.9";

#[cfg(target_os = "macos")]
fn live_executable(pid: i64) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; 4096];
    let len = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    assert!(len > 0);
    let len = buf.iter().position(|b| *b == 0).unwrap();
    PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len]))
}

/// A releases site with this build as `version`, for `targets`.
struct Releases {
    root: PathBuf,
    url: String,
    server: Child,
}

impl Releases {
    fn new(s: &Session, version: &str, targets: &[&str]) -> Releases {
        let root = s.root.join("releases");
        std::fs::create_dir_all(&root).unwrap();
        let ready = root.join("server-port");
        let server = Command::new("python3")
            .arg(format!("{ROOT}/tests/releases_server.py"))
            .arg(&ready)
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let mut r = Releases { root, url: String::new(), server };
        let mut port = None;
        wait_until("the releases site", || {
            assert!(r.server.try_wait().unwrap().is_none(), "the releases site exited before becoming ready");
            port = std::fs::read_to_string(&ready).ok().and_then(|text| text.parse::<u16>().ok());
            port.is_some()
        });
        r.url = format!("http://127.0.0.1:{}", port.unwrap());
        let dir = r.root.join("core").join(version);
        std::fs::create_dir_all(&dir).unwrap();
        let bin_dir = Path::new(BIN).parent().unwrap();
        for t in targets {
            let tar = dir.join(format!("herdr-core-{t}.tar.gz"));
            if cfg!(target_os = "macos") && t.starts_with("macos-") {
                let app = s.root.join("Herdr Core.app");
                let standalone = s.root.join("herdr-core");
                std::fs::copy(BIN, &standalone).unwrap();
                for (script, args) in [
                    ("mac-bundle", vec![PathBuf::from(BIN), app.clone()]),
                    ("mac-sign", vec![app.clone()]),
                    ("mac-sign", vec![standalone]),
                    ("package", vec![app, tar]),
                ] {
                    let out = Command::new("bash")
                        .arg(format!("{ROOT}/../../scripts/{script}"))
                        .args(args)
                        .env_remove("APPLE_CERTIFICATE_P12")
                        .env_remove("APPLE_CERTIFICATE_PASSWORD")
                        .output()
                        .unwrap();
                    assert!(out.status.success(), "{script}: {out:?}");
                }
                continue;
            }
            assert!(
                Command::new("tar")
                    .arg("-czf")
                    .arg(&tar)
                    .arg("-C")
                    .arg(bin_dir)
                    .arg("herdr-core")
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let out = Command::new(format!("{ROOT}/../../scripts/manifest"))
            .arg(&dir)
            .env("RELEASES_URL", &r.url)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        // Dressed up as `version`: its field and its links.
        let manifest = String::from_utf8(out.stdout)
            .unwrap()
            .replace(&format!("\"version\": \"{THIS}\""), &format!("\"version\": \"{version}\""))
            .replace(&format!("/core/{THIS}/"), &format!("/core/{version}/"));
        std::fs::write(dir.join("manifest.json"), &manifest).unwrap();
        std::fs::write(r.root.join("core/latest.json"), &manifest).unwrap();
        r
    }
}

impl Drop for Releases {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn here() -> String {
    let arch = if std::env::consts::ARCH == "aarch64" { "aarch64" } else { "x86_64" };
    format!("{}-{arch}", std::env::consts::OS)
}

#[test]
fn the_install_script_installs_and_then_updates_without_stopping_anything() {
    let mut s = Session::new();
    let r = Releases::new(&s, THIS, &[&here()]);
    let bin = s.root.join("bin");
    let install = |s: &Session| {
        // A PATH without the hc this machine already has: else no `hc` link is added.
        let mut cmd = s.program("sh");
        cmd.arg(format!("{ROOT}/../../distribution/install.sh")).env("PATH", "/usr/bin:/bin");
        let out = cmd.env("HERDR_RELEASES_URL", &r.url).env("HC_INSTALL_DIR", &bin).output().unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(install(&s).contains("installed"));
    assert!(bin.join("herdr-core").is_file() && bin.join("hc").is_symlink());
    if cfg!(target_os = "macos") {
        let bundle = s.root.join(".local/share/herdr-core/active/Herdr Core.app");
        assert!(bundle.join("Contents/Info.plist").is_file());
        assert_eq!(
            bin.join("herdr-core").canonicalize().unwrap(),
            bundle.join("Contents/MacOS/herdr-core").canonicalize().unwrap()
        );
    }

    s.bin = bin.join("hc");
    if cfg!(target_os = "macos") {
        let (_, err) = s.err(&["update", "--from", BIN]);
        assert_eq!(err["error"]["kind"], "refused");
        assert!(err["error"]["hint"].as_str().unwrap().contains("Herdr Core.app"));
        let source = s.root.join("Herdr Core.app");
        assert_eq!(s.json(&["update", "--from", source.to_str().unwrap()])["now"], format!("hc {THIS}"));
    }
    let t = run(&s, &["sh", "-c", "while read l; do echo got-$l; done"]);
    use std::os::unix::fs::MetadataExt;
    let holder_inode = std::fs::metadata(&s.bin).unwrap().ino();
    let holder_path = s.bin.canonicalize().unwrap();
    let status = s.json(&["server", "status"]);
    let pid = status["pid"].clone();
    assert!(install(&s).contains("updated"), "again: an update");
    assert_ne!(s.json(&["server", "status"])["pid"], pid, "the planned restart");
    s.json(&["send", &t, "still"]);
    wait_until("the terminal kept running", || text(&s, &t).contains("got-still"));
    if cfg!(target_os = "macos") {
        assert_eq!(s.bin.canonicalize().unwrap(), holder_path, "the newest bundle has one fixed physical path");
        assert_ne!(std::fs::metadata(&holder_path).unwrap().ino(), holder_inode, "the fixed path has the new image");
        let previous = s.root.join(".local/share/herdr-core/versions");
        let retained: Vec<_> = std::fs::read_dir(&previous)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|slot| {
                std::fs::metadata(slot.join("Herdr Core.app/Contents/MacOS/herdr-core"))
                    .is_ok_and(|m| m.ino() == holder_inode)
            })
            .collect();
        assert_eq!(retained.len(), 1, "the original holder's bundle is still on disk");
        #[cfg(target_os = "macos")]
        {
            let resolved = live_executable(status["holder"]["pid"].as_i64().unwrap());
            assert!(resolved.starts_with(retained[0].canonicalize().unwrap()));
            assert_eq!(std::fs::metadata(&resolved).unwrap().ino(), holder_inode);
        }
        let second = run(&s, &["sh", "-c", "while read l; do echo second-$l; done"]);
        let second_inode = std::fs::metadata(&s.bin).unwrap().ino();
        assert!(install(&s).contains("updated"));
        assert_eq!(s.bin.canonicalize().unwrap(), holder_path);
        assert_ne!(std::fs::metadata(&s.bin).unwrap().ino(), second_inode);
        // Further installs must retain both old holders, not just the last version.
        assert!(retained.iter().all(|slot| slot.exists()));
        for (id, expected) in [(&t, "got-third"), (&second, "second-third")] {
            s.json(&["send", id, "third"]);
            wait_until("old shells survive repeated updates", || text(&s, id).contains(expected));
        }
        let archived = retained[0].join("Herdr Core.app/Contents/MacOS/herdr-core");
        let out = s.program(archived.to_str().unwrap()).args(["server", "restart"]).output().unwrap();
        assert!(out.status.success(), "a retained executable can restart the newest core: {out:?}");
        #[cfg(target_os = "macos")]
        assert_eq!(live_executable(s.json(&["server", "status"])["pid"].as_i64().unwrap()), holder_path);
        let newest = run(&s, &["sh", "-c", "echo newest"]);
        wait_until("new terminals use the newest core", || text(&s, &newest).contains("newest"));
    }
    drop(r);
}

#[test]
fn a_macos_system_install_uses_the_standalone_release() {
    use std::os::unix::fs::PermissionsExt;
    let s = Session::new();
    let tools = s.root.join("sudo-tools");
    std::fs::create_dir(&tools).unwrap();
    let r = Releases::new(&s, THIS, &["macos-x86_64"]);
    for (name, script) in [("uname", "if [ \"$1\" = -s ]; then echo Darwin; else echo x86_64; fi"), ("id", "echo 0")] {
        let path = tools.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = s
        .program("sh")
        .arg(format!("{ROOT}/../../distribution/install.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .env("HERDR_RELEASES_URL", &r.url)
        .env("HC_INSTALL_DIR", s.root.join("system-bin"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(s.root.join("system-bin/herdr-core").is_file());
    assert!(!s.root.join("system-bin/herdr-core").is_symlink());
    assert!(!s.root.join(".local/share/herdr-core/current").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn bundle_completion_skips_releases_without_a_bundle() {
    let s = Session::new();
    let r = Releases::new(&s, THIS, &[&here()]);
    let bin = s.root.join("not-installed");
    let out = s
        .program("sh")
        .arg(format!("{ROOT}/../../distribution/install.sh"))
        .env("HERDR_RELEASES_URL", &r.url)
        .env("HC_INSTALL_DIR", &bin)
        .env("HC_BUNDLE_ONLY", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(!bin.exists(), "no binary swap or restart for a pre-bundle release");
}

#[cfg(target_os = "macos")]
#[test]
fn machine_installs_keep_the_complete_bundle_and_both_aliases() {
    for cross_arch in [false, true] {
        use std::os::unix::fs::PermissionsExt;
        let mut remote = Session::new();
        remote.bin = remote.root.join(".local/bin/herdr-core");
        let mut local = Session::new();
        let target = if cross_arch { "macos-x86_64" } else { &here() };
        let r = Releases::new(&local, THIS, &[target]);
        let source = local.root.join("Herdr Core.app/Contents/MacOS/herdr-core");
        let installed = local.root.join(".local/bin/herdr-core");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        let out = local
            .program(&source)
            .arg("--hc-install-bundle")
            .arg(&source)
            .arg(&installed)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(out.status.success(), "local bundle install: {out:?}");
        local.bin = installed.with_file_name("hc");
        assert!(local.bin.is_symlink());
        local.env.push(("HERDR_RELEASES_URL".into(), r.url.clone()));
        local.env.push(fake_ssh(&local, &[("bare", &remote)]));
        if cross_arch {
            // Force the released-build branch regardless of the test host CPU.
            let fake = remote.root.join("fakebin");
            std::fs::create_dir(&fake).unwrap();
            std::fs::write(fake.join("uname"), "#!/bin/sh\necho Darwin x86_64\n").unwrap();
            std::fs::set_permissions(fake.join("uname"), std::fs::Permissions::from_mode(0o755)).unwrap();
            // On Intel hosts use the other target instead.
            if std::env::consts::ARCH == "x86_64" {
                continue;
            }
        }
        let added = local.json(&["machine", "add", "bare", "--install"]);
        assert_eq!(added["name"], "bare");
        let current = remote.root.join(".local/share/herdr-core/current");
        assert!(current.join("Contents/Info.plist").is_file());
        let hc = remote.bin.with_file_name("hc");
        assert_eq!(hc.canonicalize().unwrap(), remote.bin.canonicalize().unwrap());
        local.json(&["machine", "update", "bare"]);
        assert_eq!(hc.canonicalize().unwrap(), remote.bin.canonicalize().unwrap());
        if !cross_arch {
            let bundled = remote.bin.canonicalize().unwrap();
            let before = std::fs::read(&bundled).unwrap();
            local.bin = local.root.join("herdr-core");
            let (_, error) = local.err(&["--json", "machine", "update", "bare"]);
            assert_eq!(error["error"]["kind"], "install_failed");
            assert!(error["error"]["message"].as_str().unwrap().contains("complete bundle"), "{error}");
            assert_eq!(remote.bin.canonicalize().unwrap(), bundled);
            assert_eq!(hc.canonicalize().unwrap(), bundled);
            assert_eq!(std::fs::read(&bundled).unwrap(), before);
        }
        local.json(&["machine", "rm", "bare"]);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_root_remote_gets_the_independently_signed_standalone_release() {
    use std::os::unix::fs::PermissionsExt;
    let mut remote = Session::new();
    remote.bin = remote.root.join(".local/bin/herdr-core");
    let tools = remote.root.join("fakebin");
    std::fs::create_dir(&tools).unwrap();
    std::fs::write(tools.join("id"), "#!/bin/sh\necho 0\n").unwrap();
    std::fs::set_permissions(tools.join("id"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut local = Session::new();
    let r = Releases::new(&local, THIS, &[&here()]);
    let source = local.root.join("Herdr Core.app/Contents/MacOS/herdr-core");
    local.bin = local.root.join(".local/bin/hc");
    std::fs::create_dir_all(local.bin.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&source, &local.bin).unwrap();
    local.env.push(("HERDR_RELEASES_URL".into(), r.url.clone()));
    local.env.push(fake_ssh(&local, &[("root", &remote)]));
    local.json(&["machine", "add", "root", "--install"]);
    for update in [false, true] {
        if update {
            local.json(&["machine", "update", "root"]);
        }
        assert!(!remote.bin.is_symlink());
        assert!(!remote.root.join(".local/share/herdr-core/current").exists());
        assert_eq!(remote.bin.with_file_name("hc").canonicalize().unwrap(), remote.bin.canonicalize().unwrap());
        let out = Command::new("/usr/bin/codesign").args(["--verify", "--strict"]).arg(&remote.bin).output().unwrap();
        assert!(out.status.success(), "standalone signature: {out:?}");
    }
    local.json(&["machine", "rm", "root"]);
}

#[cfg(target_os = "macos")]
#[test]
fn concurrent_bundle_installers_and_a_self_update_share_one_active_path() {
    let s = Session::new();
    let _r = Releases::new(&s, THIS, &[&here()]);
    let bin = s.root.join("bin/herdr-core");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    let source = s.root.join("Herdr Core.app/Contents/MacOS/herdr-core");
    let spawn =
        || s.program(source.to_str().unwrap()).arg("--hc-install-bundle").arg(&source).arg(&bin).spawn().unwrap();
    let mut first = spawn();
    let mut second = spawn();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    let active =
        s.root.join(".local/share/herdr-core/active/Herdr Core.app/Contents/MacOS/herdr-core").canonicalize().unwrap();
    assert_eq!(bin.canonicalize().unwrap(), active);
    // The source may be the same active bundle being replaced. Its copied
    // candidate must be complete before exchanging the live directories.
    assert!(
        s.program(bin.to_str().unwrap())
            .arg("--hc-install-bundle")
            .arg(&active)
            .arg(&active)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(bin.canonicalize().unwrap(), active);
    assert!(s.program(bin.to_str().unwrap()).arg("--version").status().unwrap().success());
}

#[cfg(target_os = "macos")]
#[test]
fn a_bundle_update_preserves_read_only_aliases_to_the_active_executable() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let s = Session::new();
    let _r = Releases::new(&s, THIS, &[&here()]);
    let bin = s.root.join("bin/herdr-core");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    let source = s.root.join("Herdr Core.app/Contents/MacOS/herdr-core");
    let path_bin = s.root.join("path-bin");
    std::fs::create_dir(&path_bin).unwrap();
    let install = |dest: &Path| {
        s.program(source.to_str().unwrap())
            .arg("--hc-install-bundle")
            .arg(&source)
            .arg(dest)
            .env("HC_INSTALL_DIR", bin.parent().unwrap())
            .env("PATH", format!("{}:/usr/bin:/bin", path_bin.display()))
            .output()
            .unwrap()
    };
    assert!(install(&bin).status.success());
    let active = bin.canonicalize().unwrap();
    std::fs::set_permissions(bin.parent().unwrap(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = install(&active);
    std::fs::set_permissions(bin.parent().unwrap(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(out.status.success(), "already-correct links need no writes: {out:?}");
    let alias = bin.with_file_name("hc");
    std::fs::remove_file(&alias).unwrap();
    symlink(&active, &alias).unwrap();
    let path_alias = path_bin.join("hc");
    symlink(&alias, &path_alias).unwrap();
    std::fs::set_permissions(bin.parent().unwrap(), std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::set_permissions(&path_bin, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = install(&active);
    std::fs::set_permissions(bin.parent().unwrap(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&path_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(out.status.success(), "correct direct and chained PATH aliases need no writes: {out:?}");
    assert_eq!(std::fs::read_link(&alias).unwrap(), active);
    assert_eq!(std::fs::read_link(&path_alias).unwrap(), alias);
    assert_eq!(path_alias.canonicalize().unwrap(), active);
}

#[cfg(target_os = "macos")]
#[test]
fn a_bundle_installer_never_replaces_a_host_application() {
    let s = Session::new();
    let _r = Releases::new(&s, THIS, &[&here()]);
    let host = s.root.join("Host.app/Contents/MacOS/herdr-core");
    std::fs::create_dir_all(host.parent().unwrap()).unwrap();
    std::fs::copy(BIN, &host).unwrap();
    let source = s.root.join("Herdr Core.app/Contents/MacOS/herdr-core");
    let before = std::fs::read(&host).unwrap();
    let out = s.program(source.to_str().unwrap()).arg("--hc-install-bundle").arg(&source).arg(&host).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("host application"));
    assert_eq!(std::fs::read(host).unwrap(), before);
}

#[cfg(target_os = "macos")]
#[test]
fn a_bundle_install_preserves_an_unrelated_hc_symlink() {
    let mut s = Session::new();
    let r = Releases::new(&s, THIS, &[&here()]);
    let bin = s.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("/usr/bin/true", bin.join("hc")).unwrap();
    let out = s
        .program("sh")
        .arg(format!("{ROOT}/../../distribution/install.sh"))
        .env("HERDR_RELEASES_URL", &r.url)
        .env("HC_INSTALL_DIR", &bin)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    s.bin = bin.join("herdr-core");
    assert_eq!(std::fs::read_link(bin.join("hc")).unwrap(), Path::new("/usr/bin/true"));
}

#[cfg(target_os = "macos")]
#[test]
fn a_bare_development_core_never_downloads_or_migrates_on_startup() {
    let mut s = Session::new();
    let bin = s.root.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(BIN, bin.join("herdr-core")).unwrap();
    s.bin = bin.join("herdr-core");
    s.env.push(("HERDR_RELEASES_URL".into(), format!("file://{}/missing", s.root.display())));
    let pid = s.json(&["server", "status"])["pid"].clone();
    let t = run(&s, &["sh", "-c", "echo no-download"]);
    wait_until("a working bare core", || text(&s, &t).contains("no-download"));
    assert_eq!(s.json(&["server", "status"])["pid"], pid);
    assert!(!s.bin.is_symlink());
    assert!(!s.root.join(".local/share/herdr-core/Herdr Core.app").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn an_explicit_bundle_upgrade_restarts_before_its_first_new_holder() {
    let mut s = Session::new();
    let r = Releases::new(&s, THIS, &[&here()]);
    let bin = s.root.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(BIN, bin.join("herdr-core")).unwrap();
    s.bin = bin.join("herdr-core");
    s.env.push(("HERDR_RELEASES_URL".into(), r.url.clone()));
    let pid = s.json(&["server", "status"])["pid"].clone();
    s.json(&["update", "--from", s.root.join("Herdr Core.app").to_str().unwrap()]);
    assert!(s.bin.is_symlink());
    assert_ne!(s.json(&["server", "status"])["pid"], pid);
    // No holder existed before migration: its first spawn must match the new
    // core, not the now-replaced bare executable's inode-based build identity.
    let t = run(&s, &["sh", "-c", "while read l; do echo got-$l; done"]);
    assert!(s.root.join(".local/share/herdr-core/Herdr Core.app/Contents/Info.plist").is_file());
    s.json(&["server", "restart"]);
    s.json(&["send", &t, "migrated"]);
    wait_until("terminal survives migration and later restart", || text(&s, &t).contains("got-migrated"));
}

/// The URL the install script downloads hc from, for a list giving `url`,
/// with curl stubbed to record it (the download itself fails).
fn install_script_downloads(s: &Session, url: &str, releases: &str, installed: bool) -> String {
    let dir = s.root.join(format!(
        "marker-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let (stub, bin) = (dir.join("stub"), dir.join("bin"));
    std::fs::create_dir_all(&stub).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    if installed {
        std::fs::write(bin.join("herdr-core"), "#!/bin/sh\n").unwrap();
        Command::new("chmod").arg("755").arg(bin.join("herdr-core")).status().unwrap();
    }
    let list = dir.join("latest.json");
    std::fs::write(
        &list,
        format!(r#"{{"version":"1.0.0","generation":1,"targets":{{"{}":{{"url":"{url}","sha256":"x"}}}}}}"#, here()),
    )
    .unwrap();
    let log = dir.join("urls");
    std::fs::write(
        stub.join("curl"),
        format!(
            "#!/bin/sh\nout=; url=; prev=\nfor a; do\n  if [ \"$prev\" = -o ]; then out=$a; else case $a in -*) ;; *) url=$a ;; esac; fi\n  prev=$a\ndone\necho \"$url\" >> {log}\ncase $url in */latest.json) cp {list} \"$out\"; exit 0 ;; esac\nexit 22\n",
            log = log.display(),
            list = list.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("755").arg(stub.join("curl")).status().unwrap();
    let out = Command::new("sh")
        .arg(format!("{ROOT}/../../distribution/install.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
        .env("HERDR_RELEASES_URL", releases)
        .env_remove("HC_VERSION")
        .env("HC_INSTALL_DIR", &bin)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("download failed"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(log).unwrap().lines().last().unwrap().to_owned()
}

/// The download counter on releases.herdr.dev: plain release URLs say install
/// or update; a mirror's query (a signed URL) or fragment, or another host,
/// is fetched exactly as the list gives it.
#[test]
fn the_install_script_marks_only_plain_release_downloads() {
    let s = Session::new();
    let r = "https://releases.example";
    let tarball = format!("{r}/core/1.0.0/herdr-core-{}.tar.gz", here());
    assert_eq!(install_script_downloads(&s, &tarball, r, false), format!("{tarball}?src=install"));
    assert_eq!(install_script_downloads(&s, &tarball, r, true), format!("{tarball}?src=update"));
    for url in [
        format!("{tarball}?sig=a%2Fb&exp=1"),
        format!("{tarball}?src=install"),
        format!("{tarball}#x"),
        "https://mirror.example/core/1.0.0/hc.tar.gz".to_owned(),
        "https://releases.example.evil/core/1.0.0/hc.tar.gz".to_owned(),
    ] {
        assert_eq!(install_script_downloads(&s, &url, r, false), url);
    }
}

#[test]
fn hc_update_downloads_checks_and_swaps() {
    let mut s = Session::new();
    let r = Releases::new(&s, NEWER, &[&here()]);
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("herdr-core")).unwrap();
    s.bin = dir.join("herdr-core");
    s.env.push(("HERDR_RELEASES_URL".into(), r.url.clone()));
    let pid = s.json(&["server", "status"])["pid"].clone();

    let up = s.json(&["update"]);
    assert_eq!((up["restarted"].as_bool(), up["release"].as_str()), (Some(true), Some(NEWER)), "{up}");
    assert_ne!(s.json(&["server", "status"])["pid"], pid);

    let tar = r.root.join(format!("core/{NEWER}/herdr-core-{}.tar.gz", here()));
    std::fs::write(&tar, b"not what the list says").unwrap();
    let (_, e) = s.err(&["--json", "update"]);
    assert_eq!(e["error"]["kind"], "refused", "a damaged download changes nothing: {e}");
    drop(r);
}

#[test]
fn hc_update_to_a_version_takes_at_least_that_one() {
    let mut s = Session::new();
    let r = Releases::new(&s, NEWER, &[&here()]);
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("herdr-core")).unwrap();
    s.bin = dir.join("herdr-core");
    s.env.push(("HERDR_RELEASES_URL".into(), r.url.clone()));
    s.json(&["server", "status"]);

    let up = s.json(&["update", "--version", NEWER]);
    assert_eq!((up["restarted"].as_bool(), up["release"].as_str()), (Some(true), Some(NEWER)), "{up}");
    // Already that or newer: nothing changes, nothing is downloaded.
    let same = s.json(&["update", "--version", "0.0.1"]);
    assert_eq!((same["updated"].as_bool(), same["release"].as_str()), (Some(false), Some("0.0.1")), "{same}");
    let (_, e) = s.err(&["--json", "update", "--version", "9.9.10"]);
    assert_eq!(e["error"]["kind"], "unavailable", "no such release: {e}");
    let (_, e) = s.err(&["--json", "update", "--version", "../x"]);
    assert_eq!(e["error"]["kind"], "usage", "{e}");
    drop(r);
}

#[test]
fn hc_update_never_takes_a_release_of_another_generation() {
    let mut s = Session::new();
    let r = Releases::new(&s, NEWER, &[&here()]);
    let latest = r.root.join("core/latest.json");
    let breaking = std::fs::read_to_string(&latest).unwrap().replace("\"generation\": 1", "\"generation\": 2");
    std::fs::write(&latest, breaking).unwrap();
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("herdr-core")).unwrap(); // never the build under test
    s.bin = dir.join("herdr-core");
    s.env.push(("HERDR_RELEASES_URL".into(), r.url.clone()));
    let (_, e) = s.err(&["--json", "update"]);
    assert_eq!(e["error"]["kind"], "refused", "apps built for this generation must keep working: {e}");
    assert!(e["error"]["message"].as_str().unwrap().contains("generation"), "{e}");
    drop(r);
}

#[test]
fn a_machine_of_another_cpu_gets_the_released_build_for_it() {
    let mut odd = Session::new();
    odd.bin = odd.root.join("bin/herdr-core"); // no hc there yet
    std::fs::create_dir_all(odd.root.join("fakebin")).unwrap();
    let other = if std::env::consts::ARCH == "aarch64" { "x86_64" } else { "aarch64" };
    let os = if std::env::consts::OS == "macos" { "Darwin" } else { "Linux" };
    let uname = odd.root.join("fakebin/uname");
    std::fs::write(&uname, format!("#!/bin/sh\necho '{os} {other}'\n")).unwrap();
    std::fs::set_permissions(&uname, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let mut s = Session::new();
    // The "other CPU" build is this binary, so it runs on the fake machine.
    let target = format!("{}-{other}", std::env::consts::OS);
    let r = Releases::new(&s, THIS, &[&target]);
    s.env.push(fake_ssh(&s, &[("odd", &odd)]));
    s.env.push(("HERDR_RELEASES_URL".into(), r.url.clone()));
    let t = run(&s, &["sh", "-c", &format!("{BIN} machine add odd; echo added-$?; sleep 100")]);
    wait_until("the offer", || text(&s, &t).contains("[Y/n]"));
    s.json(&["send", &t, "y"]);
    wait_until("added", || text(&s, &t).contains("added-0"));
    assert!(odd.root.join(".local/bin/herdr-core").is_file());
    odd.bin = odd.root.join(".local/bin/herdr-core");
    drop(r);
}
