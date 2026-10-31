use super::*;

#[test]
fn display_changes_report_to_subscribers_and_queries_use_the_latest_facts() {
    let mut t = Terminal::new(80, 24, 0);
    t.write(b"\x1b[?2031h");
    t.take_responses(); // Enabling reporting may itself report the current scheme.
    t.set_display(Some([1, 2, 3]), Some([255; 3]), Some([4, 5, 6]), Some((9, 18)));
    assert_eq!(t.take_responses(), b"\x1b[?997;2n", "dark to light");
    t.set_display(None, Some([0; 3]), None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "light to dark");

    // Mode 2031 also tells subscribers to re-query colors within the same scheme.
    t.set_display(Some([7, 8, 9]), None, None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "foreground changed");
    t.set_display(None, None, Some([10, 11, 12]), None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "cursor changed");
    t.set_display(Some([7, 8, 9]), Some([0; 3]), Some([10, 11, 12]), Some((10, 20)));
    t.set_display(None, None, None, None);
    assert!(t.take_responses().is_empty(), "repeated colors and cell size are silent");

    t.write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b[?996n\x1b[16t");
    assert_eq!(
        t.take_responses(),
        b"\x1b]10;rgb:0707/0808/0909\x07\x1b]11;rgb:0000/0000/0000\x07\x1b]12;rgb:0a0a/0b0b/0c0c\x07\x1b[?997;1n\x1b[6;20;10t"
    );
}

#[test]
fn display_changes_do_not_report_without_a_subscription() {
    let mut t = Terminal::new(80, 24, 0);
    t.set_display(None, Some([255; 3]), None, None);
    assert!(t.take_responses().is_empty());
    t.write(b"\x1b[?2031h\x1b[?2031l");
    t.take_responses();
    t.set_display(None, Some([0; 3]), None, None);
    assert!(t.take_responses().is_empty(), "disabled subscription stays silent");
}

#[test]
fn a_repaint_into_a_dirty_viewer_turns_off_modes_the_screen_no_longer_has() {
    let mut viewer = Terminal::new(40, 10, 0);
    viewer.write(b"\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1004h");
    let screen = Terminal::new(40, 10, 0);
    viewer.write(&screen.paint(0));
    for mode in [1000, 1006, 2004, 1004] {
        assert!(!viewer.dec_mode(mode), "mode {mode} left on");
    }
    let mut mouse = Terminal::new(40, 10, 0);
    mouse.write(b"\x1b[?1000h");
    viewer.write(&mouse.paint(0));
    assert!(viewer.dec_mode(1000), "and the screen's own modes come back");
}

#[test]
fn parses_reads_and_round_trips_a_snapshot() {
    let mut t = Terminal::new(20, 3, 1 << 20);
    for i in 0..10 {
        t.write(format!("line {i}\r\n").as_bytes());
    }
    let text = t.plain_text();
    assert!(text.contains("line 0") && text.contains("line 9"), "{text:?}");

    // Snapshot taken in the middle of an escape sequence finishes it after restore.
    t.write(b"\x1b[3");
    let snap = t.snapshot().unwrap();
    let mut back = Terminal::restore(&snap, 1 << 20).unwrap();
    back.write(b"1mred\x1b[0m");
    assert_eq!(back.size(), (20, 3));
    let text = back.plain_text();
    assert!(text.contains("line 0") && text.contains("red"), "{text:?}");
    assert!(!text.contains("1mred"), "{text:?}");

    t.write(b"m\x1b[?1049h");
    assert!(t.alt_screen());
    assert!(Terminal::restore(b"garbage", 1 << 20).is_none());
}

#[test]
fn a_painted_terminal_looks_the_same_on_a_fresh_one() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    for i in 0..8 {
        t.write(format!("\x1b[31mline {i}\x1b[0m\r\n").as_bytes());
    }
    t.write(b"\x1b]2;my title\x07\x1b[?2026h\x1b[?1004hprompt> ");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(3));
    let screen = |t: &Terminal| t.plain_text().lines().rev().take(4).map(String::from).collect::<Vec<_>>();
    assert_eq!(screen(&fresh), screen(&t));
    assert!(fresh.plain_text().contains("line 2"), "{:?}", fresh.plain_text());
    assert!(!fresh.plain_text().contains("line 1\n"), "history beyond 3 lines");
    assert_eq!(fresh.title(), "my title");
    assert!(fresh.dec_mode(1004));
    assert!(!fresh.dec_mode(2026), "synchronized output must not be replayed");

    let mut alt = Terminal::new(30, 4, 1 << 20);
    alt.write(b"main\r\n\x1b[?1049h\x1b[2;3Hfull screen");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&alt.paint(10));
    assert!(fresh.alt_screen());
    assert!(fresh.plain_text().contains("full screen"));
}

#[test]
fn repaint_uses_the_active_screen_not_stale_alternate_screen_modes() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    t.write(b"main\r\n\x1b[?47h\x1b[?1049hALT\x1b[?1049lDONE");
    assert!(!t.alt_screen());
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(10));
    assert!(!fresh.alt_screen());
    assert_eq!(fresh.screen_rows(), t.screen_rows());

    for enter in [47, 1047, 1049] {
        for exit in [None, Some(47), Some(1047), Some(1049)] {
            let mut t = Terminal::new(30, 4, 1 << 20);
            t.write(format!("main\r\n\x1b[?{enter}h\x1b[2;3HALT").as_bytes());
            if let Some(exit) = exit {
                t.write(format!("\x1b[?{exit}lDONE").as_bytes());
            }
            let paint = t.paint(10);
            for viewer_mode in [None, Some(47), Some(1047), Some(1049)] {
                let mut viewer = Terminal::new(30, 4, 1 << 20);
                if let Some(mode) = viewer_mode {
                    viewer.write(format!("old main\x1b[?{mode}hOLD ALT").as_bytes());
                }
                viewer.write(&paint);
                let case = (enter, exit, viewer_mode);
                assert_eq!(viewer.alt_screen(), t.alt_screen(), "{case:?}");
                assert_eq!(viewer.screen_rows(), t.screen_rows(), "{case:?}");
                assert!(!viewer.dec_mode(47), "{case:?}");
                assert!(!viewer.dec_mode(1047), "{case:?}");
                assert_eq!(viewer.dec_mode(1049), t.alt_screen(), "{case:?}");
            }
        }
    }
}

#[test]
fn program_status_reports_and_title_changes_reach_us() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.write(b"\x1b]7501;state=working:app=claude-code:progress=40\x1b\\");
    t.write(b"\x1b]7501;state=blocked:kind=permission:id=a/b:msg=QXBwbHk/\x07");
    t.write(b"\x1b]7501;state=clear\x1b\\\x1b]7501;state=bogus\x1b\\");
    let reports = t.take_status_reports();
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert_eq!((reports[0].state, reports[0].progress, reports[0].app.as_str()), ("working", Some(40), "claude-code"));
    assert_eq!(
        (reports[1].kind, reports[1].id.as_str(), reports[1].message.as_str()),
        (Some("permission"), "a/b", "Apply?")
    );
    assert_eq!(reports[2].state, "clear");
    // A program asking whether we support the protocol gets an answer.
    t.write(b"\x1b]7501;?\x1b\\");
    assert!(String::from_utf8_lossy(&t.take_responses()).starts_with("\x1b]7501;?"));

    assert!(!t.take_title_changed());
    t.write(b"\x1b]2;hi\x07");
    assert!(t.take_title_changed());
}

#[test]
fn wheel_notches_follow_the_programs_mouse_mode_and_rows_keep_their_width() {
    let mut t = Terminal::new(20, 4, 1 << 20);
    assert!(t.wheel(true).is_empty(), "no mouse reports asked for");
    t.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    assert_eq!(String::from_utf8_lossy(&t.wheel(true)), "\x1b[<64;11;3M");
    assert_eq!(String::from_utf8_lossy(&t.wheel(false)), "\x1b[<65;11;3M");
    t.write(b"hello\r\nworld  \x1b[2;19H\xe2\x94\x82");
    let rows = t.screen_rows();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], format!("{:<20}", "hello"));
    assert!(rows[1].starts_with("world") && rows[1].chars().nth(18) == Some('│'), "{:?}", rows[1]);
    assert!(rows.iter().all(|r| r.chars().count() == 20));
}

#[test]
fn answers_queries_like_ghostty_with_viewer_facts() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.set_xtversion("WezTerm 20240203");
    t.set_display(Some([1, 2, 3]), Some([0xff, 0xff, 0xff]), None, Some((9, 18)));
    let ask = |t: &mut Terminal, q: &[u8]| {
        t.write(q);
        String::from_utf8_lossy(&t.take_responses()).into_owned()
    };
    assert_eq!(ask(&mut t, b"\x1b[c"), "\x1b[?62;22c");
    assert_eq!(ask(&mut t, b"\x1b[>c"), "\x1b[>1;10;0c");
    assert!(ask(&mut t, b"\x1b[>q").contains("WezTerm 20240203"));
    assert_eq!(ask(&mut t, b"\x1b[3;5H\x1b[6n"), "\x1b[3;5R");
    assert!(ask(&mut t, b"\x1b]11;?\x07").contains("rgb:ffff/ffff/ffff"));
    assert!(ask(&mut t, b"\x1b]10;?\x07").contains("rgb:0101/0202/0303"));
    assert_eq!(ask(&mut t, b"\x1b[?996n"), "\x1b[?997;2n", "light background");
    assert_eq!(ask(&mut t, b"\x1b[16t"), "\x1b[6;18;9t");
    assert_eq!(ask(&mut t, b"\x1b[18t"), "\x1b[8;24;80t");
    assert_eq!(ask(&mut t, b"\x1b[?2004$p"), "\x1b[?2004;2$y");
    assert_eq!(ask(&mut t, b"\x1b[?u"), "\x1b[?0u");
}
