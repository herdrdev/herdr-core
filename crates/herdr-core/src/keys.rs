//! Key names (`enter`, `up`, `ctrl+c`, `alt+x`, `f5`, `a`) → the bytes a real
//! terminal would send, using the modes the program set.

use crate::rpc::RpcError;
use ghostty_vt::Terminal as Screen;

/// GhosttyKey values (vendor/ghostty key/event.h), checked by the tests below.
fn key_code(name: &str) -> Option<(i32, char)> {
    let c = name.chars().next()?;
    let single = name.chars().count() == 1;
    Some(match name {
        _ if single && c.is_ascii_lowercase() => (20 + (c as i32 - 'a' as i32), c),
        _ if single && c.is_ascii_digit() => (6 + (c as i32 - '0' as i32), c),
        "`" => (1, '`'),
        "\\" | "backslash" => (2, '\\'),
        "[" => (3, '['),
        "]" => (4, ']'),
        "," => (5, ','),
        "=" => (16, '='),
        "-" => (46, '-'),
        "." => (47, '.'),
        "'" => (48, '\''),
        ";" => (49, ';'),
        "/" => (50, '/'),
        "backspace" => (53, '\0'),
        "enter" | "return" => (58, '\0'),
        "space" => (63, ' '),
        "tab" => (64, '\0'),
        "delete" | "del" => (68, '\0'),
        "end" => (69, '\0'),
        "home" => (71, '\0'),
        "insert" => (72, '\0'),
        "pagedown" | "pgdn" => (73, '\0'),
        "pageup" | "pgup" => (74, '\0'),
        "down" => (75, '\0'),
        "left" => (76, '\0'),
        "right" => (77, '\0'),
        "up" => (78, '\0'),
        "esc" | "escape" => (120, '\0'),
        _ => {
            let n: i32 = name.strip_prefix('f')?.parse().ok().filter(|n| (1..=12).contains(n))?;
            (120 + n, '\0')
        }
    })
}

/// Encodes one key name like `ctrl+shift+tab` for this screen.
pub fn encode(screen: &Screen, spec: &str) -> Result<Vec<u8>, RpcError> {
    let bad = || RpcError::invalid_params(format!("unknown key {spec:?} (try enter, esc, up, ctrl+c, alt+x, f5, a)"));
    let lower = spec.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    // `ctrl++` style ends in an empty part for the plus key itself; we don't support it.
    let name = parts.pop().filter(|n| !n.is_empty()).ok_or_else(bad)?;
    let mut mods = 0u16;
    for m in parts {
        mods |= match m {
            "shift" => 1,
            "ctrl" | "control" => 2,
            "alt" | "meta" | "opt" => 4,
            "super" | "cmd" => 8,
            _ => return Err(bad()),
        };
    }
    let (code, ch) = key_code(name).ok_or_else(bad)?;
    let text = match ch {
        '\0' => String::new(),
        c if mods & 1 != 0 => c.to_ascii_uppercase().to_string(),
        c => c.to_string(),
    };
    // Ctrl alone is a C0 byte. Shift must still be visible, including Alt+Shift.
    let bytes = screen.encode_key(code, mods, if mods & 2 != 0 && mods & 1 == 0 { "" } else { &text }, ch as u32);
    if bytes.is_empty() { Err(bad()) } else { Ok(bytes) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(screen: &Screen, names: &[&str]) -> Vec<String> {
        names.iter().map(|n| String::from_utf8_lossy(&encode(screen, n).unwrap()).into_owned()).collect()
    }

    #[test]
    fn names_encode_like_a_real_terminal_in_the_programs_modes() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        assert_eq!(
            keys(
                &screen,
                &["enter", "esc", "tab", "backspace", "a", "shift+a", "7", "ctrl+c", "alt+x", "up", "f1", "space"]
            ),
            ["\r", "\x1b", "\t", "\x7f", "a", "A", "7", "\x03", "\x1bx", "\x1b[A", "\x1bOP", " "]
        );
        assert_eq!(keys(&screen, &["alt+a", "alt+shift+a"]), ["\x1ba", "\x1bA"]);
        assert!(!encode(&screen, "ctrl+shift+c").unwrap().is_empty());
        // Application cursor keys (vim, less) change what arrows send.
        screen.write(b"\x1b[?1h");
        assert_eq!(keys(&screen, &["up", "left"]), ["\x1bOA", "\x1bOD"]);
        // A program that asked for the kitty keyboard gets kitty encodings.
        screen.write(b"\x1b[>1u");
        assert_eq!(keys(&screen, &["esc", "ctrl+backslash"]), ["\x1b[27u", "\x1b[92;5u"]);
        for bad in ["hello", "hyper+x", "f13", "ctrl+", ""] {
            assert!(encode(&screen, bad).is_err(), "{bad}");
        }
    }
}
