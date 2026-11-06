//! `hc attach`: puts this terminal on a core terminal. Raw bytes both ways;
//! the only key we look at is the detach chord.

use crate::client;
use crate::config::DetachKey;
use crate::paths::Paths;
use crate::rpc::RpcError;
use crate::viewers::Display;
use crate::wire;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Ask the outer terminal for its colors and cell size; DA1 marks the end.
const ASK_DISPLAY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]12;?\x1b\\\x1b[16t\x1b[c";
/// Undo what the program may have changed in the outer terminal, checked
/// against Ghostty's mode list: synchronized output first (it freezes the
/// screen), kitty key flags on both screens (each screen keeps its own), then
/// modes back to their defaults, scroll region, cursor, charset, colors.
/// Origin mode and the scroll region move the cursor home, so the position
/// is saved around them: the next prompt follows the program's last line.
const RESET: &[u8] = b"\x1b[?2026l\x1b[?1049h\x1b[=0;1u\x1b[?1049l\x1b[?1047l\x1b[?47l\x1b[=0;1u\x1b[>4;0m\
\x1b7\x1b[4l\x1b[20l\x1b[?1l\x1b[?5l\x1b[?6l\x1b[?66l\x1b[?67l\x1b[?69l\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\
\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?2031l\x1b[?2033l\
\x1b[?2048l\x1b[?7h\x1b[?25h\x1b[?1007h\x1b[r\x1b8\x1b[0 q\x1b(B\x1b[0m\
\x1b]104\x1b\\\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\\x1b]22;\x1b\\\r\n";

/// Where the detach chord starts in `input`: as a control byte, a kitty
/// `CSI code;M[:E]u` (press or repeat, lock keys ignored), or modifyOtherKeys
/// `CSI 27;M;code~`.
fn find_detach(input: &[u8], key: DetachKey) -> Option<usize> {
    let code = key.code.to_string();
    let ctrl_only = |mods: &str| mods.parse::<u32>().is_ok_and(|m| m >= 1 && (m - 1) & !(64 | 128) == 4);
    (0..input.len()).find(|&i| {
        if input[i] == key.byte {
            return true;
        }
        let Some(rest) = input[i..].strip_prefix(b"\x1b[") else { return false };
        let Some(end) = rest.iter().position(|b| (0x40..=0x7e).contains(b)) else { return false };
        let Ok(params) = std::str::from_utf8(&rest[..end]) else { return false };
        let fields: Vec<&str> = params.split(';').collect();
        match (rest[end], fields.as_slice()) {
            (b'u', [key, mods, ..]) => {
                let (mods, event) = mods.split_once(':').unwrap_or((mods, "1"));
                key.split(':').next() == Some(code.as_str()) && ctrl_only(mods) && event != "3"
            }
            (b'~', ["27", mods, key]) => *key == code && ctrl_only(mods),
            _ => false,
        }
    })
}

pub(crate) use crate::platform::console::{RawMode, catch_termination, out, read_stdin, terminated, window_size};

/// Where a trailing escape sequence that hasn't fully arrived starts
/// (`input.len()` when none): reads don't keep sequences whole.
pub(crate) fn incomplete_tail(input: &[u8]) -> usize {
    let mut at = 0;
    while let Some(found) = input[at..].iter().position(|b| *b == 0x1b) {
        let esc = at + found;
        let rest = &input[esc + 1..];
        // How long the sequence after ESC is, if it is all there.
        let len = match rest.first() {
            None => None,
            // CSI: parameters, then one final byte.
            Some(b'[') => rest[1..].iter().position(|b| (0x40..=0x7e).contains(b)).map(|p| p + 2),
            // OSC, DCS, APC: until BEL or ST; a runaway one is let through
            // rather than held forever.
            Some(b']' | b'P' | b'_') => (1..rest.len())
                .find_map(|j| match rest[j] {
                    0x07 => Some(j + 1),
                    0x1b if rest.get(j + 1) == Some(&b'\\') => Some(j + 2),
                    _ => None,
                })
                .or((rest.len() > 512).then_some(rest.len())),
            Some(b'O') => (rest.len() >= 2).then_some(2),
            Some(_) => Some(1),
        };
        match len {
            Some(n) => at = esc + 1 + n,
            None => return esc,
        }
    }
    input.len()
}

/// Program output as the outer terminal gets it. Focus reports are ours
/// (they pick the active viewer; the core passes them on only if the program
/// asked), so a repaint or program turning them off turns them back on.
fn shown(payload: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    use std::borrow::Cow;
    const OFF: &[u8] = b"\x1b[?1004l";
    let mut out = Cow::Borrowed(payload);
    // ConPTY asks its viewer for win32-input-mode. A Windows console hosting
    // hc would obey and send keys as key-event codes: no detach key, no replies.
    for mode in [&b"\x1b[?9001h"[..], b"\x1b[?9001l"] {
        while let Some(at) = out.windows(mode.len()).position(|w| w == mode) {
            out.to_mut().drain(at..at + mode.len());
        }
    }
    // Turned back on where it was turned off, not at the end: a payload can
    // end partway through a character that the next payload finishes.
    while let Some(at) = out.windows(OFF.len()).position(|w| w == OFF) {
        out.to_mut()[at + OFF.len() - 1] = b'h';
    }
    out
}

/// Pulls replies to `ASK_DISPLAY` out of input. Returns the rest and whether DA1 arrived.
fn take_replies(input: &[u8], facts: &mut Display) -> (Vec<u8>, bool) {
    let mut rest = Vec::with_capacity(input.len());
    let mut done = false;
    let mut i = 0;
    while i < input.len() {
        let tail = &input[i..];
        let osc = [b"\x1b]10;", b"\x1b]11;", b"\x1b]12;"].iter().position(|p| tail.starts_with(*p));
        if let Some(which) = osc {
            let end = tail.windows(2).position(|w| w == b"\x1b\\").map(|p| (p, p + 2));
            let end = end.or_else(|| tail.iter().position(|b| *b == 0x07).map(|p| (p, p + 1)));
            if let Some((body_end, len)) = end {
                let color = parse_rgb(&tail[5..body_end]);
                match which {
                    0 => facts.fg = color,
                    1 => facts.bg = color,
                    _ => facts.cursor = color,
                }
                i += len;
                continue;
            }
        }
        if let Some(body) = tail.strip_prefix(b"\x1b[6;")
            && let Some(t) = body.iter().position(|b| *b == b't')
        {
            let nums: Vec<u32> =
                std::str::from_utf8(&body[..t]).unwrap_or("").split(';').filter_map(|n| n.parse().ok()).collect();
            if let [h, w] = nums[..] {
                facts.cell = Some([w, h]);
            }
            i += 4 + t + 1;
            continue;
        }
        if let Some(body) = tail.strip_prefix(b"\x1b[?")
            && let Some(c) = body.iter().position(|b| !b.is_ascii_digit() && *b != b';')
            && body[c] == b'c'
        {
            done = true;
            i += 3 + c + 1;
            continue;
        }
        rest.push(input[i]);
        i += 1;
    }
    (rest, done)
}

/// `rgb:RRRR/GGGG/BBBB` (any hex width) → `#rrggbb`.
fn parse_rgb(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?.strip_prefix("rgb:")?;
    let parts: Vec<&str> = s.split('/').collect();
    let [r, g, b] = parts[..] else { return None };
    let byte = |p: &str| u8::from_str_radix(p.get(..2)?, 16).ok();
    Some(format!("#{:02x}{:02x}{:02x}", byte(r)?, byte(g)?, byte(b)?))
}

pub fn run(paths: &Paths, terminal: &str, observe: bool, history: usize) -> Result<String, RpcError> {
    use std::io::IsTerminal;
    let detach_key = crate::config::load(&paths.config)?.detach_key()?;
    let tty = std::io::stdin().is_terminal();
    if !tty && !observe {
        return Err(RpcError::new("usage", "attach needs a terminal", "use --observe to watch from a pipe"));
    }
    let _raw = tty.then(RawMode::enter);
    catch_termination();
    let mut facts = Display::default();
    let mut early = Vec::new();
    if tty {
        out(ASK_DISPLAY);
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut carry = Vec::new();
        while Instant::now() < deadline {
            let Some(chunk) = read_stdin(50) else { break };
            carry.extend(chunk);
            let tail = carry.split_off(incomplete_tail(&carry)); // a reply may arrive in pieces
            let (rest, done) = take_replies(&carry, &mut facts);
            early.extend(rest);
            carry = tail;
            if done {
                break;
            }
        }
        early.extend(carry);
    }

    let stream = client::connect_or_start(paths)?;
    let lost = |e: std::io::Error| RpcError::new("unavailable", format!("lost the core: {e}"), "");
    let mut reader = BufReader::new(stream.try_clone().map_err(lost)?);
    (&stream)
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\",\"params\":{\"framed\":true}}\n")
        .map_err(lost)?;
    reader.read_line(&mut String::new()).map_err(lost)?;
    let writer = Arc::new(Mutex::new(stream));
    let send = {
        let writer = writer.clone();
        move |channel: u32, kind: u8, payload: &[u8]| {
            let _ = writer.lock().unwrap_or_else(|p| p.into_inner()).write_all(&wire::encode(channel, kind, payload));
        }
    };

    let (cols, rows) = window_size();
    let caller = crate::client::terminal_id();
    let params = json!({"terminal": terminal, "observe": observe, "history": history, "cols": cols, "rows": rows, "display": facts, "caller": caller});
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "terminal.attach", "params": params});
    send(0, wire::DATA, req.to_string().as_bytes());
    let mut early_frames = Vec::new();
    let channel = loop {
        let (ch, kind, payload) =
            read_frame(&mut reader).ok_or_else(|| lost(std::io::ErrorKind::UnexpectedEof.into()))?;
        if ch == 0 && kind == wire::DATA {
            let reply: Value = serde_json::from_slice(&payload).unwrap_or_default();
            if let Some(err) = reply.get("error") {
                return Err(RpcError::from_json(err));
            }
            break reply["result"]["channel"].as_u64().unwrap_or(0) as u32;
        }
        early_frames.push((ch, kind, payload));
    };
    let address = terminal.to_string();
    if tty {
        out(b"\x1b[?1004h"); // focus reports: switching back to this window makes it the active viewer
    }

    let done = Arc::new(AtomicBool::new(false));
    let ending = Arc::new(Mutex::new(None::<Value>));
    let output = {
        let (done, ending, send) = (done.clone(), ending.clone(), send.clone());
        std::thread::spawn(move || {
            let mut frames =
                early_frames.into_iter().map(Some).chain(std::iter::from_fn(|| Some(read_frame(&mut reader))));
            while let Some(Some((ch, kind, payload))) = frames.next() {
                match (ch == channel, kind) {
                    (true, wire::DATA) => {
                        out(&shown(&payload));
                        send(channel, wire::CREDIT, &(payload.len() as u32).to_le_bytes());
                    }
                    (true, wire::CLOSE) => {
                        *ending.lock().unwrap_or_else(|p| p.into_inner()) = serde_json::from_slice(&payload).ok();
                        break;
                    }
                    _ => {}
                }
            }
            done.store(true, Ordering::SeqCst);
        })
    };

    let mut size = (cols, rows);
    let mut detached = !tty; // a piped observer ends when its input does
    let mut asking: Option<Instant> = None;
    let mut sent = facts.clone();
    let (mut pending, mut carry) = (early, Vec::new());
    while !done.load(Ordering::SeqCst) {
        // A sequence cut off at the end of a read waits for its rest; nothing
        // more within 50 ms (tmux's escape-time) means it was a lone Esc after all.
        let wait = if carry.is_empty() { 100 } else { 50 };
        let mut input = std::mem::take(&mut carry);
        input.extend(std::mem::take(&mut pending));
        match read_stdin(wait) {
            Some(chunk) if chunk.is_empty() => {}
            Some(chunk) => {
                input.extend(chunk);
                carry = input.split_off(incomplete_tail(&input));
            }
            None if tty => {
                detached = true; // the window closed
                break;
            }
            None => {}
        }
        if terminated() {
            detached = true;
            break;
        }
        // Replies are ours even when late: a Windows console answers DA1 at
        // once but passes the rest on to the real terminal.
        let (rest, finished) = take_replies(&input, &mut facts);
        input = rest;
        if asking.is_some_and(|since| finished || since.elapsed() > Duration::from_millis(500)) {
            asking = None;
        }
        if asking.is_none() && facts != sent {
            send(channel, wire::FACTS, serde_json::to_string(&facts).unwrap_or_default().as_bytes());
            sent = facts.clone();
        }
        let detach = find_detach(&input, detach_key);
        if let Some(at) = detach {
            input.truncate(at);
        }
        for (seq, focused) in [(&b"\x1b[I"[..], true), (&b"\x1b[O"[..], false)] {
            while let Some(at) = input.windows(3).position(|w| w == seq) {
                input.drain(at..at + 3);
                send(channel, wire::FOCUS, &[focused as u8]);
                if focused && asking.is_none() {
                    out(ASK_DISPLAY); // the theme may have changed while we were away
                    asking = Some(Instant::now());
                }
            }
        }
        if !observe && !input.is_empty() {
            send(channel, wire::DATA, &input);
        }
        if detach.is_some() {
            detached = true;
            send(channel, wire::CLOSE, &[]);
            break;
        }
        let now = window_size();
        if now != size && !observe {
            size = now;
            let mut p = now.0.to_le_bytes().to_vec();
            p.extend(now.1.to_le_bytes());
            send(channel, wire::RESIZE, &p);
        }
    }
    if let Ok(w) = writer.lock() {
        let _ = w.shutdown(std::net::Shutdown::Both);
    }
    let _ = output.join();
    if tty {
        out(RESET);
    }
    let ending = ending.lock().unwrap_or_else(|p| p.into_inner()).take();
    Ok(match ending {
        Some(end) => format!("[{address} finished: {}]", end["reason"].as_str().unwrap_or("ended")),
        None if detached => format!("[detached from {address}]"),
        None => format!("[lost the core; it keeps {address} running: hc attach {address}]"),
    })
}

fn read_frame(r: &mut impl Read) -> Option<(u32, u8, Vec<u8>)> {
    let mut header = [0u8; wire::HEADER];
    r.read_exact(&mut header).ok()?;
    let (channel, kind, len) = wire::decode_header(&header).ok()?;
    let mut payload = vec![0; len];
    r.read_exact(&mut payload).ok()?;
    Some((channel, kind, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sequence_cut_off_at_the_end_of_a_read_is_held_back() {
        assert_eq!(incomplete_tail(b"ab\x1b[92;"), 2, "a key half there");
        assert_eq!(incomplete_tail(b"ab\x1b]11;rgb:ff"), 2, "a color reply half there");
        assert_eq!(incomplete_tail(b"ab\x1b]11;rgb:ff\x1b"), 2, "cut inside its terminator");
        assert_eq!(incomplete_tail(b"\x1b[Aab\x1b[1;5"), 5, "after a whole one");
        assert_eq!(incomplete_tail(b"ab\x1b"), 2, "maybe a lone Esc: decided by the clock");
        assert_eq!(incomplete_tail(b"ab\x1b[A"), 5);
        assert_eq!(incomplete_tail(b"\x1b]11;rgb:ff\x1b\\x"), 14, "ended by ST");
        assert_eq!(incomplete_tail(b"\x1bx"), 2, "alt+x");
    }

    #[test]
    fn detach_leaves_no_key_mode_behind_on_either_screen() {
        let mut t = ghostty_vt::Terminal::new(80, 24, 0);
        // A full-screen agent turned on kitty keys (with release events) and app cursor keys.
        t.write(b"\x1b[?1049h\x1b[>11u\x1b[?1h");
        t.write(RESET);
        assert_eq!(crate::keys::encode(&t, "down").unwrap(), b"\x1b[B");
        t.write(b"\x1b[?1049h"); // the next full-screen program, like the picker
        assert_eq!(crate::keys::encode(&t, "down").unwrap(), b"\x1b[B");
    }

    #[test]
    fn after_detach_the_next_text_follows_the_last_line_not_the_top() {
        let row_of_at = |app: &[u8]| {
            let mut t = ghostty_vt::Terminal::new(80, 24, 0);
            t.write(b"\x1b[8;1Hlast line> ");
            t.write(app);
            t.write(RESET);
            t.write(b"@");
            t.screen_rows().iter().position(|r| r.starts_with('@'))
        };
        assert_eq!(row_of_at(b""), Some(8), "a main-screen program");
        assert_eq!(row_of_at(b"\x1b[3;20r\x1b[8;13H"), Some(8), "one that left a scroll region");
        assert_eq!(row_of_at(b"\x1b[?1049h\x1b[2;3Hfull screen"), Some(8), "a full-screen program");
    }

    #[test]
    fn focus_reports_stay_on_whatever_the_program_or_a_repaint_says() {
        let mut t = ghostty_vt::Terminal::new(80, 24, 0);
        t.write(b"\x1b[?1004h");
        t.write(&shown(&ghostty_vt::Terminal::new(80, 24, 0).paint(0)));
        assert!(t.dec_mode(1004), "after first paint");
        t.write(&shown(b"hi\x1b[?1004l there"));
        assert!(t.dec_mode(1004), "after the program turned them off");
    }

    #[test]
    fn a_character_cut_off_by_the_first_paint_joins_the_bytes_after_it() {
        // 上 is E4 B8 8A; the program has printed only E4 when the viewer attaches.
        let mut screen = ghostty_vt::Terminal::new(80, 24, 0);
        screen.write(b"A \xe4");
        let mut first = screen.paint(0);
        first.extend(screen.continuation().unwrap());
        let mut t = ghostty_vt::Terminal::new(80, 24, 0);
        t.write(&shown(&first));
        t.write(&shown(b"\xb8\x8a B"));
        assert_eq!(t.plain_text().trim_end(), "A 上 B");
        assert!(t.dec_mode(1004));
    }

    #[test]
    fn conpty_never_switches_our_keys_to_win32_input_mode() {
        assert_eq!(&*shown(b"\x1b[?9001h\x1b[?1004hhi\x1b[?9001l"), b"\x1b[?1004hhi");
    }
    #[test]
    fn detach_chord_is_found_in_every_keyboard_encoding() {
        for input in [
            &b"ab\x1c"[..],
            b"ab\x1b[92;5u",
            b"ab\x1b[92;5:1u",
            b"ab\x1b[92;69u",
            b"ab\x1b[92;133:2u",
            b"ab\x1b[27;5;92~",
        ] {
            assert_eq!(find_detach(input, BACKSLASH), Some(2), "{:?}", String::from_utf8_lossy(input));
        }
        for input in [&b"\x1b[92;5:3u"[..], b"\x1b[92;7u", b"\x1b[93;5u", b"\x1b[92u", b"\\"] {
            assert_eq!(find_detach(input, BACKSLASH), None, "{:?}", String::from_utf8_lossy(input));
        }
    }

    const BACKSLASH: DetachKey = DetachKey { byte: 0x1c, code: 92 };

    #[test]
    fn display_replies_are_taken_out_of_typed_input() {
        let mut facts = Display::default();
        let input = b"a\x1b]11;rgb:ffff/8080/0000\x1b\\b\x1b]10;rgb:11/22/33\x07\x1b[6;18;9t\x1b[?62;22cc";
        let (rest, done) = take_replies(input, &mut facts);
        assert_eq!(rest, b"abc");
        assert!(done);
        assert_eq!(facts.bg.as_deref(), Some("#ff8000"));
        assert_eq!(facts.fg.as_deref(), Some("#112233"));
        assert_eq!(facts.cell, Some([9, 18]));
    }
}
