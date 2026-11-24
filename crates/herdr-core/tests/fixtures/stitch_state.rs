//! A real-PTY fixture: the first wheel changes its screen and makes scrolling unsafe.
use std::io::{Read, Write};

fn draw(wheels: usize, keys: &str) {
    print!("\x1b[H\x1b[2J");
    for row in 0..11 {
        let phase = if wheels == 0 { "ready" } else { "changed" };
        print!("\x1b[{};1H{phase} row {row}", row + 1);
    }
    print!("\x1b[12;1Hstatus wheels={wheels} keys={keys}");
    std::io::stdout().flush().unwrap();
}

fn main() {
    assert!(std::process::Command::new("stty").args(["raw", "-echo"]).status().unwrap().success());
    let state = std::env::args().nth(1).unwrap();
    print!("\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    let (mut wheels, mut keys, mut escape) = (0, String::new(), Vec::new());
    draw(wheels, &keys);
    let mut input = std::io::stdin().lock();
    let mut byte = [0];
    while input.read_exact(&mut byte).is_ok() {
        let b = byte[0];
        if b == 0x1b || !escape.is_empty() {
            escape.push(b);
            if b == b'M' {
                assert!(escape.starts_with(b"\x1b[<65;") || escape.starts_with(b"\x1b[<64;"));
                if wheels == 0 {
                    assert!(escape.starts_with(b"\x1b[<65;"), "the first probe must be one notch down");
                    match state.as_str() {
                        "no-mouse" => print!("\x1b[?1000l\x1b[?1006l"),
                        "normal-screen" => print!("\x1b[?1049l"),
                        _ => print!("\x1b]7501;state={state}\x1b\\"),
                    }
                }
                wheels += 1;
                escape.clear();
                draw(wheels, &keys);
            }
        } else {
            keys.push(b as char);
            draw(wheels, &keys);
        }
    }
}
