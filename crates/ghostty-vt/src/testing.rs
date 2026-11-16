//! What tests of this crate and of those using it all need.

use crate::Terminal;
pub use crate::kitty::{ImagePlacement, base64};

impl Terminal {
    /// The active screen's placements, as the screen holds them.
    pub fn image_placements(&self) -> Vec<ImagePlacement> {
        self.placements()
    }

    /// An image's pixels, for comparing screens.
    pub fn image_pixels(&self, id: u32) -> Option<Vec<u8>> {
        let image = self.image(id)?;
        (!image.data.is_null()).then(|| unsafe { std::slice::from_raw_parts(image.data, image.len) }.to_vec())
    }

    #[cfg(test)]
    pub(crate) fn images_within(&self, budget: usize) -> crate::kitty::Replay {
        self.images_walk(budget)
    }
}

/// A kitty transmission in the 4096-byte chunks programs use.
pub fn chunked(control: &str, payload: &[u8]) -> Vec<u8> {
    let body = base64(payload);
    let parts: Vec<&[u8]> = body.as_bytes().chunks(4096).collect();
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let more = (i + 1 < parts.len()) as u8;
        let head = if i == 0 { format!("{control},m={more}") } else { format!("m={more}") };
        out.extend(format!("\x1b_G{head};").bytes());
        out.extend_from_slice(part);
        out.extend(b"\x1b\\");
    }
    out
}

/// Bytes nothing compresses, and different ones for each seed.
pub fn noise_from(seed: u64, bytes: usize) -> Vec<u8> {
    let mut s = 88172645463325252u64 + seed;
    (0..bytes)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

pub fn noise(bytes: usize) -> Vec<u8> {
    noise_from(0, bytes)
}

/// Four cells of a placeholder image, whose id its colour says: 2.
pub fn placeholder_cells() -> String {
    let marks = ["\u{305}", "\u{30d}", "\u{30e}", "\u{310}"];
    let mut cells = String::from("\x1b[38;5;2m");
    for mark in marks {
        cells.push('\u{10eeee}');
        cells.push_str(marks[0]);
        cells.push_str(mark);
    }
    cells + "\x1b[39m"
}

pub type Placed = (u32, bool, i32, i32, (u32, u32), (u32, u32), [u32; 4], i32);

/// What a viewer shows of each placement. Implicit ids differ by viewer, and no program can name them.
pub fn placed(t: &Terminal) -> Vec<Placed> {
    let mut all: Vec<Placed> = t
        .image_placements()
        .iter()
        .map(|p| (p.image.min(2147483647), p.virtual_, p.row, p.col, p.cells, p.pixels, p.source, p.z))
        .collect();
    all.sort_by_key(|p| (p.0, p.2, p.3));
    all
}

/// What a viewer decodes from a replay is RGBA, whichever the screen keeps.
pub fn same_image(screen: &[u8], viewer: &[u8]) -> bool {
    if screen.len() == viewer.len() {
        return screen == viewer;
    }
    screen.len() * 4 == viewer.len() * 3
        && screen.chunks(3).zip(viewer.chunks(4)).all(|(s, v)| s == &v[..3] && v[3] == 255)
}

pub fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack.windows(needle.len()).filter(|w| *w == needle).count()
}

/// A PNG of one colour.
pub fn png_filled(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let pixels: Vec<u8> = rgba.iter().copied().cycle().take((width * height * 4) as usize).collect();
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
    out
}
