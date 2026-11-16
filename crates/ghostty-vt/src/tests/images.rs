use super::*;

use crate::testing::{base64, chunked, count, noise, placed, placeholder_cells, same_image};
use kitty::tests::png_of;

/// One kitty command, base64 included, without chunks.
fn image(control: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("\x1b_G{control},q=2;{}", base64(payload)).into_bytes();
    out.extend(b"\x1b\\");
    out
}

/// A 4x2 png over two cells.
fn small(id: u32) -> Vec<u8> {
    chunked(&format!("a=T,f=100,i={id},c=2,r=1,C=1,q=2"), &png_of(4, 2))
}

/// An 8x2 png behind placeholder cells, as image 2.
fn placeholder_png() -> Vec<u8> {
    chunked("a=T,f=100,U=1,i=2,c=4,r=1,q=2", &png_of(8, 2))
}

fn ids(t: &Terminal) -> Vec<u32> {
    t.image_placements().iter().map(|p| p.image).collect()
}

#[test]
fn images_without_a_size_take_it_from_the_viewers_cell() {
    let shown = |cell| {
        let mut t = Terminal::new(80, 24, 1 << 20);
        t.set_display(None, None, None, cell);
        t.write(&image("a=T,f=100,i=1", &png_of(100, 80)));
        t.write(b"X");
        t.screen_rows().iter().position(|row| row.contains('X')).unwrap()
    };
    assert_eq!(shown(None), 3, "80 px over the default 20 px cell: 4 rows");
    assert_eq!(shown(Some((20, 40))), 1, "over a 40 px cell: 2 rows");
}

#[test]
fn a_screen_keeps_images_past_the_librarys_default_limit() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    let pixels = vec![1u8; 2000 * 2000 * 4];
    t.write(&image("a=T,f=32,s=2000,v=2000,i=1,c=4,r=3", &pixels));
    t.write(b"X");
    assert!(t.screen_rows()[2].contains('X'), "16 MB is over the 10 MB default, and placing it moved the cursor");
}

// ---- Replay: a viewer ends up with what the screen has.

fn fresh(paint: &[u8]) -> Terminal {
    let mut viewer = Terminal::new(80, 24, 1 << 20);
    viewer.write(paint);
    viewer
}

fn cursor_of(t: &mut Terminal) -> Vec<u8> {
    t.take_responses();
    t.write(b"\x1b[6n");
    t.take_responses()
}

#[test]
fn a_painted_terminal_brings_back_its_images_and_never_makes_a_viewer_reply() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(b"direct\r\n");
    a.write(&chunked("a=T,f=100,i=1,c=8,r=2,z=-1,q=2", &png_of(16, 8)));
    a.write(b"\r\nraw\r\n");
    a.write(&chunked("a=T,f=24,s=2,v=2,i=3,c=2,r=2,q=2", &[9u8; 12]));
    a.write(b"\r\nimplicit ");
    a.write(&chunked("a=T,f=100,c=2,r=1,C=1,q=2", &png_of(4, 2)));
    a.write(b"\r\n");
    a.write(&placeholder_png());
    a.write(placeholder_cells().as_bytes());
    a.write(b"\x1b[31mred");
    let mut v = fresh(&a.paint(0));
    assert!(v.take_responses().is_empty(), "a program would read these as typing");
    assert_eq!(placed(&a), placed(&v));
    assert_eq!(a.screen_rows(), v.screen_rows());
    for id in [1, 2, 3, 2147483647] {
        assert!(same_image(&a.image_pixels(id).unwrap(), &v.image_pixels(id).unwrap()), "pixels of {id}");
    }
    assert_eq!(cursor_of(&mut a), cursor_of(&mut v));
}

#[test]
fn native_size_and_partly_sized_images_replay_at_the_size_the_screen_gives_them() {
    for control in ["", ",c=5", ",r=3", ",c=5,r=2", ",X=3,Y=7", ",c=5,Y=7"] {
        let mut a = Terminal::new(80, 24, 1 << 20);
        a.write(&chunked(&format!("a=T,f=32,s=100,v=30,i=1{control},C=1,q=2"), &vec![1u8; 100 * 30 * 4]));
        let v = fresh(&a.paint(0));
        assert_eq!(placed(&a), placed(&v), "with {control:?}");
    }
}

#[test]
fn text_goes_first_and_is_sent_once_when_no_direct_image_is_waited_for() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(b"MARK ");
    a.write(&small(1));
    a.write(&placeholder_png());
    a.write(placeholder_cells().as_bytes());
    let paint = a.paint(0);
    let at = |needle: &[u8]| paint.windows(needle.len()).position(|w| w == needle).unwrap();
    assert!(at(b"MARK") < at(b"i=1"), "text before the direct image");
    assert_eq!(count(&paint, b"MARK"), 2, "and again after it, to restore the screen's modes");
    let last_text = paint.windows(4).rposition(|w| w == b"MARK").unwrap();
    assert!(at(b"i=1") < last_text);
    assert!(last_text < at(b"i=2"), "placeholder images after everything");

    let mut placeholders_only = Terminal::new(80, 24, 1 << 20);
    placeholders_only.write(b"MARK ");
    placeholders_only.write(&placeholder_png());
    assert_eq!(count(&placeholders_only.paint(0), b"MARK"), 1, "a paint without direct images sends the text once");
}

#[test]
fn a_direct_image_keeps_a_pending_wrap_at_the_bottom_right() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&small(1));
    a.write(b"\x1b[24;1H");
    a.write(&[b'x'; 80]);
    let mut v = fresh(&a.paint(0));
    assert_eq!(placed(&a), placed(&v));
    for t in [&mut a, &mut v] {
        t.write(b"Y");
    }
    assert_eq!(a.screen_rows(), v.screen_rows(), "Y wraps and scrolls on both");
    assert_eq!(placed(&a), placed(&v));
}

#[test]
fn a_half_received_image_on_a_viewer_swallows_nothing_that_follows() {
    let open = chunked("a=T,f=32,s=600,v=600,i=77,q=2", &vec![7u8; 600 * 600 * 4]);
    let cut = open.windows(2).enumerate().filter(|(_, w)| *w == b"\x1b\\").nth(5).unwrap().0 + 2;
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=T,f=100,i=5,c=4,r=2,C=1,q=2", &png_of(8, 4)));

    let mut v = Terminal::new(80, 24, 1 << 20);
    v.write(&open[..cut]);
    v.write(&a.paint(0));
    assert_eq!(placed(&a), placed(&v), "the replay isn't taken for its continuation");

    let empty = Terminal::new(80, 24, 1 << 20);
    let mut v = Terminal::new(80, 24, 1 << 20);
    v.write(&open[..cut]);
    v.write(&empty.paint(0));
    v.write(&chunked("a=T,f=100,i=6,c=2,r=1,C=1,q=2", &png_of(4, 2)));
    assert_eq!(ids(&v), [6], "nor the next live image");
}

#[test]
fn a_load_a_paint_happens_to_complete_leaves_nothing_behind() {
    // All its data arrived, and only the empty last command was lost with the backlog.
    let mut unfinished =
        format!("\x1b_Ga=T,f=32,s=2,v=2,i=70,c=4,r=3,q=2,m=1;{}\x1b\\", base64(&[1u8; 16])).into_bytes();
    let mut a = Terminal::new(80, 24, 1 << 20);
    for i in 0..40 {
        a.write(format!("line{i}\r\n").as_bytes());
    }
    let mut v = Terminal::new(80, 24, 1 << 20);
    v.write(&std::mem::take(&mut unfinished));
    v.write(&a.paint(40));
    assert!(placed(&v).is_empty(), "the clear after it takes the stray placement");
    assert_eq!(fresh(&a.paint(40)).plain_text(), v.plain_text(), "and nothing it moved shifts the history");
}

#[test]
fn replayed_images_keep_their_ids_so_placeholder_cells_still_name_them() {
    // The screen's id for a numbered image is what its reply told the program.
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=t,f=100,I=7,q=2", &png_of(4, 2)));
    a.write(&chunked("a=T,f=100,I=8,U=1,c=4,r=1,q=2", &png_of(8, 2)));
    assert_eq!(ids(&a), [2]);
    let v = fresh(&a.paint(0));
    assert_eq!(ids(&v), [2], "the cells on screen name image 2");
    let mut twice = fresh(&a.paint(0));
    twice.write(&a.paint(0));
    assert_eq!(ids(&twice), [2], "repainting doesn't stack copies");
}

/// Asks for a paint that image 1 can't fit in three times, and says it was encoded once.
#[track_caller]
fn tried_once(a: &Terminal) {
    kitty::tests::ENCODES.set(0);
    for _ in 0..3 {
        assert!(a.images_within(1000).direct.is_empty());
    }
    assert_eq!(kitty::tests::ENCODES.get(), 1);
}

#[test]
fn an_image_too_big_for_a_paint_is_encoded_once_however_many_places_it_has() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=T,f=32,s=100,v=100,i=1,c=2,r=1,C=1,q=2", &noise(40_000)));
    for row in 2..6 {
        a.write(format!("\x1b[{row};1H\x1b_Ga=p,i=1,c=2,r=1,C=1,q=2\x1b\\").as_bytes());
    }
    tried_once(&a);
    assert!(
        a.encoded[0].borrow().values().all(|e| matches!(e, kitty::Encoded::TooBig(_))),
        "and keeps no bytes for it"
    );
    kitty::tests::ENCODES.set(0);
    assert!(!a.images_within(100_000).direct.is_empty(), "it fits a larger paint");
    let _ = a.images_within(100_000);
    assert_eq!(kitty::tests::ENCODES.get(), 1, "which then reuses it");
}

#[test]
fn the_cache_forgets_images_that_are_gone() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&small(1));
    let _ = a.paint(0);
    assert_eq!(a.encoded[0].borrow().len(), 1);
    a.write(b"\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
    let _ = a.paint(0);
    assert!(a.encoded[0].borrow().is_empty());
}

#[test]
fn painting_a_viewer_again_puts_its_direct_images_back_once() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=T,f=100,i=1,c=4,r=2,C=1,q=2", &png_of(8, 4)));
    a.write(&placeholder_png());
    a.write(placeholder_cells().as_bytes());
    let mut v = fresh(&a.paint(0));
    v.write(&a.paint(0));
    assert_eq!(placed(&a), placed(&v));
    assert!(v.take_responses().is_empty());
}

#[test]
fn images_on_the_alternate_screen_are_painted_after_switching_to_it() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&small(1));
    a.write(b"\x1b[?1049h");
    a.write(&small(2));
    let v = fresh(&a.paint(0));
    assert!(v.alt_screen());
    assert_eq!(placed(&a), placed(&v));
}

/// How a screen repaints itself, which is all a program or viewer can tell of it.
fn looks_like(t: &Terminal) -> Vec<u8> {
    t.paint_text(0)
}

#[test]
fn a_paint_with_direct_images_leaves_a_viewer_with_the_same_state_as_one_without() {
    let image = chunked("a=T,f=100,i=1,c=4,r=2,C=1,q=2", &png_of(8, 4));
    let link = "\x1b]8;;http://x\x1b\\";
    let states: &[(&str, String)] = &[
        ("insert mode, which would double text", "hello\x1b[4h".into()),
        ("a link left open", format!("before {link}linked")),
        ("a link closed again", format!("{link}linked\x1b]8;;\x1b\\ plain")),
        ("margins and origin mode", "\x1b[3;10r\x1b[?6h\x1b[2;2Hin".into()),
        ("colors", "\x1b[41;33mred\x1b[K\x1b[0m\r\nplain".into()),
        ("tabs", "\x1b[3g\x1b[1;5H\x1bH\rA\tB".into()),
        ("no wrap", "\x1b[?7lxyz".into()),
        ("a full last row", format!("\x1b[24;1H{}", "z".repeat(80))),
        ("color to the very end", format!("\x1b[24;1H\x1b[41m{}\x1b[0m", "z".repeat(80))),
        ("color left on", "\x1b[41mstill red".into()),
        ("the alternate screen", "main\x1b[?1049halt".into()),
        ("modes and keyboard flags", "\x1b[=3;1u\x1b[?2004h\x1b[?1000hmodes".into()),
    ];
    for (what, state) in states {
        let mut a = Terminal::new(80, 24, 1 << 20);
        a.write(state.as_bytes());
        let without = looks_like(&fresh(&a.paint_text(0)));
        a.write(&image);
        assert_eq!(a.image_placements().len(), 1, "{what}");
        let paint = a.paint(0);
        assert!(count(&paint, b"a=p,i=1") == 1, "{what}");
        let viewer = fresh(&paint);
        assert_eq!(looks_like(&viewer), without, "{what}");
        assert_eq!(placed(&viewer), placed(&a), "{what}");
    }
}

#[test]
fn modes_that_report_when_turned_on_report_once_however_images_are_painted() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(b"\x1b[?2048h");
    a.take_responses();
    a.write(&chunked("a=T,f=100,i=1,c=4,r=2,C=1,q=2", &png_of(8, 4)));
    let mut viewer = fresh(&a.paint(0));
    assert_eq!(count(&viewer.take_responses(), b"\x1b[48;"), 1);
}

#[test]
fn a_repaint_closes_a_link_a_dropped_backlog_left_open() {
    let mut viewer = Terminal::new(80, 24, 1 << 20);
    viewer.write(b"\x1b]8;;http://x\x1b\\linked");
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(b"plain");
    viewer.write(&a.paint(0));
    viewer.write(b" more");
    a.write(b" more");
    assert_eq!(looks_like(&viewer), looks_like(&a));
}

#[test]
fn a_paint_without_images_leaves_them_out_and_a_viewer_keeps_placeholder_ones() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    assert!(!a.has_images());
    a.write(&chunked("a=T,f=100,i=1,c=4,r=2,C=1,q=2", &png_of(8, 4)));
    a.write(&placeholder_png());
    a.write(placeholder_cells().as_bytes());
    assert!(a.has_images());
    let mut placeholder_only = Terminal::new(80, 24, 1 << 20);
    placeholder_only.write(&placeholder_png());
    assert!(placeholder_only.has_images());
    let mut scrolled = Terminal::new(80, 24, 1 << 20);
    scrolled.write(&chunked("a=T,f=100,i=1,c=4,r=2,C=1,q=2", &png_of(8, 4)));
    scrolled.write(&b"\n".repeat(30));
    assert_eq!(scrolled.image_placements().len(), 1);
    assert!(!scrolled.has_images(), "there's nothing to paint");
    let text = a.paint_text(0);
    assert_eq!(count(&text, b"a=t,"), 0);
    assert_eq!(count(&text, b"a=p,"), 0);
    assert!(a.encoded[0].borrow().is_empty(), "and costs no encoding");
    let mut viewer = fresh(&a.paint(0));
    assert_eq!(viewer.image_placements().len(), 2);
    viewer.write(&text);
    assert_eq!(ids(&viewer), [2]);
    assert!(viewer.image_pixels(2).is_some(), "and a text paint leaves its picture");
}

#[test]
fn only_a_new_cell_size_counts_as_a_resize() {
    let mut t = Terminal::new(80, 24, 0);
    t.set_display(None, None, None, Some((9, 18)));
    t.write(b"\x1b[?2048h");
    t.take_responses();
    t.set_display(None, None, None, Some((9, 18)));
    assert!(t.take_responses().is_empty());
    t.set_display(None, None, None, Some((10, 20)));
    assert_eq!(count(&t.take_responses(), b"\x1b[48;"), 1);
}

#[test]
fn in_a_tight_paint_direct_images_go_before_placeholder_ones_and_the_newest_first() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=T,f=32,s=100,v=100,i=1,c=2,r=1,C=1,q=2", &noise(40_000)));
    a.write(&chunked("a=T,f=32,s=100,v=100,U=1,i=2,c=4,r=1,q=2", &noise(40_000)));
    let one = a.images_within(60_000);
    assert_eq!(count(&one.direct, b"a=t,"), 1);
    assert_eq!(count(&one.placeholder, b"a=t,"), 0);

    let mut many = Terminal::new(80, 24, 1 << 20);
    for id in 1..=8 {
        many.write(&chunked(&format!("a=T,f=32,s=100,v=100,i={id},c=2,r=1,C=1,q=2"), &noise(40_000)));
    }
    let mut viewer = Terminal::new(80, 24, 1 << 20);
    viewer.write(&many.images_within(60_000).direct);
    assert_eq!(ids(&viewer), [8]);
}

#[test]
fn a_paint_carries_at_most_so_many_images_and_placements() {
    let mut images = Terminal::new(80, 24, 1 << 20);
    for id in 1..=300 {
        images.write(&image(&format!("a=T,f=100,i={id},c=1,r=1,C=1"), &png_of(1, 1)));
    }
    let direct = images.images_within(1 << 20).direct;
    assert_eq!(count(&direct, b"a=t,"), 256);
    assert_eq!(count(&direct, b"a=p,"), 256);

    let mut places = Terminal::new(80, 24, 1 << 20);
    places.write(&image("a=T,f=100,i=1,c=1,r=1,C=1", &png_of(1, 1)));
    for _ in 0..1100 {
        places.write(b"\x1b_Ga=p,i=1,c=1,r=1,C=1,q=2\x1b\\");
    }
    assert_eq!(count(&places.images_within(1 << 20).direct, b"a=p,"), 1024);
}

#[test]
fn every_image_a_screen_replays_reaches_a_viewer_whatever_its_content() {
    let gradient = |w: u32, h: u32, channels: usize| -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let pixel = [(x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y) * 255 / (w + h)) as u8, 255];
                pixels.extend_from_slice(&pixel[..channels]);
            }
        }
        pixels
    };
    for (w, h) in [(1, 1), (8, 4), (32, 24), (64, 48), (100, 80), (200, 150), (400, 300)] {
        let area = (w * h) as usize;
        let cases = [(24, gradient(w, h, 3)), (32, gradient(w, h, 4)), (32, noise(area * 4)), (24, noise(area * 3))];
        for (format, pixels) in cases {
            let mut a = Terminal::new(80, 24, 1 << 20);
            a.write(&chunked(&format!("a=T,f={format},s={w},v={h},i=1,c=4,r=2,C=1,q=2"), &pixels));
            let viewer = fresh(&a.paint(0));
            let shown = viewer.image_pixels(1).unwrap_or_else(|| panic!("f={format} {w}x{h} never arrived"));
            let channels = pixels.len() / area;
            let rgb: Vec<&[u8]> = shown.chunks(shown.len() / area).map(|p| &p[..channels]).collect();
            assert!(rgb.iter().zip(pixels.chunks(channels)).all(|(a, b)| *a == b), "f={format} {w}x{h} changed");
        }
    }
}

#[test]
fn an_image_too_big_for_a_paint_does_not_leave_a_viewer_its_old_picture() {
    for places in ["U=1,c=4,r=1", "c=4,r=1,C=1"] {
        let mut a = Terminal::new(80, 24, 1 << 20);
        a.write(&chunked(&format!("a=T,f=100,i=1,{places},q=2"), &png_of(8, 2)));
        let mut viewer = fresh(&a.paint(0));
        assert!(viewer.image_pixels(1).is_some());
        let side = 1040u32;
        a.write(&chunked(&format!("a=T,f=32,s={side},v={side},i=1,{places},q=2"), &noise((side * side * 4) as usize)));
        a.write(b"\x1b_Ga=p,U=1,i=1,p=7,c=4,r=1,q=2\x1b\\");
        let paint = a.paint(0);
        assert_eq!(count(&paint, b"a=t,"), 0, "too big");
        viewer.write(&paint);
        assert!(viewer.image_pixels(1).is_none(), "no picture beats the wrong one");
    }
}

#[test]
fn a_retransmitted_image_replays_its_new_pixels() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    let two = |pixels: &[u8]| chunked("a=T,f=32,s=2,v=1,i=1,c=4,r=1,C=1,q=2", pixels);
    a.write(&two(&[1, 2, 3, 4, 5, 6, 7, 8]));
    let _ = a.paint(0);
    a.write(&two(&[9, 9, 9, 9, 8, 8, 8, 8]));
    assert_eq!(fresh(&a.paint(0)).image_pixels(1).unwrap(), [9, 9, 9, 9, 8, 8, 8, 8]);
}

/// The zlib stream of a PNG. Ghostty inflates `o=z` itself, and strictly, as it does a PNG
/// in a terminal with no decoder hook: every viewer in these tests has the core's lenient one.
fn idat_of(png: &[u8]) -> Vec<u8> {
    let mut zlib = Vec::new();
    let mut at = 8;
    while at < png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
        if &png[at + 4..at + 8] == b"IDAT" {
            zlib.extend_from_slice(&png[at + 8..at + 8 + len]);
        }
        at += 12 + len;
    }
    zlib
}

/// Pixels whose filtered rows repeat no three bytes, over a small alphabet: nothing to
/// match, but worth compressing, so a block with no distance codes if an encoder allows it.
fn pixels_with_nothing_to_match(width: u32, height: u32) -> Vec<u8> {
    // A de Bruijn sequence: every three of 16 symbols, once.
    fn extend(t: usize, p: usize, a: &mut [usize], symbols: &mut Vec<u8>) {
        if t > 3 {
            if 3 % p == 0 {
                symbols.extend(a[1..=p].iter().map(|x| *x as u8 + 32));
            }
            return;
        }
        a[t] = a[t - p];
        extend(t + 1, p, a, symbols);
        for j in a[t - p] + 1..16 {
            a[t] = j;
            extend(t + 1, t, a, symbols);
        }
    }
    let mut symbols = Vec::new();
    extend(1, 1, &mut [0; 4], &mut symbols);
    let row = width as usize * 4;
    let mut pixels = Vec::<u8>::new();
    for i in 0..row * height as usize {
        // Undoes what the Paeth filter will do, so that it leaves the sequence.
        let x = i % row;
        let left = if x >= 4 { pixels[i - 4] as i32 } else { 0 };
        let up = if i >= row { pixels[i - row] as i32 } else { 0 };
        let corner = if i >= row && x >= 4 { pixels[i - row - 4] as i32 } else { 0 };
        let guess = left + up - corner;
        let (l, u, c) = ((guess - left).abs(), (guess - up).abs(), (guess - corner).abs());
        let paeth = if l <= u && l <= c {
            left
        } else if u <= c {
            up
        } else {
            corner
        };
        pixels.push(symbols[i % symbols.len()].wrapping_add(paeth as u8));
    }
    pixels
}

#[test]
fn what_a_replay_compresses_is_something_ghostty_itself_can_read() {
    for w in [4, 8, 16, 32, 64, 100, 128, 256] {
        let h = 4;
        let area = (w * h * 4) as usize;
        let gradient: Vec<u8> = (0..area as u32).map(|i| (i / 4 + i % 4 * 11) as u8).collect();
        for pixels in [pixels_with_nothing_to_match(w, h), gradient, noise(area)] {
            let mut a = Terminal::new(80, 24, 1 << 20);
            a.write(&chunked(&format!("a=T,f=32,s={w},v={h},i=1,c=4,r=2,C=1,q=2"), &pixels));
            let png = kitty::tests::png_file_of(&a.image(1).unwrap());
            // Filtered rows are one byte longer each: `h` of 4 makes them a whole number of pixels.
            let s = (w * 4 + 1) * h / 4;
            let mut strict = Terminal::new(80, 24, 1 << 20);
            strict.write(&image_command(&format!("a=T,f=32,s={s},v=1,o=z,i=1,c=4,r=1"), &idat_of(&png)));
            assert!(strict.image_pixels(1).is_some(), "{w}x{h}: Ghostty refused it");
        }
    }
}

fn image_command(control: &str, payload: &[u8]) -> Vec<u8> {
    chunked(&format!("{control},q=2"), payload)
}

#[test]
fn filtering_rows_is_what_makes_photos_fit() {
    use std::io::Write;
    // Grain over a gradient: what compresses by filtering and hardly otherwise.
    let grain = noise(128 * 128 * 4);
    let pixels: Vec<u8> = (0..128u32 * 128 * 4)
        .map(|i| {
            let (x, y) = ((i / 4) % 128, (i / 4) / 128);
            if i % 4 == 3 { 255 } else { ((x + y) as u8).wrapping_add(grain[i as usize] & 3) }
        })
        .collect();
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=T,f=32,s=128,v=128,i=1,c=4,r=2,C=1,q=2", &pixels));
    let png = kitty::tests::png_file_of(&a.image(1).unwrap());
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(2));
    zlib.write_all(&pixels).unwrap();
    let plain = zlib.finish().unwrap();
    assert!(png.len() * 5 < plain.len() * 4, "{} against {}", png.len(), plain.len());
}

#[test]
fn an_image_exactly_as_big_as_what_is_left_fits_cached_or_not() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=T,f=32,s=100,v=100,i=1,U=1,c=4,r=1,q=2", &noise(40_000)));
    let exact = {
        let replay = a.images_within(1 << 20);
        count(&replay.placeholder, b"a=t,");
        a.encoded[0].borrow_mut().clear();
        kitty::tests::encoded_len(&a.image(1).unwrap())
    };
    assert_eq!(count(&a.images_within(exact).placeholder, b"a=t,"), 1, "fresh");
    assert_eq!(count(&a.images_within(exact - 1).placeholder, b"a=t,"), 0, "a byte short");
    assert_eq!(count(&a.images_within(exact).placeholder, b"a=t,"), 1, "and known by then to be that big");
}

#[test]
fn a_paint_from_the_alternate_screen_leaves_the_main_screens_images_be() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&chunked("a=T,f=100,U=1,i=42,c=2,r=1,q=2", &png_of(2, 2)));
    let mut viewer = fresh(&a.paint(0));
    assert_eq!(viewer.image_placements().len(), 1);
    a.write(b"\x1b[?1049h");
    viewer.write(b"\x1b[?1049h");
    viewer.write(&a.paint(0));
    a.write(b"\x1b[?1049l");
    viewer.write(b"\x1b[?1049l");
    assert_eq!(viewer.image_placements().len(), a.image_placements().len());
    assert!(viewer.image_pixels(42).is_some());
}

#[test]
fn a_paint_sends_images_oldest_first_so_a_viewer_lets_the_same_ones_go() {
    let at = |paint: &[u8], id: u32| {
        let wanted = format!("i={id},");
        paint.windows(wanted.len()).position(|w| w == wanted.as_bytes()).expect("sent")
    };
    for places in ["c=2,r=1,C=1", "U=1,c=2,r=1"] {
        let mut a = Terminal::new(80, 24, 1 << 20);
        for id in 1..=3 {
            a.write(&chunked(&format!("a=T,f=100,i={id},{places},q=2"), &png_of(2, 2)));
        }
        let paint = a.paint(0);
        assert!(at(&paint, 1) < at(&paint, 2) && at(&paint, 2) < at(&paint, 3), "{places}");
    }
}

#[test]
fn a_relative_placement_all_off_the_left_stays_out_of_sight() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&image("a=T,f=32,s=1,v=1,i=1,p=1,c=1,r=1,C=1", &[255, 0, 0, 255]));
    a.write(&image("a=t,f=32,s=4,v=1,i=2", &[0, 255, 0, 255].repeat(4)));
    a.write(b"\x1b_Ga=p,i=2,p=2,P=1,Q=1,H=-6,V=1,c=4,r=1,C=1,q=2\x1b\\");
    let child = a.image_placements().into_iter().find(|p| p.image == 2).unwrap();
    assert_eq!((child.col, child.visible), (-6, false));
    let viewer = fresh(&a.paint(0));
    assert!(viewer.image_placements().iter().all(|p| p.image != 2));
}

#[test]
fn a_first_paint_with_no_image_to_bring_leaves_what_a_used_terminal_holds() {
    let mut outer = Terminal::new(80, 24, 1 << 20);
    outer.write(b"\x1b_Ga=T,f=32,s=1,v=1,i=5,c=2,r=1,q=2;AAAAAA==\x1b\\");
    outer.write(&b"\r\n".repeat(60));
    let mut screen = Terminal::new(80, 24, 1 << 20);
    screen.write(b"text");
    outer.write(&screen.paint(0));
    assert!(outer.image_pixels(5).is_some(), "as a paint without images always did");
    screen.write(b"\x1b_Ga=T,f=32,s=1,v=1,i=6,c=2,r=1,q=2;AAAAAA==\x1b\\");
    outer.write(&screen.paint(0));
    assert!(outer.image_pixels(5).is_none(), "of one it can't bring, no picture beats an older one");
}

#[test]
fn an_image_cut_off_at_the_top_keeps_its_offset_inside_the_cell() {
    let mut screen = Terminal::new(80, 24, 1 << 20);
    screen.set_display(None, None, None, Some((10, 40)));
    let mut image = b"\x1b_Ga=T,f=32,s=40,v=80,i=5,Y=39,C=1,q=2;".to_vec();
    image.extend(crate::testing::base64(&vec![7u8; 40 * 80 * 4]).as_bytes());
    image.extend(b"\x1b\\");
    screen.write(&image);
    screen.set_display(None, None, None, Some((10, 10)));
    screen.write(b"\x1b[24;1H");
    screen.write(&[b'\n'; 4]);
    let p = screen.image_placements()[0].clone();
    assert!(p.visible && p.row < 0, "or this isn't the case it's about");
    let mut viewer = Terminal::new(80, 24, 1 << 20);
    viewer.set_display(None, None, None, Some((10, 10)));
    viewer.write(&screen.paint(0));
    let shows = viewer.image_placements()[0].source[3] as i64;
    let wanted = p.pixels.1 as i64 + p.row as i64 * 10 + p.offset.1.min(9) as i64;
    assert!(shows.abs_diff(wanted) <= 10, "shows {shows} of {wanted}");
}

#[test]
fn a_paint_stops_encoding_past_its_share_and_leaves_the_rest_for_the_next() {
    // Noise doesn't compress: three fit a paint, and trying the others would only stall the lock.
    let mut a = Terminal::new(80, 24, 1 << 20);
    for id in 1..=8 {
        a.write(&chunked(&format!("a=T,f=32,s=512,v=512,i={id},c=1,r=1,C=1,q=2"), &noise(1 << 20)));
    }
    kitty::tests::ENCODES.set(0);
    let paint = a.paint(0);
    assert_eq!(count(&paint, b"a=t,"), 3);
    assert!(kitty::tests::ENCODES.get() < 8, "not every one: {}", kitty::tests::ENCODES.get());
    assert_eq!(count(&a.paint(0), b"a=t,"), 3, "the same ones, whatever the next paint tries");
}

#[test]
fn painting_the_alternate_screen_keeps_what_the_main_one_encoded() {
    let mut a = Terminal::new(80, 24, 1 << 20);
    a.write(&small(1));
    let _ = a.paint(0);
    a.write(b"\x1b[?1049h");
    a.write(&small(2));
    let _ = a.paint(0);
    a.write(b"\x1b[?1049l");
    kitty::tests::ENCODES.set(0);
    let _ = a.paint(0);
    assert_eq!(kitty::tests::ENCODES.get(), 0);
}
