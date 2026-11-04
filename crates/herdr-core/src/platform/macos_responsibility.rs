//! Core and holder processes own their privacy responsibility, not terminal programs.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

type Disclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;
const MARKER: &str = "_HC_RESPONSIBLE_PID";

pub fn holder_startup() {
    if std::env::args_os().nth(1).as_deref() != Some(OsStr::new("hold")) {
        return;
    }
    let marked = std::env::var(MARKER).ok() == Some(std::process::id().to_string());
    unsafe { std::env::remove_var(MARKER) };
    if marked {
        return;
    }
    let result = (|| {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(std::env::args_os().skip(1)).env(MARKER, std::process::id().to_string());
        exec(cmd)
    })();
    if let Err(error) = result {
        eprintln!("hc: cannot disclaim holder responsibility: {error}");
        std::process::exit(1);
    }
}

/// SETEXEC leaves no short-lived wrapper as the privacy subject. If the SPI is
/// absent, retain the previous exec behavior rather than preventing startup.
pub fn exec(cmd: Command) -> io::Result<()> {
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_spawnattrs_setdisclaim".as_ptr()) };
    let disclaim = (!symbol.is_null()).then(|| unsafe { std::mem::transmute::<*mut libc::c_void, Disclaim>(symbol) });
    exec_or_inherit(cmd, disclaim)
}

fn exec_or_inherit(mut cmd: Command, disclaim: Option<Disclaim>) -> io::Result<()> {
    match disclaim {
        Some(disclaim) => {
            let error = exec_with(&cmd, disclaim).unwrap_err();
            eprintln!("hc: responsibility disclaimer failed ({error}); using inherited responsibility");
        }
        None => eprintln!("hc: responsibility disclaimer unavailable; using inherited responsibility"),
    }
    Err(cmd.exec())
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}

fn exec_with(cmd: &Command, disclaim: Disclaim) -> io::Result<()> {
    let program = cstring(cmd.get_program())?;
    let argv: Vec<_> =
        std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(cstring).collect::<Result<_, _>>()?;
    let mut env: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => {
                env.insert(key.to_owned(), value.to_owned());
            }
            None => {
                env.remove(key);
            }
        }
    }
    let env: Vec<_> = env
        .into_iter()
        .map(|(key, value)| {
            let mut pair = key;
            pair.push("=");
            pair.push(value);
            cstring(&pair)
        })
        .collect::<Result<_, _>>()?;
    let pointers = |strings: &[CString]| {
        strings.iter().map(|s| s.as_ptr().cast_mut()).chain(std::iter::once(std::ptr::null_mut())).collect::<Vec<_>>()
    };
    let argv = pointers(&argv);
    let env = pointers(&env);
    let check = |code| if code == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(code)) };
    unsafe {
        let mut attrs = std::mem::zeroed();
        check(libc::posix_spawnattr_init(&mut attrs))?;
        let result = (|| {
            check(libc::posix_spawnattr_setflags(&mut attrs, libc::POSIX_SPAWN_SETEXEC as libc::c_short))?;
            check(disclaim(&mut attrs, 1))?;
            check(libc::posix_spawn(
                std::ptr::null_mut(),
                program.as_ptr(),
                std::ptr::null(),
                &attrs,
                argv.as_ptr(),
                env.as_ptr(),
            ))?;
            Err(io::Error::other("SETEXEC unexpectedly returned"))
        })();
        libc::posix_spawnattr_destroy(&mut attrs);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn refuse(_: *mut libc::posix_spawnattr_t, value: i32) -> i32 {
        assert_eq!(value, 1);
        libc::EPERM
    }

    #[test]
    fn disclaimer_failure_falls_back_to_plain_exec() {
        let error = exec_or_inherit(Command::new("/nonexistent/hc-disclaim-test"), Some(refuse)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn missing_spi_uses_the_previous_exec_path() {
        let error = exec_or_inherit(Command::new("/nonexistent/hc-disclaim-test"), None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn c_arguments_reject_embedded_nuls() {
        assert_eq!(cstring(OsStr::from_bytes(b"bad\0arg")).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
