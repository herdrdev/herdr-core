//! Windows: named pipes, ConPTY, job objects, the console API.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::IO::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Pipes::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::core::BOOL;

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(Some(0)).collect()
}

fn check(ok: BOOL) -> io::Result<()> {
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

// --- Connections: named pipes in overlapped mode ---------------------------
//
// Synchronous I/O on one pipe is serialized by Windows: a read waiting in one
// thread would hold up every write from another. Overlapped handles let both
// run at once; each call still blocks its own thread until done.

struct Pipe {
    handle: OwnedHandle,
}

impl Pipe {
    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    /// One overlapped operation, waited for.
    fn io(&self, op: impl FnOnce(HANDLE, *mut OVERLAPPED) -> BOOL) -> io::Result<usize> {
        unsafe {
            let event = CreateEventW(null(), 1, 0, null());
            if event.is_null() {
                return Err(io::Error::last_os_error());
            }
            let event = OwnedHandle::from_raw_handle(event as RawHandle);
            let mut ov: OVERLAPPED = zeroed();
            ov.hEvent = event.as_raw_handle() as HANDLE;
            if op(self.raw(), &mut ov) == 0 {
                let err = GetLastError();
                if err != ERROR_IO_PENDING {
                    return match err {
                        ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED => Ok(0),
                        _ => Err(io::Error::from_raw_os_error(err as i32)),
                    };
                }
            }
            let mut n = 0u32;
            if GetOverlappedResult(self.raw(), &ov, &mut n, 1) == 0 {
                return match GetLastError() {
                    ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED => Ok(0),
                    err => Err(io::Error::from_raw_os_error(err as i32)),
                };
            }
            Ok(n as usize)
        }
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.io(|h, ov| unsafe {
            ReadFile(h, buf.as_mut_ptr(), buf.len().min(u32::MAX as usize) as u32, null_mut(), ov)
        })
    }

    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let n = self.io(|h, ov| unsafe {
            WriteFile(h, buf.as_ptr(), buf.len().min(u32::MAX as usize) as u32, null_mut(), ov)
        })?;
        if n == 0 && !buf.is_empty() {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        Ok(n)
    }
}

/// A connection to a core or a holder (blocking, safe to read and write from
/// different threads at once).
#[derive(Clone)]
pub struct Stream(Arc<Pipe>);

impl Stream {
    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(self.clone())
    }

    /// Pipes can't close one direction: either way ends the connection, and
    /// wakes any read waiting on it.
    pub fn shutdown(&self, _: std::net::Shutdown) -> io::Result<()> {
        unsafe {
            CancelIoEx(self.0.raw(), null());
            DisconnectNamedPipe(self.0.raw());
        }
        Ok(())
    }

    pub fn set_read_timeout(&self, _: Option<std::time::Duration>) -> io::Result<()> {
        Ok(())
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Read for &Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn connect(path: &Path) -> io::Result<Stream> {
    let name = wide(path.as_os_str());
    for _ in 0..50 {
        let h = unsafe {
            // Identification only: whoever serves this name can't act as us
            // (a fake core can't borrow an elevated hc's rights).
            let flags = FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
            CreateFileW(name.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, flags, null_mut())
        };
        if h != INVALID_HANDLE_VALUE {
            return Ok(Stream(Arc::new(Pipe { handle: unsafe { OwnedHandle::from_raw_handle(h as RawHandle) } })));
        }
        // Every instance busy: one frees up as soon as the server accepts.
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
            return Err(io::Error::last_os_error());
        }
        unsafe { WaitNamedPipeW(name.as_ptr(), 100) };
    }
    Err(io::Error::from(io::ErrorKind::TimedOut))
}

/// Who may open our pipes: this account and SYSTEM (the default also lets
/// everyone read, and names Administrators as owner when elevated). The
/// integrity label keeps lower processes out: an unelevated program can't
/// drive an elevated hc.
fn pipe_security() -> io::Result<*const SECURITY_ATTRIBUTES> {
    use windows_sys::Win32::Security::Authorization::*;
    use windows_sys::Win32::Security::*;
    static MADE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    if let Some(sa) = MADE.get() {
        return Ok(*sa as *const SECURITY_ATTRIBUTES);
    }
    unsafe {
        let mut token = null_mut();
        check(OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token))?;
        let token = OwnedHandle::from_raw_handle(token as RawHandle);
        let mut n = 0u32;
        GetTokenInformation(token.as_raw_handle() as HANDLE, TokenUser, null_mut(), 0, &mut n);
        let mut user = vec![0usize; (n as usize).div_ceil(size_of::<usize>())];
        check(GetTokenInformation(token.as_raw_handle() as HANDLE, TokenUser, user.as_mut_ptr().cast(), n, &mut n))?;
        let mut sid = null_mut();
        check(ConvertSidToStringSidW((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut sid))?;
        let sid_text =
            String::from_utf16_lossy(std::slice::from_raw_parts(sid, (0..).take_while(|&i| *sid.add(i) != 0).count()));
        LocalFree(sid.cast());
        let mut elevation: TOKEN_ELEVATION = zeroed();
        check(GetTokenInformation(
            token.as_raw_handle() as HANDLE,
            TokenElevation,
            (&raw mut elevation).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        ))?;
        let level = if elevation.TokenIsElevated != 0 { "HI" } else { "ME" };
        let sddl = wide(format!("D:P(A;;GA;;;SY)(A;;GA;;;{sid_text})S:(ML;;NRNW;;;{level})").as_ref());
        let mut sd = null_mut();
        check(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        ))?;
        // Made once, kept for the life of the process.
        let sa = Box::leak(Box::new(SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        }));
        Ok(*MADE.get_or_init(|| sa as *const _ as usize) as *const SECURITY_ATTRIBUTES)
    }
}

fn new_instance(name: &[u16], first: bool) -> io::Result<Pipe> {
    let flags = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let h = unsafe {
        CreateNamedPipeW(name.as_ptr(), flags, mode, PIPE_UNLIMITED_INSTANCES, 65536, 65536, 0, pipe_security()?)
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Pipe { handle: unsafe { OwnedHandle::from_raw_handle(h as RawHandle) } })
}

/// A pipe name others connect to (blocking accepts: the holder).
pub struct Listener {
    name: Vec<u16>,
    next: std::sync::Mutex<Option<Pipe>>,
}

pub fn may_autostart(explicit: bool) -> Result<(), crate::rpc::RpcError> {
    if explicit { Ok(()) } else { super::windows_session::check_autostart() }
}

pub fn status_may_start_core() -> bool {
    false
}

pub fn connect_error(error: &io::Error, existing_only: bool) -> Option<crate::rpc::RpcError> {
    if error.kind() == io::ErrorKind::PermissionDenied {
        Some(crate::rpc::RpcError::new(
            "unavailable",
            format!("the Windows core exists but this process cannot access its pipe: {error}"),
            "if it was started elevated or over SSH, stop it with 'hc server stop' in an elevated terminal on that Windows machine, then open Herdr or run 'hc server start' in a normal desktop terminal",
        ))
    } else if existing_only {
        Some(crate::rpc::RpcError::new(
            "unavailable",
            format!("the Windows core is not reachable: {error}"),
            "open Herdr on that Windows machine, or run 'hc server start' there",
        ))
    } else {
        None
    }
}

pub fn merge_env(env: &mut BTreeMap<String, String>, extra: BTreeMap<String, String>) {
    env.retain(|key, _| !extra.keys().any(|other| key.eq_ignore_ascii_case(other)));
    env.extend(extra);
}

pub fn listen(path: &Path) -> io::Result<Listener> {
    let name = wide(path.as_os_str());
    let first = new_instance(&name, true)?;
    Ok(Listener { name, next: std::sync::Mutex::new(Some(first)) })
}

impl Listener {
    pub fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        std::iter::from_fn(move || Some(self.accept()))
    }

    fn accept(&self) -> io::Result<Stream> {
        let pipe = match self.next.lock().unwrap_or_else(|p| p.into_inner()).take() {
            Some(pipe) => pipe,
            None => new_instance(&self.name, false)?,
        };
        let connected = pipe.io(|h, ov| unsafe { ConnectNamedPipe(h, ov) });
        match connected {
            Ok(_) => {}
            // A client that came before we waited is connected already.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) => {}
            Err(e) => return Err(e),
        }
        *self.next.lock().unwrap_or_else(|p| p.into_inner()) = Some(new_instance(&self.name, false)?);
        Ok(Stream(Arc::new(pipe)))
    }
}

pub type AsyncRead = tokio::io::ReadHalf<tokio::net::windows::named_pipe::NamedPipeServer>;
pub type AsyncWrite = tokio::io::WriteHalf<tokio::net::windows::named_pipe::NamedPipeServer>;

/// The core's pipe for clients.
pub struct AsyncListener {
    name: PathBuf,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

impl AsyncListener {
    /// Pipes are made inside the runtime; this only names it.
    pub fn bind(path: &Path) -> io::Result<PathBuf> {
        Ok(path.to_path_buf())
    }

    pub fn serve(name: PathBuf) -> io::Result<AsyncListener> {
        let next = Self::create(&name, true)?;
        Ok(AsyncListener { name, next })
    }

    fn create(name: &Path, first: bool) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
        options.first_pipe_instance(first).reject_remote_clients(true);
        unsafe { options.create_with_security_attributes_raw(name, pipe_security()? as *mut c_void) }
    }

    pub async fn accept(&mut self) -> io::Result<(AsyncRead, AsyncWrite)> {
        self.next.connect().await?;
        let connected = std::mem::replace(&mut self.next, Self::create(&self.name, false)?);
        Ok(tokio::io::split(connected))
    }
}

/// Pipe names vanish with their last handle.
pub fn remove_endpoint(_: &Path) {}

// --- Files -----------------------------------------------------------------

/// Default homes for state and config (XDG_STATE_HOME / XDG_CONFIG_HOME win).
pub fn data_dirs() -> (PathBuf, PathBuf) {
    let profile = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| r"C:\".into());
    let local = std::env::var_os("LOCALAPPDATA").map_or_else(|| profile.join(r"AppData\Local"), PathBuf::from);
    let roaming = std::env::var_os("APPDATA").map_or_else(|| profile.join(r"AppData\Roaming"), PathBuf::from);
    (local, roaming)
}

/// Pipes live in their own namespace; names carry the user so two users'
/// cores never meet.
/// Pipe names: up to 256 characters.
pub const MAX_SOCKET_PATH: usize = 256;

pub fn endpoints(_state: &Path) -> (PathBuf, String) {
    let user: String =
        std::env::var("USERNAME").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    (PathBuf::from(r"\\.\pipe\"), format!("herdr-core-{user}-"))
}

pub fn private_dir(path: &Path) -> io::Result<()> {
    if path.as_os_str().to_string_lossy().starts_with(r"\\.\pipe") {
        return Ok(());
    }
    std::fs::create_dir_all(path)
}

pub fn try_lock(file: &File) -> bool {
    unsafe {
        let mut ov: OVERLAPPED = zeroed();
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut ov,
        ) != 0
    }
}

pub fn can_replace(_: &Path) -> bool {
    true
}

pub fn make_executable(_: &Path) -> io::Result<()> {
    Ok(())
}

/// The files of a release, as they sit beside herdr-core.exe.
const RELEASE: [&str; 3] = ["herdr-core.exe", r"conpty\conpty.dll", r"conpty\x64\OpenConsole.exe"];

/// Puts the release whose herdr-core.exe is `from` where `dest`'s folder
/// is. Windows won't overwrite a running file but will rename one: every
/// file is staged and checked first, then each old one is moved aside and
/// the new one moved in; any failure moves everything back. Moved-aside
/// files go on the next update. `hc.exe`, if it is a link to herdr-core.exe,
/// is linked to the new one.
pub fn swap_in(from: &Path, dest: &Path, works: impl Fn(&Path) -> bool) -> io::Result<()> {
    let dir = dest.parent().ok_or_else(|| io::Error::other("no folder"))?;
    let src = from.parent().ok_or_else(|| io::Error::other("no folder"))?;
    let _lock = retry(|| {
        std::os::windows::fs::OpenOptionsExt::share_mode(
            std::fs::OpenOptions::new().create(true).truncate(false).write(true),
            0,
        )
        .open(dir.join(".update.lock"))
    })?;
    for leftover in [dir.to_path_buf(), dir.join("conpty"), dir.join(r"conpty\x64")]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
    {
        // Only what an update named: <release file>.<old|new|failed>-<pid>-<32 hex>.
        let name = leftover.file_name().to_string_lossy().to_ascii_lowercase();
        let tagged = |rest: &str| {
            rest.split_once('-').is_some_and(|(pid, hex)| {
                !pid.is_empty()
                    && pid.bytes().all(|b| b.is_ascii_digit())
                    && hex.len() == 32
                    && hex.bytes().all(|b| b.is_ascii_hexdigit())
            })
        };
        let ours = ["herdr-core.exe", "conpty.dll", "openconsole.exe", "hc.exe"]
            .iter()
            .flat_map(|f| ["old", "new", "failed"].map(|w| format!("{f}.{w}-")))
            .any(|prefix| name.strip_prefix(&prefix).is_some_and(tagged));
        if ours {
            let _ = std::fs::remove_file(leftover.path()); // still in use: next time
        }
    }
    let tag = format!("{}-{}", std::process::id(), crate::platform::random_hex());
    let aside = |p: &Path, what: &str| PathBuf::from(format!("{}.{what}-{tag}", p.display()));
    let exe = dir.join(RELEASE[0]);
    let hc = dir.join("hc.exe");
    let linked = |a: &Path, b: &Path| matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(a), Ok(b)) if file_identity(&a) == file_identity(&b));
    let hc_ours = linked(&hc, &exe);

    // (staged, final) pairs, all staged before anything moves.
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let stage = || -> io::Result<Vec<(PathBuf, PathBuf)>> {
        let mut done = Vec::new();
        for rel in RELEASE {
            let (from, to) = (src.join(rel), dir.join(rel));
            if !from.is_file() {
                continue; // a bare exe (an app's own build): its ConPTY stays
            }
            std::fs::create_dir_all(to.parent().unwrap())?;
            std::fs::copy(&from, aside(&to, "new"))?;
            done.push((aside(&to, "new"), to));
        }
        Ok(done)
    };
    let result = (|| {
        staged = stage()?;
        if !staged.first().is_some_and(|(s, _)| works(s)) {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        if hc_ours {
            std::fs::hard_link(&staged[0].0, aside(&hc, "new"))?;
            staged.push((aside(&hc, "new"), hc.clone()));
        }
        Ok(())
    })();
    let cleanup = |staged: &[(PathBuf, PathBuf)]| staged.iter().for_each(|(s, _)| drop(std::fs::remove_file(s)));
    if let Err(e) = result {
        cleanup(&staged);
        return Err(e);
    }

    // (final, moved-aside old) for each file swapped so far.
    let mut swapped: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for (new, to) in &staged {
        let old = to.exists().then(|| aside(to, "old"));
        let step = old.as_ref().map_or(Ok(()), |old| retry(|| std::fs::rename(to, old))).and_then(|_| {
            retry(|| std::fs::rename(new, to)).inspect_err(|_| {
                if let Some(old) = &old {
                    let _ = retry(|| std::fs::rename(old, to));
                }
            })
        });
        if let Err(e) = step {
            // Back as it was, with the same patience as going forward (a
            // scanner may be reading the file we just moved in). What can't
            // go back is said, and its old copy kept out of cleanup's reach.
            let mut stuck = Vec::new();
            for (to, old) in swapped.iter().rev() {
                let moved = retry(|| std::fs::rename(to, aside(to, "failed")));
                if let Some(old) = old
                    && (moved.is_err() || retry(|| std::fs::rename(old, to)).is_err())
                {
                    let keep = aside(to, "keep");
                    let _ = std::fs::rename(old, &keep);
                    stuck.push(format!("{} (the previous one is {})", to.display(), keep.display()));
                }
            }
            cleanup(&staged);
            if !stuck.is_empty() {
                return Err(io::Error::other(format!("{e}; and could not put back {}", stuck.join(", "))));
            }
            return Err(e);
        }
        swapped.push((to.clone(), old));
    }
    Ok(())
}

/// A file someone briefly holds (a scanner, an indexer): tried for 5 s.
fn retry<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let start = std::time::Instant::now();
    loop {
        match op() {
            // Access denied, sharing violation.
            Err(e)
                if matches!(e.raw_os_error(), Some(5 | 32)) && start.elapsed() < std::time::Duration::from_secs(5) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            other => return other,
        }
    }
}

pub fn file_identity(m: &std::fs::Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    format!("{}:{}:{}", m.creation_time(), m.last_write_time(), m.file_size())
}

// --- Processes -------------------------------------------------------------

/// Starts `cmd` with no console (no new process group: that would turn off
/// Ctrl+C for every program the holder starts), out of the job of
/// the terminal it was started from (ending that terminal won't end it),
/// stdin/stdout to NUL and stderr to `log`. It inherits only those two
/// handles: std's spawn hands down every inheritable one, and a pipe of an
/// ssh session living on in the daemon keeps that session from ending.
pub fn spawn_detached(cmd: Command, log: File) -> io::Result<()> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    for (k, v) in cmd.get_envs() {
        let k = k.to_string_lossy().into_owned();
        match v {
            Some(v) => env.insert(k, v.to_string_lossy().into_owned()),
            None => env.remove(&k),
        };
    }
    let parts: Vec<String> =
        std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(|a| quote(&a.to_string_lossy())).collect();
    let mut line = wide(parts.join(" ").as_ref());
    let mut block = env_block(&env);
    let nul = std::fs::OpenOptions::new().read(true).write(true).open("NUL")?;
    unsafe {
        let handles = [nul.as_raw_handle() as HANDLE, log.as_raw_handle() as HANDLE];
        for h in handles {
            check(SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT))?;
        }
        let mut n = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut n);
        let mut attrs = vec![0u8; n];
        let list = attrs.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        check(InitializeProcThreadAttributeList(list, 1, 0, &mut n))?;
        let set = UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of_val(&handles),
            null_mut(),
            null(),
        );
        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = list;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = handles[0];
        si.StartupInfo.hStdOutput = handles[0];
        si.StartupInfo.hStdError = handles[1];
        let mut pi: PROCESS_INFORMATION = zeroed();
        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS;
        let mut start = |flags| {
            CreateProcessW(
                null(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                flags,
                block.as_mut_ptr().cast(),
                null(),
                &si.StartupInfo,
                &mut pi,
            ) != 0
        };
        let mut started = set != 0 && start(flags | CREATE_BREAKAWAY_FROM_JOB);
        // A job that won't let us leave. Under ssh, one that kills on close
        // ends every terminal at logout: refuse. Elsewhere
        // (cargo, some launchers) stay in it and live as long as it does.
        if !started && set != 0 && GetLastError() == ERROR_ACCESS_DENIED {
            if std::env::var_os("SSH_CONNECTION").is_some() && job_kills_on_close() {
                DeleteProcThreadAttributeList(list);
                return Err(io::Error::other(
                    "this ssh session ends everything it started when you log out, and won't let hc leave: start hc once outside ssh (hc server start in a local terminal), then ssh in",
                ));
            }
            started = start(flags);
        }
        let err = io::Error::last_os_error();
        DeleteProcThreadAttributeList(list);
        if !started {
            return Err(err);
        }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    Ok(())
}

/// Whether the job we run in (if any) ends its processes when it closes.
fn job_kills_on_close() -> bool {
    unsafe {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        let ok = QueryInformationJobObject(
            null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            null_mut(),
        );
        ok != 0 && limits.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0
    }
}

pub fn parent_pid() -> u32 {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    unsafe {
        let me = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return 0;
        }
        let snap = OwnedHandle::from_raw_handle(snap as RawHandle);
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap.as_raw_handle() as HANDLE, &mut entry);
        while ok != 0 {
            if entry.th32ProcessID == me {
                return entry.th32ParentProcessID;
            }
            ok = Process32NextW(snap.as_raw_handle() as HANDLE, &mut entry);
        }
        0
    }
}

pub fn exit_signal(_: &std::process::ExitStatus) -> Option<i32> {
    None
}

pub fn random_bytes(bytes: &mut [u8]) {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
    let status =
        unsafe { BCryptGenRandom(null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert_eq!(status, 0, "BCryptGenRandom failed");
}

/// SHA-256 as lowercase hex, by Windows (no sha256sum there).
pub fn sha256(bytes: &[u8]) -> Option<String> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
    let mut out = [0u8; 32];
    let status = unsafe {
        BCryptHash(BCRYPT_SHA256_ALG_HANDLE, null(), 0, bytes.as_ptr(), bytes.len() as u32, out.as_mut_ptr(), 32)
    };
    (status == 0).then(|| out.iter().map(|b| format!("{b:02x}")).collect())
}

/// No SIGTERM: planned restarts come through the API.
pub async fn termination() {
    std::future::pending::<()>().await
}

pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "windows".into()).to_lowercase()
}

/// This system's own id (MachineGuid, new on a sysprepped clone). None when unreadable.
pub fn machine_id() -> Option<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("reg")
        .args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let line = text.lines().find(|l| l.contains("MachineGuid"))?;
    line.split_whitespace().last().map(String::from)
}

// --- Terminals: ConPTY ----------------------------------------------------

type CreatePc = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type ResizePc = unsafe extern "system" fn(HPCON, COORD) -> i32;
type ClosePc = unsafe extern "system" fn(HPCON);

struct ConPty {
    create: CreatePc,
    resize: ResizePc,
    close: ClosePc,
}

/// The ConPTY shipped beside hc (`conpty\conpty.dll`): Windows' own drops
/// input escape sequences agents answer with. Windows' own if none is there.
/// A broken or incompatible one fails each start, never the process.
fn conpty() -> io::Result<&'static ConPty> {
    use windows_sys::Win32::System::LibraryLoader::*;
    static API: std::sync::OnceLock<Result<ConPty, String>> = std::sync::OnceLock::new();
    let api = API.get_or_init(|| {
        let bundled = std::env::current_exe()
            .ok()
            .and_then(|e| Some(e.parent()?.join(r"conpty\conpty.dll")))
            .filter(|p| p.is_file());
        let from = bundled.as_ref().map_or_else(|| "kernel32.dll".into(), |p| p.display().to_string());
        // SAFETY: the names are NUL-terminated wide strings that outlive the
        // calls; a loaded module stays loaded for the life of the process.
        let module = unsafe {
            match &bundled {
                Some(dll) => LoadLibraryW(wide(dll.as_os_str()).as_ptr()),
                None => GetModuleHandleW(wide("kernel32.dll".as_ref()).as_ptr()),
            }
        };
        if module.is_null() {
            return Err(format!("cannot load ConPTY from {from}: {}", io::Error::last_os_error()));
        }
        type Proc = unsafe extern "system" fn() -> isize;
        // SAFETY: `module` is loaded; each name is a C string literal.
        let get = |name: &std::ffi::CStr| {
            unsafe { GetProcAddress(module, name.as_ptr().cast()) }.ok_or_else(|| format!("{from} has no {name:?}"))
        };
        // SAFETY: these exports have exactly these signatures (consoleapi.h).
        unsafe {
            Ok(ConPty {
                create: std::mem::transmute::<Proc, CreatePc>(get(c"CreatePseudoConsole")?),
                resize: std::mem::transmute::<Proc, ResizePc>(get(c"ResizePseudoConsole")?),
                close: std::mem::transmute::<Proc, ClosePc>(get(c"ClosePseudoConsole")?),
            })
        }
    });
    api.as_ref().map_err(|e| io::Error::other(e.clone()))
}

/// Closes once: when its program ended (so the last output arrives and its
/// reader ends), or when the terminal is let go.
struct PseudoConsole(std::sync::Mutex<Option<usize>>);

impl PseudoConsole {
    fn close(&self) {
        if let Some(hpc) = self.0.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // A console exists only if ConPTY loaded.
            if let Ok(api) = conpty() {
                unsafe { (api.close)(hpc as HPCON) };
            }
        }
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        self.close();
    }
}

/// A program on its own pseudo console, everything it starts in one job.
pub struct Pty {
    console: Arc<PseudoConsole>,
    input: File,
    output: File,
    job: OwnedHandle,
    pid: u32,
}

pub struct Process {
    pid: u32,
    handle: OwnedHandle,
    console: Arc<PseudoConsole>,
}

impl Process {
    pub fn id(&self) -> u32 {
        self.pid
    }
}

/// `program` as Windows starts it: found on PATH with PATHEXT, batch files
/// through cmd (npm installs agents as `.cmd` shims).
fn command_line(argv: &[String], env: &BTreeMap<String, String>) -> io::Result<Vec<u16>> {
    let program = argv.first().ok_or_else(|| io::Error::other("empty argv"))?;
    let path = env.iter().find(|(k, _)| k.eq_ignore_ascii_case("PATH")).map(|(_, v)| v.clone()).unwrap_or_default();
    let exts = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATHEXT"))
        .map_or(".COM;.EXE;.BAT;.CMD".into(), |(_, v)| v.clone());
    let found = resolve(program, &path, &exts)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{program}: not found on PATH")))?;
    let ext = found.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext == "cmd" || ext == "bat" {
        return batch_line(&found, &argv[1..]).map(|l| wide(l.as_ref()));
    }
    if super::is_shell_line(argv) {
        return Ok(wide(format!("{} /d /s /c \"{}\"", quote(&found.display().to_string()), argv[4]).as_ref()));
    }
    let line: Vec<String> =
        std::iter::once(found.display().to_string()).chain(argv[1..].iter().cloned()).map(|a| quote(&a)).collect();
    Ok(wide(line.join(" ").as_ref()))
}

/// A .cmd/.bat (how npm installs agents) runs through cmd, which reads
/// `& | < > ^ %` itself: each argument is quoted, `"` doubled and `%`
/// defused, so a prompt arrives as one literal argument (the scheme Rust's
/// std uses since BatBadBut). cmd can't carry a line break in an argument.
fn batch_line(script: &Path, args: &[String]) -> io::Result<String> {
    let mut line = format!("cmd.exe /e:ON /v:OFF /d /c \"\"{}\"", script.display());
    for arg in args {
        if arg.contains(['\r', '\n', '\0']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a batch file: it can't take an argument with a line break", script.display()),
            ));
        }
        let plain =
            |c: char| c.is_ascii_alphanumeric() || r"#$*+-./:?@\_".contains(c) || !c.is_ascii() && !c.is_control();
        let quoted = arg.is_empty() || arg.ends_with('\\') || !arg.chars().all(plain);
        line.push(' ');
        if quoted {
            line.push('"');
        }
        let mut slashes = 0;
        for c in arg.chars() {
            match c {
                '\\' => slashes += 1,
                '"' => {
                    line.push_str(&"\\".repeat(slashes));
                    line.push('"'); // doubled
                    slashes = 0;
                }
                // `%cd:~,%` expands to nothing, so `%VAR%` can't pair up.
                '%' => {
                    line.push_str("%%cd:~,");
                    slashes = 0;
                }
                _ => slashes = 0,
            }
            line.push(c);
        }
        if quoted {
            line.push_str(&"\\".repeat(slashes));
            line.push('"');
        }
    }
    line.push('"');
    Ok(line)
}

fn resolve(program: &str, path: &str, exts: &str) -> Option<PathBuf> {
    let with_exts = |base: PathBuf| -> Option<PathBuf> {
        if base.extension().is_some() && base.is_file() {
            return Some(base);
        }
        exts.split(';')
            .filter(|e| !e.is_empty())
            .map(|e| PathBuf::from(format!("{}{e}", base.display())))
            .find(|p| p.is_file())
    };
    let p = Path::new(program);
    if p.components().count() > 1 || p.is_absolute() {
        return with_exts(p.to_path_buf());
    }
    std::env::split_paths(path).find_map(|dir| with_exts(dir.join(program)))
}

/// One argument as Windows' command line parsing reads it back.
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.into();
    }
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            c => {
                out.push_str(&"\\".repeat(slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

fn env_block(env: &BTreeMap<String, String>) -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in env {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

pub fn spawn_pty(
    argv: &[String],
    env: &BTreeMap<String, String>,
    cwd: &str,
    cols: u16,
    rows: u16,
) -> io::Result<(Pty, Process)> {
    let mut cmd = command_line(argv, env)?;
    let mut block = env_block(env);
    let cwd = wide(cwd.as_ref());
    unsafe {
        // Ctrl+C ignored is inherited (an ssh session sets it): the program
        // inherits ours, so take Ctrl+C again.
        SetConsoleCtrlHandler(None, 0);
        let (mut in_r, mut in_w, mut out_r, mut out_w) = (null_mut(), null_mut(), null_mut(), null_mut());
        check(CreatePipe(&mut in_r, &mut in_w, null(), 0))?;
        check(CreatePipe(&mut out_r, &mut out_w, null(), 0))?;
        let (in_r, input, output, out_w) = (
            OwnedHandle::from_raw_handle(in_r as RawHandle),
            File::from_raw_handle(in_w as RawHandle),
            File::from_raw_handle(out_r as RawHandle),
            OwnedHandle::from_raw_handle(out_w as RawHandle),
        );
        let mut hpc: HPCON = 0;
        let hr = (conpty()?.create)(
            COORD { X: cols as i16, Y: rows as i16 },
            in_r.as_raw_handle() as HANDLE,
            out_w.as_raw_handle() as HANDLE,
            0,
            &mut hpc,
        );
        if hr != 0 {
            return Err(io::Error::other(format!("CreatePseudoConsole failed: {hr:#x}")));
        }
        let console = Arc::new(PseudoConsole(std::sync::Mutex::new(Some(hpc as usize))));

        // Everything it starts goes with it: killed when the job's last handle
        // closes (us, or the holder dying). Breakaway lets hc's own core and
        // holders, started from inside, outlive it.
        let job = CreateJobObjectW(null(), null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle::from_raw_handle(job as RawHandle);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        check(SetInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ))?;

        let mut n = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut n);
        let mut attrs = vec![0u8; n];
        let list = attrs.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        check(InitializeProcThreadAttributeList(list, 1, 0, &mut n))?;
        check(UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const c_void,
            size_of::<HPCON>(),
            null_mut(),
            null(),
        ))?;
        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = list;
        // Otherwise it inherits the holder's own stdio and writes there, not
        // into the pseudo console.
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
        let started = CreateProcessW(
            null(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            flags,
            block.as_mut_ptr().cast(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(list);
        check(started)?;
        let handle = OwnedHandle::from_raw_handle(pi.hProcess as RawHandle);
        let thread = OwnedHandle::from_raw_handle(pi.hThread as RawHandle);
        AssignProcessToJobObject(job.as_raw_handle() as HANDLE, handle.as_raw_handle() as HANDLE);
        ResumeThread(thread.as_raw_handle() as HANDLE);
        // The console holds its own ends now.
        drop((in_r, out_w));
        let process = Process { pid: pi.dwProcessId, handle, console: console.clone() };
        Ok((Pty { console, input, output, job, pid: pi.dwProcessId }, process))
    }
}

impl Pty {
    pub fn reader(&self) -> io::Result<File> {
        self.output.try_clone()
    }

    pub fn writer(&self) -> io::Result<File> {
        self.input.try_clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(hpc) = *self.console.0.lock().unwrap_or_else(|p| p.into_inner())
            && let Ok(api) = conpty()
        {
            unsafe { (api.resize)(hpc as HPCON, COORD { X: cols as i16, Y: rows as i16 }) };
        }
    }

    /// No process groups on a pseudo console: follow its unique shell child.
    pub fn foreground(&self) -> i32 {
        super::windows_process::shell_pid(self.pid) as i32
    }

    /// Ctrl+C through the console for an interrupt; anything harder ends the
    /// whole job (Windows has no hangup or terminate to ask with).
    pub fn signal(&self, _pid: u32, signal: i32) {
        if signal == super::signal::INT {
            let _ = (&self.input).write_all(b"\x03");
        } else {
            unsafe { TerminateJobObject(self.job.as_raw_handle() as HANDLE, 1) };
        }
    }

    pub fn hang_up(&self, _pid: u32) {
        unsafe { TerminateJobObject(self.job.as_raw_handle() as HANDLE, 1) };
    }
}

/// Waits for it to end: (exit code, no signal on Windows). Then closes its
/// console, so its last output arrives and the reader ends.
pub fn wait(child: &mut Process) -> io::Result<(Option<i32>, Option<i32>)> {
    unsafe {
        WaitForSingleObject(child.handle.as_raw_handle() as HANDLE, INFINITE);
        let mut code = 0u32;
        check(GetExitCodeProcess(child.handle.as_raw_handle() as HANDLE, &mut code))?;
        child.console.close();
        Ok((Some(code as i32), None))
    }
}

// --- The console (attach, picker) --------------------------------------------

pub mod console {
    use super::*;

    static TERMINATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe extern "system" fn on_ctrl(_: u32) -> BOOL {
        TERMINATED.store(true, std::sync::atomic::Ordering::SeqCst);
        1
    }

    /// Ctrl+Break, closing the window, logoff: a flag the input loop sees, so
    /// the console is given back.
    pub fn catch_termination() {
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
    }

    pub fn terminated() -> bool {
        TERMINATED.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn std(which: STD_HANDLE) -> HANDLE {
        unsafe { GetStdHandle(which) }
    }

    /// Keys as VT bytes in, VT sequences out, nothing cooked.
    pub struct RawMode(Option<(u32, u32, u32, u32)>);

    impl RawMode {
        pub fn enter() -> RawMode {
            unsafe {
                let (mut input, mut output) = (0u32, 0u32);
                if GetConsoleMode(std(STD_INPUT_HANDLE), &mut input) == 0
                    || GetConsoleMode(std(STD_OUTPUT_HANDLE), &mut output) == 0
                {
                    return RawMode(None);
                }
                let (cp_out, cp_in) = (GetConsoleOutputCP(), GetConsoleCP());
                let raw_in = (input & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT;
                SetConsoleMode(std(STD_INPUT_HANDLE), raw_in);
                SetConsoleMode(
                    std(STD_OUTPUT_HANDLE),
                    output | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN,
                );
                // Both ways: keys are read as bytes in the input code page,
                // and the program's console takes UTF-8.
                SetConsoleOutputCP(65001);
                SetConsoleCP(65001);
                RawMode(Some((input, output, cp_out, cp_in)))
            }
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            if let Some((input, output, cp_out, cp_in)) = self.0 {
                unsafe {
                    SetConsoleMode(std(STD_INPUT_HANDLE), input);
                    SetConsoleMode(std(STD_OUTPUT_HANDLE), output);
                    SetConsoleOutputCP(cp_out);
                    SetConsoleCP(cp_in);
                }
            }
        }
    }

    pub fn stdin() -> std::mem::ManuallyDrop<File> {
        std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(std(STD_INPUT_HANDLE) as RawHandle) })
    }

    pub fn stdout() -> std::mem::ManuallyDrop<File> {
        std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(std(STD_OUTPUT_HANDLE) as RawHandle) })
    }

    pub fn out(bytes: &[u8]) {
        let _ = stdout().write_all(bytes);
    }

    pub fn window_size() -> (u16, u16) {
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = zeroed();
            if GetConsoleScreenBufferInfo(std(STD_OUTPUT_HANDLE), &mut info) != 0 {
                let w = info.srWindow;
                return ((w.Right - w.Left + 1) as u16, (w.Bottom - w.Top + 1) as u16);
            }
        }
        (80, 24)
    }

    /// Waits up to `ms` for keys; returns what arrived (empty: nothing yet,
    /// None: input closed). Console events that aren't keys (focus, resize,
    /// mouse) wake the wait without bytes: those are taken off and skipped.
    pub fn read_stdin(ms: i32) -> Option<Vec<u8>> {
        unsafe {
            let h = std(STD_INPUT_HANDLE);
            if WaitForSingleObject(h, ms.max(0) as u32) != WAIT_OBJECT_0 {
                return Some(Vec::new());
            }
            let mut mode = 0u32;
            if GetConsoleMode(h, &mut mode) != 0 {
                let mut events: [INPUT_RECORD; 32] = zeroed();
                let mut n = 0u32;
                if PeekConsoleInputW(h, events.as_mut_ptr(), events.len() as u32, &mut n) == 0 {
                    return None;
                }
                // Shift, Ctrl, Alt, Win or a lock key alone type nothing, and
                // ReadFile would wait for a key that does. (Arrows count: VT
                // mode turns them into bytes as they are read.)
                let modifier = |vk: u16| matches!(vk, 0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0x90 | 0x91 | 0xA0..=0xA5);
                let key = events[..n as usize].iter().any(|e| {
                    e.EventType == KEY_EVENT as u16
                        && e.Event.KeyEvent.bKeyDown != 0
                        && !modifier(e.Event.KeyEvent.wVirtualKeyCode)
                });
                if !key {
                    ReadConsoleInputW(h, events.as_mut_ptr(), n, &mut n);
                    return Some(Vec::new());
                }
            }
            let mut buf = [0u8; 8192];
            let mut got = 0u32;
            if ReadFile(h, buf.as_mut_ptr(), buf.len() as u32, &mut got, null_mut()) == 0 || got == 0 {
                return None;
            }
            Some(buf[..got as usize].to_vec())
        }
    }
}
