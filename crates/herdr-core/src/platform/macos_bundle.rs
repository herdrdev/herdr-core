//! The independently signed core bundle. Installation never downloads on startup.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const EXECUTABLE: &str = "Contents/MacOS/herdr-core";
const REQUIREMENT: &str =
    "=identifier \"dev.herdr.core\" and anchor apple generic and certificate leaf[subject.OU] = \"2W9652Q7QR\"";
const LSREGISTER: &str =
    "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";

fn bundle(exe: &Path) -> Option<&Path> {
    let root = exe.parent()?.parent()?.parent()?;
    (exe.strip_prefix(root).ok()? == Path::new(EXECUTABLE) && root.file_name()? == "Herdr Core.app").then_some(root)
}

fn checked(program: &str, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    let out = Command::new(program).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program}: {}", String::from_utf8_lossy(&out.stderr))))
    }
}

pub fn verify(path: &Path, local: bool) -> io::Result<()> {
    let verify = |requirement: &str| {
        checked(
            "/usr/bin/codesign",
            &["--verify".as_ref(), "--strict".as_ref(), "-R".as_ref(), requirement.as_ref(), path.as_os_str()],
        )
    };
    if verify(REQUIREMENT).is_ok() {
        return Ok(());
    }
    // Explicit local builds can be ad-hoc, but another team's signature is never ours.
    if local {
        let out = Command::new("/usr/bin/codesign").arg("-dv").arg(path).output()?;
        if String::from_utf8_lossy(&out.stderr).lines().any(|line| line == "Signature=adhoc") {
            return verify("=identifier \"dev.herdr.core\"");
        }
    }
    Err(io::Error::other("expected Herdr's Developer ID signature (dev.herdr.core, team 2W9652Q7QR)"))
}

struct Link {
    dest: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    existed: bool,
    published: bool,
}

impl Link {
    fn stage(target: &Path, dest: &Path) -> io::Result<Option<Self>> {
        if std::fs::read_link(dest).ok().as_deref() == Some(target) {
            return Ok(None);
        }
        let link = Self {
            dest: dest.to_path_buf(),
            staged: dest.with_file_name(format!(".hc-link-{}", super::random_hex())),
            backup: dest.with_file_name(format!(".hc-backup-{}", super::random_hex())),
            existed: dest.symlink_metadata().is_ok(),
            published: false,
        };
        std::os::unix::fs::symlink(target, &link.staged)?;
        if link.existed {
            std::fs::hard_link(dest, &link.backup)?;
        }
        Ok(Some(link))
    }

    fn publish(&mut self) -> io::Result<()> {
        std::fs::rename(&self.staged, &self.dest)?;
        self.published = true;
        Ok(())
    }

    fn rollback(&self) -> io::Result<()> {
        if !self.published {
            return Ok(());
        }
        if self.existed { std::fs::rename(&self.backup, &self.dest) } else { std::fs::remove_file(&self.dest) }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.staged);
        let _ = std::fs::remove_file(&self.backup);
    }
}

pub fn bundle_destination(dest: &Path) -> bool {
    (unsafe { libc::geteuid() != 0 }) && std::env::var_os("HOME").is_some_and(|home| dest.starts_with(home))
}

fn executable_is_gone(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT))
}

fn live_executables() -> io::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    const PROC_ALL_PIDS: u32 = 1; // libproc.h; libc exposes the function but not this selector.
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pids = vec![0i32; bytes as usize / 4 + 4096];
    let capacity = (pids.len() * 4) as i32;
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), capacity) };
    if bytes <= 0 || bytes >= capacity {
        return Err(io::Error::other("incomplete process list"));
    }
    pids.truncate(bytes as usize / 4);
    let mut paths = Vec::new();
    for pid in pids.into_iter().filter(|pid| *pid > 0) {
        let mut buf = [0u8; 4096];
        let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if len <= 0 {
            let error = io::Error::last_os_error();
            // An unlinked executable cannot live in a retained bundle that still exists.
            if executable_is_gone(&error) {
                continue;
            }
            return Err(error); // An unreadable live process means we cannot safely reclaim anything.
        }
        let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        paths.push(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])));
    }
    Ok(paths)
}

fn reclaim(previous: &Path, live: io::Result<Vec<PathBuf>>) -> io::Result<()> {
    let Ok(live) = live else { return Ok(()) };
    for entry in std::fs::read_dir(previous)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() && !live.iter().any(|exe| exe.starts_with(&path)) {
            std::fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

fn base_directory(root: &Path) -> &Path {
    let parent = root.parent().unwrap();
    if parent.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "versions")) {
        parent.parent().unwrap().parent().unwrap()
    } else if parent.file_name().is_some_and(|n| n == "active") {
        parent.parent().unwrap()
    } else {
        parent
    }
}

fn activate(staged: &Path, active: &Path) -> io::Result<()> {
    if !active.exists() {
        return std::fs::rename(staged, active);
    }
    use std::os::unix::ffi::OsStrExt;
    let staged = std::ffi::CString::new(staged.as_os_str().as_bytes())?;
    let active = std::ffi::CString::new(active.as_os_str().as_bytes())?;
    // The old executable's vnode stays reachable in the staging slot. A crash
    // leaves either the old or new active bundle, never a missing active entry.
    if unsafe {
        libc::renameatx_np(libc::AT_FDCWD, staged.as_ptr(), libc::AT_FDCWD, active.as_ptr(), libc::RENAME_SWAP)
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Keep the newest bundle at one physical path. Old live executables remain
/// reachable after an atomic rename into retained storage.
pub fn install(source: &Path, dest: &Path, works: impl Fn(&Path) -> bool) -> io::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Err(io::Error::other("install Herdr Core as your user, not with sudo"));
    }
    if dest.exists()
        && let Some(manager) = crate::client::installed_by_path(dest)
    {
        return Err(io::Error::other(format!(
            "{} belongs to {manager}; update it with its package manager",
            dest.display()
        )));
    }
    let source = bundle(source).ok_or_else(|| io::Error::other("source is not a core app bundle"))?;
    verify(source, true)?;
    let enclosing_app = dest.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"));
    if let Some(app) = enclosing_app {
        if bundle(dest) != Some(app) {
            return Err(io::Error::other("refusing to replace a host application"));
        }
        verify(app, true)?;
    }
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is unset"))?;
    let home = PathBuf::from(home);
    let base = if let Some(root) = bundle(dest) {
        base_directory(root).to_path_buf()
    } else {
        home.join(".local/share/herdr-core")
    };
    std::fs::create_dir_all(&base)?;
    let base = base.canonicalize()?;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(base.join("install.lock"))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let versions = base.join("versions");
    let current = base.join("current");
    let legacy = base.join("Herdr Core.app");
    std::fs::create_dir_all(&versions)?;
    let active = base.join("active/Herdr Core.app");
    std::fs::create_dir_all(active.parent().unwrap())?;
    // Keep the installed version even when no core is running. An unreadable
    // process means nothing is reclaimed, rather than risking a live bundle.
    let live = live_executables().and_then(|mut live| {
        if let Ok(path) = current.canonicalize() {
            live.push(path.join(EXECUTABLE));
        }
        live.push(source.join(EXECUTABLE).canonicalize()?);
        if bundle(dest).is_some() {
            live.push(dest.to_path_buf());
        }
        Ok(live)
    });
    if let Ok(live) = &live
        && legacy.is_dir()
        && !legacy.is_symlink()
        && !live.iter().any(|exe| exe.starts_with(&legacy))
    {
        verify(&legacy, true)?;
        std::fs::remove_dir_all(&legacy)?;
    }
    let _ = reclaim(&versions, live);
    let mut entries = Vec::new();
    if bundle(dest).is_none() {
        entries.push(dest.to_path_buf());
        let hc = dest.with_file_name("hc");
        let ours = hc.canonicalize().ok().is_some_and(|p| dest.canonicalize().ok().as_ref() == Some(&p));
        let on_path =
            std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("hc").exists()));
        if ours || (hc.symlink_metadata().is_err() && !on_path) {
            entries.push(hc);
        }
    } else if dest != active.join(EXECUTABLE) {
        // Aliases to active survive swaps; only migrate aliases from older layouts.
        let mut dirs = vec![home.join(".local/bin")];
        if let Some(dir) = std::env::var_os("HC_INSTALL_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
        for dir in dirs {
            for name in ["herdr-core", "hc"] {
                let entry = dir.join(name);
                if entry.is_symlink() && entry.canonicalize().ok().as_deref() == Some(dest) && !entries.contains(&entry)
                {
                    entries.push(entry);
                }
            }
        }
    }
    let slot = versions.join(super::random_hex());
    std::fs::create_dir(&slot)?;
    let staged = slot.join("Herdr Core.app");
    let mut activated = false;
    let result = (|| {
        checked("/usr/bin/ditto", &[source.as_os_str(), staged.as_os_str()])?;
        verify(&staged, true)?;
        if !works(&staged.join(EXECUTABLE)) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut links = Vec::new();
        for (target, entry) in std::iter::once((active.clone(), current.clone()))
            .chain(entries.into_iter().map(|entry| (current.join(EXECUTABLE), entry)))
            .chain(legacy.symlink_metadata().is_err().then(|| (current.clone(), legacy.clone())))
        {
            if let Some(link) = Link::stage(&target, &entry)? {
                links.push(link);
            }
        }
        let replacing = active.exists();
        activate(&staged, &active)?;
        activated = true;
        for index in 0..links.len() {
            if let Err(error) = links[index].publish() {
                let mut rollback_error = None;
                for link in links[..index].iter().rev() {
                    if let Err(error) = link.rollback() {
                        rollback_error = Some(error);
                    }
                }
                let restored = if replacing { activate(&staged, &active) } else { std::fs::rename(&active, &staged) };
                restored?;
                activated = false;
                return Err(rollback_error.unwrap_or(error));
            }
        }
        Ok(())
    })();
    if !activated {
        let _ = std::fs::remove_dir_all(slot);
    }
    // LaunchServices doesn't find apps in hidden folders by itself. Unknown to
    // it, the first Local Network prompt names `herdr-core` with Apple's generic text.
    if result.is_ok() {
        let _ = Command::new(LSREGISTER).arg("-f").arg(&active).output();
    }
    result
}

pub fn core_executable(exe: &Path) -> PathBuf {
    if let Some(root) = bundle(exe) {
        let base = base_directory(root);
        let current = base.join("current").join(EXECUTABLE);
        if current.is_file() && verify(&base.join("current"), true).is_ok() {
            return current;
        }
    }
    exe.to_path_buf()
}

pub fn installer_startup() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--hc-install-bundle")) {
        return false;
    }
    let args: Vec<_> = std::env::args_os().skip(2).collect();
    let result = if args.len() == 2 {
        install(Path::new(&args[0]), Path::new(&args[1]), |exe| {
            Command::new(exe).arg("--version").status().is_ok_and(|s| s.success())
        })
    } else {
        Err(io::Error::other("expected source executable and destination entry point"))
    };
    if let Err(error) = result {
        eprintln!("hc: bundle install failed: {error}");
        std::process::exit(1);
    }
    std::process::exit(0)
}

pub fn needs_bundle_update() -> bool {
    std::env::current_exe().and_then(|p| p.canonicalize()).is_ok_and(|exe| {
        bundle(&exe).is_none()
            && !exe.ancestors().any(|p| p.extension().is_some_and(|e| e == "app"))
            && bundle_destination(&exe)
            && crate::client::installed_by().is_none()
            && verify(&exe, false).is_ok()
    })
}

pub fn legacy_notice(state: &Path) {
    if needs_bundle_update() {
        legacy_notice_once(state, &mut std::io::stderr());
    }
}

fn legacy_notice_once(state: &Path, out: &mut impl std::io::Write) {
    if super::private_dir(state).is_ok()
        && std::fs::OpenOptions::new().write(true).create_new(true).open(state.join("bundle-upgrade-notice")).is_ok()
    {
        let _ = writeln!(out, "hc: run hc update to finish installing Herdr Core");
    }
}

pub fn source_is_bundle(path: &Path) -> bool {
    bundle(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_legacy_notice_reaches_stderr_once_even_before_the_core_has_started() {
        let root = std::env::temp_dir().join(format!("hc-notice-{}", super::super::random_hex()));
        let state = root.join("state");
        let mut out = Vec::new();
        legacy_notice_once(&state, &mut out);
        legacy_notice_once(&state, &mut out);
        assert_eq!(out, b"hc: run hc update to finish installing Herdr Core\n");
        assert!(state.join("bundle-upgrade-notice").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_the_exact_bundle_executable_has_a_bundle_root() {
        assert_eq!(
            bundle(Path::new("/share/Herdr Core.app/Contents/MacOS/herdr-core")),
            Some(Path::new("/share/Herdr Core.app"))
        );
        for path in [
            "/bin/herdr-core",
            "/x.app/Contents/MacOS/herdr-core",
            "/x/Contents/MacOS/herdr-core",
            "/Herdr Core.app/Contents/MacOS/hc",
        ] {
            assert!(bundle(Path::new(path)).is_none(), "{path}");
        }
    }
    #[test]
    fn only_missing_processes_or_executables_are_safe_to_skip() {
        for code in [libc::ESRCH, libc::ENOENT] {
            assert!(executable_is_gone(&io::Error::from_raw_os_error(code)), "errno {code}");
        }
        for code in [libc::EPERM, libc::EACCES, libc::EIO] {
            assert!(!executable_is_gone(&io::Error::from_raw_os_error(code)), "errno {code}");
        }
        assert!(!executable_is_gone(&io::Error::other("unknown failure")));
    }

    #[test]
    fn live_paths_include_this_process() {
        let paths = live_executables().unwrap();
        assert!(paths.contains(&std::env::current_exe().unwrap().canonicalize().unwrap()));
    }

    #[test]
    fn another_vendor_is_not_a_core_release() {
        assert!(verify(Path::new("/bin/ls"), true).is_err());
    }

    #[test]
    fn bundle_base_is_shared_by_active_retained_and_legacy_paths() {
        for path in [
            "/share/core/active/Herdr Core.app",
            "/share/core/versions/123/Herdr Core.app",
            "/share/core/Herdr Core.app",
        ] {
            assert_eq!(base_directory(Path::new(path)), Path::new("/share/core"));
        }
    }

    #[test]
    fn atomic_activation_preserves_old_inodes_and_recovers_interrupted_slots() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(format!("hc-activation-{}", super::super::random_hex()));
        let active = root.join("active/Herdr Core.app");
        let versions = root.join("versions");
        let first = versions.join("first/Herdr Core.app");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(first.join("image"), "A").unwrap();
        activate(&first, &active).unwrap();
        let inode = std::fs::metadata(active.join("image")).unwrap().ino();
        let second = versions.join("second/Herdr Core.app");
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("image"), "B").unwrap();
        activate(&second, &active).unwrap();
        assert_eq!(std::fs::read_to_string(active.join("image")).unwrap(), "B");
        assert_eq!(std::fs::metadata(second.join("image")).unwrap().ino(), inode);
        assert!(activate(&root.join("missing"), &active).is_err());
        assert_eq!(std::fs::read_to_string(active.join("image")).unwrap(), "B");
        // A crash after the exchange leaves the old app in its staging slot.
        // Unknown process inventory must preserve it, just like a live holder.
        reclaim(&versions, Err(io::Error::other("unreadable process"))).unwrap();
        assert!(second.exists());
        reclaim(&versions, Ok(vec![second.join(EXECUTABLE)])).unwrap();
        assert!(second.exists());
        reclaim(&versions, Ok(vec![])).unwrap();
        assert!(!second.exists());
        assert!(active.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reclaim_keeps_live_bundles_and_removes_unused_ones() {
        let root = std::env::temp_dir().join(format!("hc-previous-{}", super::super::random_hex()));
        let active = root.join("active/Herdr Core.app/Contents/MacOS");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::create_dir_all(root.join("unused/Herdr Core.app")).unwrap();
        reclaim(&root, Ok(vec![active.join("herdr-core")])).unwrap();
        assert!(active.exists());
        assert!(!root.join("unused").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
