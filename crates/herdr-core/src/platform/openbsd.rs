//! Process facts through sysctl: OpenBSD has no procfs.

use std::mem::size_of;

fn read(mib: &mut [i32]) -> Option<Vec<u8>> {
    for _ in 0..4 {
        let mut size = 0;
        if unsafe {
            libc::sysctl(mib.as_mut_ptr(), mib.len() as u32, std::ptr::null_mut(), &mut size, std::ptr::null_mut(), 0)
        } != 0
        {
            return None;
        }
        let mut buf = vec![0u8; size + size / 4 + 1024];
        size = buf.len();
        if mib.get(1) == Some(&libc::KERN_PROC) {
            mib[5] = (size / size_of::<libc::kinfo_proc>()) as i32;
        }
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 {
            buf.truncate(size);
            return Some(buf);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return None;
        }
    }
    None
}

fn info(kind: i32, pid: i32) -> Vec<libc::kinfo_proc> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, kind, pid, size_of::<libc::kinfo_proc>() as i32, 0];
    read(&mut mib)
        .unwrap_or_default()
        .chunks_exact(size_of::<libc::kinfo_proc>())
        .map(|bytes| unsafe { bytes.as_ptr().cast::<libc::kinfo_proc>().read_unaligned() })
        .collect()
}

fn strings(pid: i32, kind: i32) -> Option<Vec<Vec<u8>>> {
    let mut buf = read(&mut [libc::CTL_KERN, libc::KERN_PROC_ARGS, pid, kind])?;
    let base = buf.as_mut_ptr() as usize;
    let mut result = Vec::new();
    for bytes in buf.chunks_exact(size_of::<usize>()) {
        let address = usize::from_ne_bytes(bytes.try_into().ok()?);
        if address == 0 {
            return Some(result);
        }
        let offset = address.checked_sub(base)?;
        let tail = buf.get(offset..)?;
        let end = tail.iter().position(|&b| b == 0)?;
        result.push(tail[..end].to_vec());
    }
    None
}

pub fn process_environ(pid: i32) -> Option<Vec<u8>> {
    let mut env = Vec::new();
    for var in strings(pid, libc::KERN_PROC_ENV)? {
        env.extend(var);
        env.push(0);
    }
    Some(env)
}

pub fn processes() -> Vec<(i32, i32, u64)> {
    info(libc::KERN_PROC_ALL, 0).into_iter().filter(|p| p.p_stat != 5).map(|p| (p.p_pid, p.p_ppid, 0)).collect()
}

pub fn process_about(pid: i32) -> Option<crate::foreground::Foreground> {
    let p = info(libc::KERN_PROC_PID, pid).into_iter().next()?;
    let name = p.p_comm.iter().take_while(|&&b| b != 0).map(|&b| b as u8).collect::<Vec<_>>();
    let argv =
        strings(pid, libc::KERN_PROC_ARGV)?.into_iter().map(|s| String::from_utf8_lossy(&s).into_owned()).collect();
    let cwd = read(&mut [libc::CTL_KERN, libc::KERN_PROC_CWD, pid])?;
    let cwd = cwd.split(|&b| b == 0).next()?;
    Some(crate::foreground::Foreground {
        pid,
        name: String::from_utf8_lossy(&name).into_owned(),
        argv,
        cwd: String::from_utf8_lossy(cwd).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_current_process_without_procfs() {
        let pid = std::process::id() as i32;
        let about = process_about(pid).unwrap();
        assert_eq!(about.pid, pid);
        assert!(!about.name.is_empty());
        assert!(!about.argv.is_empty());
        assert_eq!(std::path::Path::new(&about.cwd), std::env::current_dir().unwrap());
        assert!(processes().iter().any(|p| p.0 == pid));
        assert!(process_environ(pid).is_some());
    }
}
