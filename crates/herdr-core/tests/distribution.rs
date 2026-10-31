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

    s.bin = bin.join("hc");
    let t = run(&s, &["sh", "-c", "while read l; do echo got-$l; done"]);
    let pid = s.json(&["server", "status"])["pid"].clone();
    assert!(install(&s).contains("updated"), "again: an update");
    assert_ne!(s.json(&["server", "status"])["pid"], pid, "the planned restart");
    s.json(&["send", &t, "still"]);
    wait_until("the terminal kept running", || text(&s, &t).contains("got-still"));
    drop(r);
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
