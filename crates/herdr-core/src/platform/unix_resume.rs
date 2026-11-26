use super::{ProcessState, ResumeProcess};
use crate::resume::Identity;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;

pub(crate) fn resume_user() -> Option<String> {
    let uid = unsafe { libc::geteuid() };
    (uid != 0 && uid == unsafe { libc::getuid() }).then(|| uid.to_string())
}

pub(crate) fn secure_state_dir(path: &Path) -> io::Result<()> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "state directory must belong to this user"));
    }
    if meta.mode() & 0o7777 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn private_launch_file(meta: &std::fs::Metadata) -> bool {
    meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0 && meta.nlink() == 1
}

#[cfg(target_os = "linux")]
pub(crate) fn resume_process(pid: u32) -> ProcessState {
    fn observe(pid: u32) -> io::Result<Option<ResumeProcess>> {
        use std::os::fd::AsRawFd;
        let process = std::fs::File::open(format!("/proc/{pid}"))?;
        let dir = std::path::PathBuf::from(format!("/proc/self/fd/{}", process.as_raw_fd()));
        let stat = std::fs::read(dir.join("stat"))?;
        let Some(at) = stat.iter().rposition(|b| *b == b')') else { return Ok(None) };
        let fields = std::str::from_utf8(&stat[at + 1..]).map_err(io::Error::other)?;
        let mut fields = fields.split_ascii_whitespace();
        if matches!(fields.next(), Some("Z" | "X")) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let Some(parent) = fields.next().and_then(|s| s.parse().ok()) else { return Ok(None) };
        let Some(start) = fields.nth(17).and_then(|s| s.parse().ok()) else { return Ok(None) };
        let status = std::fs::read(dir.join("status"))?;
        let user = status
            .split(|b| *b == b'\n')
            .find_map(|l| l.strip_prefix(b"Uid:"))
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.split_ascii_whitespace().nth(1));
        let Some(user) = user else { return Ok(None) };
        Ok(Some(ResumeProcess { identity: Identity { pid, start }, parent, user: user.into() }))
    }
    match observe(pid) {
        Ok(Some(process)) => ProcessState::Alive(process),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound) || e.raw_os_error() == Some(libc::ESRCH) => {
            // hidepid can make a live process invisible, in which case kill(0) sees it or returns EPERM.
            if std::path::Path::new(&format!("/proc/{pid}")).exists()
                || (pid <= i32::MAX as u32
                    && unsafe { libc::kill(pid as i32, 0) } != 0
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
            {
                ProcessState::Gone
            } else {
                ProcessState::Unknown
            }
        }
        _ => ProcessState::Unknown,
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn resume_process(pid: u32) -> ProcessState {
    let Ok(raw_pid) = i32::try_from(pid) else { return ProcessState::Unknown };
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let len = unsafe { libc::proc_pidinfo(raw_pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if len != size {
        return if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            ProcessState::Gone
        } else {
            ProcessState::Unknown
        };
    }
    if info.pbi_status == 5 {
        return ProcessState::Gone; // SZOMB
    }
    let mut usage: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    if unsafe { libc::proc_pid_rusage(raw_pid, libc::RUSAGE_INFO_V0, (&raw mut usage).cast()) } != 0 {
        return ProcessState::Unknown;
    }
    ProcessState::Alive(ResumeProcess {
        identity: Identity { pid, start: usage.ri_proc_start_abstime },
        parent: info.pbi_ppid,
        user: info.pbi_uid.to_string(),
    })
}

pub(crate) fn resume_shell(shell: &[String], helper: &Path, file: &Path, first: bool) -> Option<Vec<String>> {
    let exe = shell.first()?;
    let name = Path::new(exe).file_name()?.to_str()?;
    let helper = helper.to_str()?.to_owned();
    let file = file.to_str()?.to_owned();
    if !matches!(name, "bash" | "zsh" | "sh" | "dash" | "fish") {
        let script =
            if first { "exec \"$1\" __resume \"$2\" --first" } else { "\"$1\" __resume \"$2\"; shift 2; exec \"$@\"" };
        let mut words = vec!["/bin/sh".into(), "-c".into(), script.into(), "sh".into(), helper, file];
        if !first {
            words.extend_from_slice(shell);
        }
        return Some(words);
    }
    if !shell[1..].iter().all(|s| matches!(s.as_str(), "-l" | "-i" | "-li" | "-il" | "--login")) {
        return None;
    }
    let login =
        cfg!(target_os = "macos") || shell[1..].iter().any(|s| matches!(s.as_str(), "-l" | "-li" | "-il" | "--login"));
    let (startup, interactive) = if login { ("-lic", "-li") } else { ("-ic", "-i") };
    Some(if name == "fish" {
        let script = if first {
            "exec \"$argv[1]\" __resume \"$argv[2]\" --first".into()
        } else {
            format!("\"$argv[1]\" __resume \"$argv[2]\"; exec \"$argv[3]\" {interactive}")
        };
        vec![exe.clone(), startup.into(), script, helper, file, exe.clone()]
    } else {
        let script = if first {
            "exec \"$1\" __resume \"$2\" --first".into()
        } else {
            format!("\"$1\" __resume \"$2\"; exec \"$0\" {interactive}")
        };
        vec![exe.clone(), startup.into(), script, exe.clone(), helper, file]
    })
}

pub(crate) fn run_resume(argv: &[String], cwd: &str) -> io::Result<i32> {
    fn executable(path: &Path) -> Option<CString> {
        if !path.is_file() {
            return None;
        }
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        (unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) } == 0).then_some(path)
    }
    let name = Path::new(&argv[0]);
    let path = if name.is_absolute() {
        executable(name)
    } else {
        std::env::var_os("PATH")
            .and_then(|path| std::env::split_paths(&path).find_map(|dir| executable(&dir.join(name))))
    }
    .ok_or(io::ErrorKind::NotFound)?;
    let args = argv.iter().map(|arg| CString::new(arg.as_bytes())).collect::<Result<Vec<_>, _>>()?;
    let pointers = args.iter().map(|arg| arg.as_ptr()).chain(std::iter::once(std::ptr::null())).collect::<Vec<_>>();
    // The helper runs before the CLI starts any threads.
    unsafe {
        std::env::set_var("PWD", cwd);
        libc::execv(path.as_ptr(), pointers.as_ptr());
    }
    Err(io::Error::last_os_error())
}

pub(crate) fn resume_fallback(shell: &[String]) -> i32 {
    if let Some((exe, args)) = shell.split_first() {
        let _ = std::process::Command::new(exe).args(args).exec();
    }
    1
}
