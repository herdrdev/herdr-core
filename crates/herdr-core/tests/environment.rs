#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

/// A display whose server is gone is worse than none: a program that reaches
/// for the clipboard fails on it instead of using its own.
#[test]
fn a_program_gets_no_display_whose_server_is_gone() {
    let mut s = Session::new();
    s.env.push(("DISPLAY".into(), ":99999".into()));
    let out = s.hc(&["exec", "echo \"[${DISPLAY-unset}]\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[unset]\n", "the dead display was handed out");
}

/// A display on another machine is taken as it is: we cannot see it.
#[test]
fn a_display_on_another_machine_stays() {
    let mut s = Session::new();
    s.env.push(("DISPLAY".into(), "otherhost:0".into()));
    let out = s.hc(&["exec", "echo \"[${DISPLAY-unset}]\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[otherhost:0]\n");
}
