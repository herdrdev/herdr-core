use super::*;
use std::cell::Cell;
use std::time::Duration;

/// Each test its own: they share a process, not a lock, and one mustn't spend another's time.
fn budget() -> Mutex<DecodeBudget> {
    Mutex::new(DecodeBudget::new())
}

fn fresh_decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    decode(bytes, Instant::now(), real_seconds, &budget())
}

fn left(budget: &Mutex<DecodeBudget>) -> f64 {
    budget.lock().unwrap().left
}

thread_local! {
    pub(crate) static ENCODES: Cell<usize> = const { Cell::new(0) };
    pub(crate) static ENCODE_PANICS: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn png_file_of(image: &Image) -> Vec<u8> {
    let pixels = unsafe { std::slice::from_raw_parts(image.data, image.len) };
    png_file(image, pixels, usize::MAX).unwrap()
}

pub(crate) fn encoded_len(image: &Image) -> usize {
    encode(image, usize::MAX / 2).unwrap().len()
}

pub(crate) fn png_as(color: png::ColorType, depth: png::BitDepth, width: u32, height: u32, row: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        if color == png::ColorType::Indexed {
            encoder.set_palette(vec![1, 2, 3, 4, 5, 6]);
        }
        let mut writer = encoder.write_header().unwrap();
        let mut stream = writer.stream_writer().unwrap();
        for _ in 0..height {
            std::io::Write::write_all(&mut stream, row).unwrap();
        }
        stream.finish().unwrap();
    }
    out
}

pub(crate) fn png_of(width: u32, height: u32) -> Vec<u8> {
    png_as(png::ColorType::Rgba, png::BitDepth::Eight, width, height, &vec![7u8; width as usize * 4])
}

/// A valid header and no pixels: all it takes to claim them.
fn claiming(width: u32, height: u32) -> Vec<u8> {
    let mut file = Vec::new();
    let mut encoder = png::Encoder::new(&mut file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().finish().unwrap();
    file
}

#[test]
fn every_png_format_comes_out_as_rgba() {
    use png::{BitDepth::*, ColorType::*};
    let decoded = |color, depth, row: &[u8]| fresh_decode(&png_as(color, depth, 1, 1, row)).unwrap().2;
    assert_eq!(decoded(Rgba, Eight, &[1, 2, 3, 4]), [1, 2, 3, 4]);
    assert_eq!(decoded(Rgb, Eight, &[1, 2, 3]), [1, 2, 3, 255]);
    assert_eq!(decoded(Grayscale, Eight, &[9]), [9, 9, 9, 255]);
    assert_eq!(decoded(GrayscaleAlpha, Eight, &[9, 4]), [9, 9, 9, 4]);
    assert_eq!(decoded(Indexed, Eight, &[1]), [4, 5, 6, 255]);
    assert_eq!(decoded(Rgba, Sixteen, &[1, 0, 2, 0, 3, 0, 4, 0]), [1, 2, 3, 4]);
    let two = png_as(Grayscale, Eight, 2, 1, &[9, 8]);
    assert_eq!(fresh_decode(&two).unwrap(), (2, 1, vec![9, 9, 9, 255, 8, 8, 8, 255]));
}

#[test]
fn a_cut_placement_starts_where_the_screen_does_and_keeps_only_what_shows() {
    let vt = |p: &ImagePlacement| String::from_utf8(direct_vt(p, (10, 20))).unwrap();
    // 400x200 shown over 40x10 cells, two rows gone: 40 of 200 pixels.
    let base = ImagePlacement {
        image: 1,
        row: -2,
        col: 3,
        cells: (40, 10),
        pixels: (400, 200),
        source: [0, 0, 400, 200],
        ..Default::default()
    };
    let cut = vt(&base);
    assert!(cut.starts_with("\x1b[1;4H"), "{cut}");
    assert!(cut.contains(",x=0,y=40,w=400,h=160,X=0,Y=0,"), "{cut}");
    assert!(!cut.contains(",c=") && !cut.contains(",r="), "native size stays native: {cut}");

    // Its pixel offset was part of what went: 14 of 20 pixels for one row.
    let offset = ImagePlacement { row: -1, offset: (5, 6), pixels: (400, 200), ..base };
    assert!(vt(&offset).contains(",y=14,w=400,h=186,X=5,Y=0,"), "{}", vt(&offset));

    // At half scale, what's cut scales too.
    let scaled = ImagePlacement { source: [0, 100, 800, 400], ..base };
    assert!(vt(&scaled).contains(",y=180,w=800,h=320,"), "{}", vt(&scaled));

    // Never all of it: a placement this reports as visible keeps a pixel.
    let nearly = ImagePlacement { row: -10, ..base };
    assert!(vt(&nearly).contains(",y=199,w=400,h=1,"), "{}", vt(&nearly));
    let far_off = ImagePlacement { row: i32::MIN, ..base };
    assert!(vt(&far_off).contains(",h=1,"), "however far, without overflowing: {}", vt(&far_off));

    // A side not given follows from what's left, keeping its shape.
    let rows_only = ImagePlacement { lines: 10, ..base };
    assert!(vt(&rows_only).contains(",r=8,") && !vt(&rows_only).contains(",c="), "{}", vt(&rows_only));
    let columns_only = ImagePlacement { columns: 40, ..base };
    assert!(vt(&columns_only).contains(",c=40,") && !vt(&columns_only).contains(",r="), "{}", vt(&columns_only));

    assert!(!cut.contains(",p="), "{cut}");
    let named = ImagePlacement { id: 7, ..base };
    assert!(vt(&named).contains("i=1,p=7,"), "{}", vt(&named));

    let from_the_side = ImagePlacement { source: [7, 0, 393, 200], ..base };
    assert!(vt(&from_the_side).contains(",x=7,y=40,w=393,"), "{}", vt(&from_the_side));

    let uncut = ImagePlacement { row: 2, offset: (5, 6), lines: 10, ..base };
    assert!(vt(&uncut).starts_with("\x1b[3;4H") && vt(&uncut).contains(",X=5,Y=6,r=10,"), "{}", vt(&uncut));
}

#[test]
fn chunks_say_more_follow_until_the_last_whatever_the_length() {
    let more_of = |len: usize| -> Vec<bool> {
        let out = transmission(1, &"A".repeat(len));
        out.split(|b| *b == 0x1b)
            .filter(|part| part.starts_with(b"_G"))
            .map(|part| part.windows(4).any(|w| w == b"m=1;"))
            .collect()
    };
    assert_eq!(more_of(100), [false]);
    assert_eq!(more_of(CHUNK), [false]);
    assert_eq!(more_of(CHUNK + 1), [true, false]);
    assert_eq!(more_of(CHUNK * 2), [true, false]);
}

#[test]
fn a_placeholder_placement_says_all_the_program_said_and_nothing_else() {
    let vt = |p: &ImagePlacement| String::from_utf8(placeholder_vt(p)).unwrap();
    let plain = ImagePlacement { image: 1, virtual_: true, ..Default::default() };
    assert_eq!(vt(&plain), "\x1b_Ga=p,U=1,i=1,q=2\x1b\\");
    let full = ImagePlacement { id: 7, columns: 3, lines: 4, given_source: [1, 2, 5, 6], z: -3, ..plain };
    assert_eq!(vt(&full), "\x1b_Ga=p,U=1,i=1,p=7,c=3,r=4,x=1,y=2,w=5,h=6,z=-3,q=2\x1b\\");
}

#[test]
fn a_png_has_to_fit_to_its_very_last_byte() {
    let pixels = [1, 2, 3, 4];
    let image = Image { id: 1, width: 1, height: 1, format: 1, generation: 1, data: pixels.as_ptr(), len: 4 };
    let length = png_file(&image, &pixels, 1024).unwrap().len();
    assert!(png_file(&image, &pixels, length - 1).is_none());
}

#[test]
fn a_placeholder_placement_keeps_the_part_of_the_image_it_shows() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    let pixels = vec![7; 20 * 20 * 4];
    t.write(
        format!("\x1b_Ga=T,f=32,s=20,v=20,i=1,U=1,c=4,r=2,x=2,y=3,w=8,h=9,C=1,q=2;{}\x1b\\", base64(&pixels))
            .as_bytes(),
    );
    let paint = t.paint(0);
    assert!(paint.windows(b",x=2,y=3,w=8,h=9,".len()).any(|w| w == b",x=2,y=3,w=8,h=9,"));
}

#[test]
fn a_file_claiming_more_than_a_screen_keeps_or_ghostty_shows_is_refused_before_it_costs_anything() {
    let budget = budget();
    let now = Instant::now();
    assert!(decode(&claiming(4097, 4096), now, real_seconds, &budget).is_none());
    assert!(decode(&claiming(1, GHOSTTY_MAX_SIDE + 1), now, real_seconds, &budget).is_none());
    assert!(decode(&claiming(GHOSTTY_MAX_SIDE + 1, 1), now, real_seconds, &budget).is_none());
    assert_eq!(left(&budget), DECODE_SAVED);
    assert!(decode(&claiming(4096, 4096), now, real_seconds, &budget).is_none());
    assert!(left(&budget) < DECODE_SAVED, "this one was worth trying, and is charged");
}

#[test]
fn decoding_comes_out_of_a_budget_that_time_refills_up_to_what_can_be_saved() {
    let (file, budget, start) = (png_of(1, 1), budget(), Instant::now());
    assert!(decode(&file, start, |_| DECODE_SAVED, &budget).is_some());
    assert!(decode(&file, start, |_| 0.0, &budget).is_none(), "spent");
    assert!(decode(&file, start + Duration::from_millis(10), |_| 0.0, &budget).is_some(), "earned a little back");
    budget.lock().unwrap().earn(start + Duration::from_secs(3600));
    assert_eq!(left(&budget), DECODE_SAVED, "an hour's rest is still only so much");
    budget.lock().unwrap().left = 0.0;
    budget.lock().unwrap().earn(start + Duration::from_secs(3601));
    assert!((left(&budget) - DECODE_SHARE).abs() < 1e-9, "a second earns a share of it");
}

#[test]
fn decoding_is_charged_for_what_it_really_took_even_when_it_fails() {
    let charged = |file: &[u8], decoded: bool| {
        let budget = budget();
        assert_eq!(decode(file, Instant::now(), |_| 0.15, &budget).is_some(), decoded);
        assert!((DECODE_SAVED - left(&budget) - 0.15).abs() < 1e-9);
    };
    let mut file = png_of(64, 64);
    charged(&file, true);
    let idat = file.windows(4).position(|w| w == b"IDAT").unwrap();
    file[idat + 8] ^= 0xff;
    charged(&file, false);
}

#[test]
fn a_real_write_draws_on_the_budget() {
    let mut t = crate::Terminal::new(80, 24, 0);
    DECODE_BUDGET.lock().unwrap().at = None;
    let file = base64(&png_of(4, 2));
    t.write(format!("\x1b_Ga=T,f=100,i=1,q=2;{file}\x1b\\").as_bytes());
    assert!(DECODE_BUDGET.lock().unwrap().at.is_some());
}
