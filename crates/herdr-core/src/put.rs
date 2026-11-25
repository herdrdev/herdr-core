//! `terminal.put`: bytes into a file beside a terminal, for its program to read.

use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Raw bytes one put may carry (inline in one message).
pub const MAX_BYTES: usize = 8 << 20;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { B64[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text {
        acc = acc << 6 | B64.iter().position(|b| b == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Where a terminal's files go; removed with its record.
pub fn dir(state: &Path, id: u32) -> PathBuf {
    state.join("files").join(crate::terminals::address(id))
}

/// The basename, safe on any machine; `file` if nothing is left.
fn clean(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let safe: String =
        base.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let safe = safe.trim_start_matches('.').to_string();
    if safe.is_empty() { "file".into() } else { safe }
}

/// Writes `bytes` as `name` (or `name-2.ext`, ... when taken) beside terminal `id`.
pub fn stage(state: &Path, id: u32, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let dir = dir(state, id);
    std::fs::create_dir_all(&dir)?;
    land(&dir, name, |path| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
        f.write_all(bytes)
    })
}

/// Tries `name`, then `name-2.ext`, ... until `make` doesn't find it taken.
fn land(dir: &Path, name: &str, mut make: impl FnMut(&Path) -> io::Result<()>) -> io::Result<PathBuf> {
    let name = clean(name);
    let (stem, ext) = match name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (name.clone(), String::new()),
    };
    for n in 1.. {
        let path = dir.join(if n == 1 { name.clone() } else { format!("{stem}-{n}{ext}") });
        match make(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

// A put to another machine comes in pieces (`terminal.put_begin`, `_chunk`,
// `_finish`), each acknowledged, so typing there never waits behind a whole
// file. The pieces gather in a hidden part file beside where it will land.

/// Raw bytes one piece may carry.
pub const MAX_PIECE: usize = 1 << 20;
/// The first piece: small enough to cross a slow link in a fraction of a second.
pub const FIRST_PIECE: usize = 4 << 10;
/// Pieces in progress on one core at once.
pub const MAX_TRANSFERS: usize = 16;
pub const BUSY: &str = "too many puts at once here";
/// Aborts that overtook a begin still pending on one connection: a sender has
/// only a few puts at once, so more is a bug and ends the connection.
pub const MAX_EARLY_ABORTS: usize = 64;
/// How long one piece should hold a line: typing waits at most about this.
const PIECE_TIME: Duration = Duration::from_millis(350);
/// Part files start so; `clean` names never do.
const PART: &str = ".hc-put-";

pub fn valid_handle(handle: &str) -> bool {
    (16..=64).contains(&handle.len()) && handle.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A chunked put in progress, bound to the connection that began it.
pub struct Transfer {
    pub terminal: u32,
    pub conn: usize,
    pub name: String,
    pub part: PathBuf,
    pub total: usize,
    pub got: usize,
    pub writing: bool,
}

/// A new empty part file beside terminal `id`'s files.
pub fn open_part(state: &Path, id: u32, handle: &str) -> io::Result<PathBuf> {
    let dir = dir(state, id);
    std::fs::create_dir_all(&dir)?;
    let part = dir.join(format!("{PART}{handle}"));
    std::fs::OpenOptions::new().write(true).create_new(true).open(&part)?;
    Ok(part)
}

/// Writes a piece at `offset`; a part removed meanwhile is not made again.
pub fn write_at(part: &Path, offset: usize, bytes: &[u8]) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open(part)?;
    f.seek(io::SeekFrom::Start(offset as u64))?;
    f.write_all(bytes)
}

/// The finished part becomes `name` (or a free variant) without replacing anything.
pub fn publish(part: &Path, name: &str) -> io::Result<PathBuf> {
    publish_with_link(part, name, |from, to| std::fs::hard_link(from, to))
}

fn publish_with_link(
    part: &Path,
    name: &str,
    mut link: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let dir = part.parent().unwrap_or(Path::new("."));
    land(dir, name, |path| match link(part, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(part);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => crate::platform::rename_noreplace(part, path),
    })
}

/// Part files a previous core left (it stopped mid-transfer).
pub fn sweep(state: &Path) {
    let files = state.join("files");
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir() && !crate::platform::is_link(&m));
    if !real_dir(&files) {
        return;
    }
    for dir in std::fs::read_dir(files).into_iter().flatten().flatten() {
        if dir.file_name().to_str().and_then(crate::terminals::parse_id).is_none() || !real_dir(&dir.path()) {
            continue;
        }
        for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let name = file.file_name();
            if name.to_str().and_then(|n| n.strip_prefix(PART)).is_some_and(valid_handle)
                && file.metadata().is_ok_and(|m| m.is_file() && !crate::platform::is_link(&m))
            {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }
}

/// The next piece size: what the last one showed the link carries in about
/// a third of a second, at most twice the last. `base` is the quickest
/// answer seen (the link's round trip), so `took` minus it is sending time.
pub fn next_piece(size: usize, took: Duration, base: Duration) -> usize {
    let sending = took.saturating_sub(base).max(Duration::from_millis(1));
    let fits = size as f64 * PIECE_TIME.as_secs_f64() / sending.as_secs_f64();
    (fits as usize).clamp(FIRST_PIECE, (size * 2).min(MAX_PIECE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_and_names_are_safe() {
        for len in 0..10 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37) as u8).collect();
            assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(encode(b"hi!"), "aGkh");
        assert_eq!(decode("not base64!"), None);
        assert_eq!(clean("../../etc/pass wd.png"), "pass_wd.png");
        assert_eq!(clean(".."), "file");
    }

    #[test]
    fn a_finished_part_lands_beside_the_others_and_never_replaces_one() {
        let state = crate::test_tmp().join(format!("hc-put-{}", crate::platform::random_hex()));
        let first = stage(&state, 3, "shot.png", b"first").unwrap();
        let part = open_part(&state, 3, "abc").unwrap();
        write_at(&part, 0, b"sec").unwrap();
        write_at(&part, 3, b"ond").unwrap();
        let second = publish(&part, "shot.png").unwrap();
        assert_eq!(
            (std::fs::read(&first).unwrap(), std::fs::read(&second).unwrap()),
            (b"first".to_vec(), b"second".to_vec())
        );
        assert!(second.ends_with("shot-2.png") && !part.exists());
        assert!(write_at(&part, 0, b"late").is_err(), "a removed part is not made again");

        let left = open_part(&state, 3, "0abcdE0123456789").unwrap();
        let unrelated = open_part(&state, 3, "left").unwrap();
        sweep(&state);
        assert!(!left.exists() && unrelated.exists() && first.exists() && second.exists());
        std::fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn a_put_sweep_skips_links_in_the_staging_path() {
        let root = crate::test_tmp().join(format!("hc-put-{}", crate::platform::random_hex()));
        let state = root.join("state");
        let outside = root.join("outside");
        let part = open_part(&outside, 3, "abcdef0123456789").unwrap();
        std::fs::create_dir_all(&state).unwrap();
        link_dir(&outside.join("files"), &state.join("files"));
        sweep(&state);
        assert!(part.exists(), "files itself is a link");
        #[cfg(unix)]
        std::fs::remove_file(state.join("files")).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir(state.join("files")).unwrap();
        std::fs::create_dir(state.join("files")).unwrap();
        link_dir(part.parent().unwrap(), &dir(&state, 3));
        sweep(&state);
        assert!(part.exists(), "the terminal directory is a link");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    fn link_dir(from: &Path, to: &Path) {
        std::os::unix::fs::symlink(from, to).unwrap();
    }

    #[cfg(windows)]
    fn link_dir(from: &Path, to: &Path) {
        let from = std::path::absolute(from).unwrap();
        let to = std::path::absolute(to).unwrap();
        let out = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(to).arg(from).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    #[test]
    fn a_finished_part_can_land_where_hard_links_are_unavailable() {
        let state = crate::test_tmp().join(format!("hc-put-{}", crate::platform::random_hex()));
        let first = stage(&state, 3, "shot.png", b"first").unwrap();
        let abandoned = open_part(&state, 3, "0123456789abcdef").unwrap();
        write_at(&abandoned, 0, b"interrupted").unwrap();
        sweep(&state);
        assert!(!abandoned.exists() && !first.with_file_name("shot-2.png").exists());
        let part = open_part(&state, 3, "0123456789abcdef").unwrap();
        write_at(&part, 0, b"second").unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(unix)]
        let source = std::fs::File::open(&part).unwrap();
        let second = publish_with_link(&part, "shot.png", |_, _| Err(io::ErrorKind::Unsupported.into())).unwrap();
        #[cfg(unix)]
        assert_eq!(source.metadata().unwrap().ino(), std::fs::metadata(&second).unwrap().ino());
        assert_eq!(std::fs::read(first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        assert!(second.ends_with("shot-2.png") && !part.exists());
        std::fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn pieces_grow_on_a_quick_link_and_shrink_on_a_slow_one() {
        let ms = Duration::from_millis;
        let rtt = ms(150);
        let mut size = FIRST_PIECE;
        while size < MAX_PIECE {
            size = next_piece(size, rtt + ms(50), rtt);
        }
        assert_eq!(next_piece(size, rtt, rtt), MAX_PIECE, "never past the cap");
        assert_eq!(next_piece(size, rtt + ms(350), rtt), size, "steady at a third of a second");
        assert_eq!(next_piece(size, rtt + ms(700), rtt), size / 2, "smaller at once on a slower link");
        assert_eq!(next_piece(FIRST_PIECE, ms(5000), rtt), FIRST_PIECE, "never below the first");
    }
}
