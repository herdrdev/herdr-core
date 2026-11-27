//! FreeBSD's process facts, straight from the kernel's sysctls (`kern.proc`).
//! The same permission checks Linux carries out for `/proc`'s copies apply:
//! a same-uid process or root may read; a setuid program escapes a sweep.
//!
//! The `kern.proc` sysctls take the operation code and, where one applies,
//! the pid: `[CTL_KERN, KERN_PROC, op, pid]` — libkvm's mib shape, not the
//! three-part one `sysctlbyname` names suggest.

/// One sysctl's value, retrying with slack if the kernel table grew (`ENOMEM`)
/// between sizing and reading.
fn sysctl_bytes(mib: &[libc::c_int]) -> Option<Vec<u8>> {
    for _ in 0..4 {
        let mut size: libc::size_t = 0;
        if unsafe { libc::sysctl(mib.as_ptr(), mib.len() as _, std::ptr::null_mut(), &mut size, std::ptr::null(), 0) }
            != 0
            || size == 0
        {
            return None;
        }
        let mut cap = size.saturating_add(size / 4).saturating_add(256);
        let mut buf = vec![0u8; cap];
        if unsafe { libc::sysctl(mib.as_ptr(), mib.len() as _, buf.as_mut_ptr().cast(), &mut cap, std::ptr::null(), 0) }
            == 0
        {
            buf.truncate(cap);
            return (!buf.is_empty()).then_some(buf);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return None;
        }
    }
    None
}

/// `pid`'s `kinfo_proc` record, as the kernel has it.
fn kinfo(pid: i32) -> Option<libc::kinfo_proc> {
    let bytes = sysctl_bytes(&[libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid])?;
    if bytes.len() < std::mem::size_of::<libc::kinfo_proc>() {
        return None;
    }
    // The kernel's records and this build's libc struct come from the same
    // OS version's headers (FreeBSD 14+).
    Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const libc::kinfo_proc) })
}

/// Every live process as (pid, parent pid, start in wall-clock microseconds);
/// zombies left out, like the `/proc` walk on Linux.
pub(crate) fn processes() -> Vec<(i32, i32, u64)> {
    let bytes = sysctl_bytes(&[libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL]).unwrap_or_default();
    if bytes.len() < std::mem::size_of::<libc::kinfo_proc>() {
        return Vec::new();
    }
    let stride = std::mem::size_of::<libc::kinfo_proc>();
    // The first record carries the kernel's record size: a newer FreeBSD's
    // records are walked by it, while the leading fields every 14+ version
    // shares are read the same.
    let struct_size = unsafe { (bytes.as_ptr() as *const libc::kinfo_proc).read_unaligned().ki_structsize };
    let stride = (struct_size as usize).max(stride);
    bytes[..bytes.len() / stride * stride]
        .chunks_exact(stride)
        .filter_map(|entry| {
            let record = unsafe { std::ptr::read_unaligned(entry.as_ptr() as *const libc::kinfo_proc) };
            if record.ki_pid <= 0 || record.ki_stat == libc::SZOMB {
                return None;
            }
            let started = record.ki_start.tv_sec.max(0) as u64 * 1_000_000 + record.ki_start.tv_usec.max(0) as u64;
            Some((record.ki_pid, record.ki_ppid, started))
        })
        .collect()
}

/// NUL-separated `KEY=value` entries; a zombie has none.
pub(crate) fn environ(pid: i32) -> Option<Vec<u8>> {
    sysctl_bytes(&[libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ENV, pid])
}

/// The process's name as the kernel has it (`ki_comm`, like Linux's `/proc/comm`).
fn name(pid: i32) -> String {
    kinfo(pid)
        .map(|record| {
            let bytes: Vec<u8> = record.ki_comm.iter().take_while(|b| **b != 0).map(|b| *b as u8).collect();
            String::from_utf8_lossy(&bytes).trim_end().to_owned()
        })
        .unwrap_or_default()
}

/// The facts about `pid`, if it is still there to read: its command line
/// (`kern.proc.args`), working folder (`kern.proc.cwd`), and name.
pub(crate) fn about(pid: i32) -> Option<crate::foreground::Foreground> {
    // `kern.proc.cwd` hands back a `kinfo_file`, the folder in its
    // `kf_path` — the same FreeBSD 14+ sysctl `procstat` reads.
    let cwd = sysctl_bytes(&[libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_CWD, pid])?;
    let path = cwd.get(std::mem::offset_of!(libc::kinfo_file, kf_path)..)?;
    let path = String::from_utf8_lossy(&path[..path.iter().position(|b| *b == 0).unwrap_or(path.len())]);
    let args = sysctl_bytes(&[libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ARGS, pid])?;
    let argv: Vec<String> = args
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let name = name(pid);
    let cwd = path.into_owned();
    (!argv.is_empty()).then_some(crate::foreground::Foreground { pid, name, argv, cwd })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_finds_self_and_excludes_zombies() {
        let me = std::process::id() as i32;
        assert!(processes().iter().any(|(pid, ..)| *pid == me));

        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe { libc::_exit(0) };
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while kinfo(child).is_some_and(|r| r.ki_stat != libc::SZOMB) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!processes().iter().any(|(pid, ..)| *pid == child), "zombie excluded");
        unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) };
    }

    #[test]
    fn about_reads_self_cwd_and_arguments() {
        let me = std::process::id() as i32;
        let fg = about(me).expect("self foreground readable");
        assert_eq!(fg.pid, me);
        assert!(!fg.name.is_empty());
        assert!(!fg.argv.is_empty());
        assert_eq!(
            std::path::PathBuf::from(&fg.cwd).canonicalize().unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn boot_and_machine_identities_are_well_formed() {
        let boot = crate::platform::boot_id().expect("kern.boottime readable").expect("non-empty boot id");
        assert!(!boot.is_empty() && !boot.contains('\0'));
        if let Some(id) = crate::platform::machine_id() {
            assert!(!id.is_empty() && !id.contains('\0'));
            assert_ne!(id, "00000000-0000-0000-0000-000000000000");
        }
    }
}
