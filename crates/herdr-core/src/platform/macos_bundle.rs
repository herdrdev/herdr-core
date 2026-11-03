//! The independently signed core bundle and legacy bare-install migration.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const EXECUTABLE: &str = "Contents/MacOS/herdr-core";

fn bundle(exe: &Path) -> Option<&Path> {
    let root = exe.parent()?.parent()?.parent()?;
    (exe.strip_prefix(root).ok()? == Path::new(EXECUTABLE) && root.extension()? == "app").then_some(root)
}

fn home_bundle() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is unset"))?;
    Ok(PathBuf::from(home).join(".local/share/herdr-core/Herdr Core.app"))
}

fn checked(program: &str, args: &[&std::ffi::OsStr]) -> io::Result<()> {
    let out = Command::new(program).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program}: {}", String::from_utf8_lossy(&out.stderr))))
    }
}

fn link(target: &Path, dest: &Path) -> io::Result<()> {
    let staged = dest.with_file_name(format!(".hc-link-{}", super::random_hex()));
    std::os::unix::fs::symlink(target, &staged)?;
    let result = std::fs::rename(&staged, dest);
    let _ = std::fs::remove_file(staged);
    result
}

/// Stage the entire sealed bundle, then exchange directories atomically. An
/// already running executable/holder keeps its open vnode through replacement.
pub fn install(source: &Path, dest: &Path, works: impl Fn(&Path) -> bool) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if dest.exists()
        && let Some(manager) = crate::client::installed_by_path(dest)
    {
        return Err(io::Error::other(format!(
            "{} belongs to {manager}; update it with its package manager",
            dest.display()
        )));
    }
    let source = bundle(source).ok_or_else(|| io::Error::other("source is not a core app bundle"))?;
    let target = bundle(dest).map(Path::to_path_buf).map_or_else(home_bundle, Ok)?;
    let parent = target.parent().unwrap();
    std::fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".Herdr Core-{}.app", super::random_hex()));
    let result = (|| {
        checked("/usr/bin/ditto", &[source.as_os_str(), staged.as_os_str()])?;
        checked("/usr/bin/codesign", &["--verify".as_ref(), "--strict".as_ref(), staged.as_os_str()])?;
        if !works(&staged.join(EXECUTABLE)) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if target.exists() {
            let from = std::ffi::CString::new(staged.as_os_str().as_bytes())?;
            let to = std::ffi::CString::new(target.as_os_str().as_bytes())?;
            if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_SWAP) } != 0 {
                return Err(io::Error::last_os_error());
            }
        } else {
            std::fs::rename(&staged, &target)?;
        }
        if bundle(dest).is_none() {
            let exe = target.join(EXECUTABLE);
            let hc = dest.with_file_name("hc");
            let ours = hc.canonicalize().ok().is_some_and(|p| dest.canonicalize().ok().as_ref() == Some(&p));
            link(&exe, dest)?;
            // Never replace an unrelated window manager's hc, even a symlink.
            if hc.symlink_metadata().is_err() || ours {
                link(&exe, &hc)?;
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(staged);
    result
}

/// Internal installer entry point: also used by install.sh before a core exists.
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

/// Old updaters copy only the root executable. Never hold up serve for network
/// access: install its matching signed release in the background, then request
/// a planned restart so new holders use the installed core's build identity.
pub fn migrate_legacy() {
    let Ok(exe) = std::env::current_exe().and_then(|p| p.canonicalize()) else { return };
    if bundle(&exe).is_some() || crate::client::installed_by().is_some() {
        return;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let bin = std::env::var_os("HC_INSTALL_DIR").map(PathBuf::from).or_else(|| home.map(|h| h.join(".local/bin")));
    let bin = bin.and_then(|p| p.canonicalize().ok());
    if bin.as_deref() != exe.parent() || exe.file_name() != Some("herdr-core".as_ref()) {
        return;
    }
    let Ok(identity) = std::fs::metadata(&exe).map(|m| super::file_identity(&m)) else { return };
    let _ = std::thread::Builder::new().name("core-bundle-migration".into()).spawn(move || {
        let result = (|| {
            let scratch = crate::releases::Scratch::new().map_err(|e| io::Error::other(e.message))?;
            let manifest = crate::releases::manifest(Some(env!("CARGO_PKG_VERSION")), &scratch.0).map_err(|e| io::Error::other(e.message))?;
            let from = crate::releases::download(&manifest, &crate::releases::this_target(), &scratch.0).map_err(|e| io::Error::other(e.message))?;
            // Do not migrate over a concurrent update or a manually replaced binary.
            if super::file_identity(&std::fs::metadata(&exe)?) != identity { return Err(io::Error::other("installed core changed during migration")); }
            install(&from, &exe, |staged| Command::new(staged).arg("--version").output().is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == concat!("hc ", env!("CARGO_PKG_VERSION"))))?;
            let paths = crate::paths::Paths::from_env().map_err(|e| io::Error::other(e.message))?;
            crate::client::spawn_detached(&paths, &["server", "restart"], "core.log")
        })();
        match result {
            Ok(()) => eprintln!("hc: installed Herdr Core.app; requested a planned core restart, keeping terminals. Recreate pre-update terminals if their macOS privacy access is broken."),
            Err(error) => eprintln!("hc: legacy bundle migration deferred (will retry next start): {error}"),
        }
    });
}

pub fn source_is_bundle(path: &Path) -> bool {
    bundle(path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_exact_bundle_executable_has_a_bundle_root() {
        assert_eq!(
            bundle(Path::new("/share/Herdr Core.app/Contents/MacOS/herdr-core")),
            Some(Path::new("/share/Herdr Core.app"))
        );
        for path in [
            "/bin/herdr-core",
            "/x.app/other/MacOS/herdr-core",
            "/x/Contents/MacOS/herdr-core",
            "/x.app/Contents/MacOS/hc",
        ] {
            assert!(bundle(Path::new(path)).is_none(), "{path}");
        }
    }
}
