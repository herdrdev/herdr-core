//! Kitty images in the core's screen: PNG decoding, so the screen holds what
//! viewers show, and the commands that put those images back on a viewer.

use super::{Terminal, ffi};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Mutex;
use std::time::Instant;

/// Image data one screen keeps. Past it the oldest images go; a bigger one is refused.
pub(crate) const STORAGE_BYTES: u64 = 64 << 20;
/// Encoded image commands one paint may carry. 5 MiB is the least that replays a full-screen
/// photo: real 1080p ones came to 2.8-4.7 MiB as commands (PNG in base64), and at 4 MiB the
/// largest came back as no picture at all. UI and text screenshots are far smaller (0.3 MiB at
/// 2560x1600). A paint goes out whole, so viewers pay for this in waiting: keys typed behind a
/// full one echo after 40 ms on a socket, 0.5 s on a 10 MiB/s line and 2.4 s on a 2 MiB/s one.
/// Each viewer also holds its copy in memory until it has read it. A larger budget stops paying:
/// a 4K photo came to 13 MiB, over 6 s of typing on that slow line, so it stays out, and its
/// viewers get the text without it.
pub(crate) const PAINT_BYTES: usize = 5 << 20;
/// Images and placements one paint may carry, however small: replaying each costs time under the lock.
const PAINT_IMAGES: usize = 256;
const PAINT_PLACEMENTS: usize = 1024;
/// Ends a chunked load a viewer may still have open (a dropped backlog):
/// otherwise it would take the next transmission for its continuation.
pub(crate) const END_LOAD: &[u8] = b"\x1b_Gq=2,m=0;\x1b\\";
const CHUNK: usize = 4096;
const OPT_DECODE_PNG: i32 = 1;
const DATA_KITTY_GRAPHICS: i32 = 30;
const GRAPHICS_PLACEMENTS: i32 = 1;
const PLACEMENT_IMAGE: i32 = 1;
const PLACEMENT_ID: i32 = 2;
const PLACEMENT_VIRTUAL: i32 = 3;
const PLACEMENT_X_OFFSET: i32 = 4;
const PLACEMENT_Y_OFFSET: i32 = 5;
const PLACEMENT_SOURCE: i32 = 6; // x, y, width, height in a row
const PLACEMENT_COLUMNS: i32 = 10;
const PLACEMENT_ROWS: i32 = 11;
const PLACEMENT_Z: i32 = 12;
const IMAGE_WIDTH: i32 = 3;
const IMAGE_HEIGHT: i32 = 4;
const IMAGE_FORMAT: i32 = 5;
const IMAGE_DATA: i32 = 7;
const IMAGE_LEN: i32 = 8;
const IMAGE_GENERATION: i32 = 9;
const FORMAT_RGB: i32 = 0;
/// Share of all time decoding may take under the lock, and how much of it can be saved up.
const DECODE_SHARE: f64 = 0.4;
const DECODE_SAVED: f64 = 0.4;
/// Ghostty refuses an image longer than this on either side, but only after decoding it.
const GHOSTTY_MAX_SIDE: u32 = 10000;
/// A debug build takes this much longer, and shouldn't refuse what a release wouldn't.
const DECODE_BUILD: f64 = if cfg!(debug_assertions) { 30.0 } else { 1.0 };

#[repr(C)]
struct SysImage {
    width: u32,
    height: u32,
    data: *mut u8,
    data_len: usize,
}

#[repr(C)]
#[derive(Default)]
struct RenderInfo {
    size: usize,
    pixel_width: u32,
    pixel_height: u32,
    grid_cols: u32,
    grid_rows: u32,
    viewport_col: i32,
    viewport_row: i32,
    viewport_visible: bool,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
}

unsafe extern "C" {
    fn ghostty_sys_set(option: i32, value: *const c_void) -> i32;
    fn ghostty_alloc(alloc: *const c_void, len: usize) -> *mut u8;
    fn ghostty_kitty_graphics_get(graphics: *mut c_void, data: i32, out: *mut c_void) -> i32;
    fn ghostty_kitty_graphics_image(graphics: *mut c_void, id: u32) -> *const c_void;
    fn ghostty_kitty_graphics_image_get(image: *const c_void, data: i32, out: *mut c_void) -> i32;
    fn ghostty_kitty_graphics_placement_iterator_new(alloc: *const c_void, out: *mut *mut c_void) -> i32;
    fn ghostty_kitty_graphics_placement_iterator_free(iterator: *mut c_void);
    fn ghostty_kitty_graphics_placement_next(iterator: *mut c_void) -> bool;
    fn ghostty_kitty_graphics_placement_get(iterator: *mut c_void, data: i32, out: *mut c_void) -> i32;
    fn ghostty_kitty_graphics_placement_render_info(
        iterator: *mut c_void,
        image: *const c_void,
        terminal: *mut c_void,
        out: *mut RenderInfo,
    ) -> i32;
}

/// Decoding runs inside `vt_write`, under the lock every terminal waits on, and a 2 KiB file
/// can claim 64 MiB of pixels and take far longer than its size suggests: past this budget,
/// PNGs are refused for a while. A refused image still reaches attached viewers; only the
/// screen goes without it. All terminals share it, charged by the clock: nothing in the file
/// can make a decode cheaper than that. One may overdraw it, and then pays that back first.
#[derive(Clone, Copy)]
pub(crate) struct DecodeBudget {
    left: f64,
    at: Option<Instant>,
}

impl DecodeBudget {
    pub(crate) const fn new() -> DecodeBudget {
        DecodeBudget { left: DECODE_SAVED, at: None }
    }

    fn earn(&mut self, now: Instant) {
        let since = self.at.map_or(0.0, |at| now.saturating_duration_since(at).as_secs_f64());
        self.left = (self.left + since * DECODE_SHARE).min(DECODE_SAVED);
        self.at = Some(now);
    }
}

static DECODE_BUDGET: Mutex<DecodeBudget> = Mutex::new(DecodeBudget::new());

pub(crate) fn install_decoder() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        ghostty_sys_set(OPT_DECODE_PNG, decode_png as *const c_void);
    });
}

unsafe extern "C" fn decode_png(
    _: *mut c_void,
    alloc: *const c_void,
    data: *const u8,
    len: usize,
    out: *mut SysImage,
) -> bool {
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    // A panic can't cross into libghostty: it would take the whole core with it.
    let decoded =
        catch_unwind(AssertUnwindSafe(|| decode(bytes, Instant::now(), real_seconds, &DECODE_BUDGET))).ok().flatten();
    let Some((width, height, rgba)) = decoded else { return false };
    let ptr = unsafe { ghostty_alloc(alloc, rgba.len()) };
    if ptr.is_null() {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(rgba.as_ptr(), ptr, rgba.len());
        *out = SysImage { width, height, data: ptr, data_len: rgba.len() };
    }
    true
}

fn real_seconds(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() / DECODE_BUILD
}

fn decode(
    bytes: &[u8],
    now: Instant,
    took: impl FnOnce(Instant) -> f64,
    budget: &Mutex<DecodeBudget>,
) -> Option<(u32, u32, Vec<u8>)> {
    let mut decoder = png_decoder(bytes);
    // Before any allocation: a tiny file can claim gigabytes of pixels.
    let header = decoder.read_header_info().ok()?;
    if header.width as u64 * header.height as u64 * 4 > STORAGE_BYTES
        || header.width.max(header.height) > GHOSTTY_MAX_SIDE
    {
        return None;
    }
    let budget = || budget.lock().unwrap_or_else(|p| p.into_inner());
    budget().earn(now);
    if budget().left <= 0.0 {
        return None;
    }
    let started = Instant::now();
    let decoded = decode_pixels(decoder);
    budget().left -= took(started);
    decoded
}

fn png_decoder(bytes: &[u8]) -> png::Decoder<std::io::Cursor<&[u8]>> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Metadata no one reads.
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    decoder
}

fn decode_pixels(mut decoder: png::Decoder<std::io::Cursor<&[u8]>>) -> Option<(u32, u32, Vec<u8>)> {
    // Every format comes out as one of the two below.
    decoder.set_transformations(png::Transformations::STRIP_16 | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => return None,
    };
    Some((info.width, info.height, rgba))
}

/// One placement as the screen holds it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImagePlacement {
    pub image: u32,
    pub id: u32,
    pub virtual_: bool,
    pub visible: bool,
    /// Viewport position, negative when partly off the top.
    pub row: i32,
    pub col: i32,
    /// Cells and pixels it takes up on screen, as worked out.
    pub cells: (u32, u32),
    pub pixels: (u32, u32),
    /// As the program gave them; 0 for "from the image's size".
    pub columns: u32,
    pub lines: u32,
    pub offset: (u32, u32),
    /// The part of the image shown, resolved, and as the program gave it.
    pub source: [u32; 4],
    pub given_source: [u32; 4],
    pub z: i32,
}

pub(crate) struct Image {
    id: u32,
    width: u32,
    height: u32,
    format: i32,
    generation: u64,
    pub(crate) data: *const u8,
    pub(crate) len: usize,
}

/// What a paint learned about an image, so later ones don't encode it again.
pub(crate) enum Encoded {
    Commands(Vec<u8>),
    /// Didn't fit; at least this many bytes.
    TooBig(usize),
}

/// Commands that put a screen's images on a viewer.
#[derive(Default)]
pub(crate) struct Replay {
    /// Need the cursor: they go before the screen's modes are final.
    pub direct: Vec<u8>,
    /// Don't: they follow the whole screen, so it never waits for large ones. Output after them does.
    pub placeholder: Vec<u8>,
}

impl Terminal {
    fn graphics(&self) -> *mut c_void {
        let mut graphics: *mut c_void = ptr::null_mut();
        unsafe { ffi::ghostty_terminal_get(self.raw, DATA_KITTY_GRAPHICS, (&raw mut graphics).cast()) };
        graphics
    }

    pub(crate) fn image(&self, id: u32) -> Option<Image> {
        let handle = unsafe { ghostty_kitty_graphics_image(self.graphics(), id) };
        if handle.is_null() {
            return None;
        }
        let mut image = Image { id, width: 0, height: 0, format: 0, generation: 0, data: ptr::null(), len: 0 };
        unsafe {
            ghostty_kitty_graphics_image_get(handle, IMAGE_WIDTH, (&raw mut image.width).cast());
            ghostty_kitty_graphics_image_get(handle, IMAGE_HEIGHT, (&raw mut image.height).cast());
            ghostty_kitty_graphics_image_get(handle, IMAGE_FORMAT, (&raw mut image.format).cast());
            ghostty_kitty_graphics_image_get(handle, IMAGE_DATA, (&raw mut image.data).cast());
            ghostty_kitty_graphics_image_get(handle, IMAGE_LEN, (&raw mut image.len).cast());
            ghostty_kitty_graphics_image_get(handle, IMAGE_GENERATION, (&raw mut image.generation).cast());
        }
        Some(image)
    }

    /// The active screen's placements.
    pub(crate) fn placements(&self) -> Vec<ImagePlacement> {
        let graphics = self.graphics();
        let mut out = Vec::new();
        unsafe {
            let mut it = ptr::null_mut();
            ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut it);
            ghostty_kitty_graphics_get(graphics, GRAPHICS_PLACEMENTS, (&raw mut it).cast());
            while ghostty_kitty_graphics_placement_next(it) {
                let mut p = ImagePlacement::default();
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_IMAGE, (&raw mut p.image).cast());
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_ID, (&raw mut p.id).cast());
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_VIRTUAL, (&raw mut p.virtual_).cast());
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_X_OFFSET, (&raw mut p.offset.0).cast());
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_Y_OFFSET, (&raw mut p.offset.1).cast());
                for (i, value) in p.given_source.iter_mut().enumerate() {
                    ghostty_kitty_graphics_placement_get(it, PLACEMENT_SOURCE + i as i32, (value as *mut u32).cast());
                }
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_COLUMNS, (&raw mut p.columns).cast());
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_ROWS, (&raw mut p.lines).cast());
                ghostty_kitty_graphics_placement_get(it, PLACEMENT_Z, (&raw mut p.z).cast());
                let handle = ghostty_kitty_graphics_image(graphics, p.image);
                let mut info = RenderInfo { size: size_of::<RenderInfo>(), ..Default::default() };
                if !handle.is_null() {
                    ghostty_kitty_graphics_placement_render_info(it, handle, self.raw, &mut info);
                }
                p.visible = info.viewport_visible;
                (p.row, p.col) = (info.viewport_row, info.viewport_col);
                p.cells = (info.grid_cols, info.grid_rows);
                p.pixels = (info.pixel_width, info.pixel_height);
                p.source = [info.source_x, info.source_y, info.source_width, info.source_height];
                out.push(p);
            }
            ghostty_kitty_graphics_placement_iterator_free(it);
        }
        out
    }

    /// Whether a paint without images would leave any out.
    pub(crate) fn has_images(&self) -> bool {
        self.placements().iter().any(|p| p.virtual_ || p.visible)
    }

    /// The commands for the images on screen, within `budget` bytes. Each image goes whole
    /// or not at all: direct ones on screen first, then placeholder ones, newest first in
    /// each, as the screen itself evicts.
    pub(crate) fn images_walk(&self, budget: usize) -> Replay {
        let placements = self.placements();
        let newest = |p: &&ImagePlacement| Reverse(self.image(p.image).map_or(0, |i| i.generation));
        let mut direct: Vec<&ImagePlacement> = placements.iter().filter(|p| !p.virtual_ && p.visible).collect();
        let mut placeholder: Vec<&ImagePlacement> = placements.iter().filter(|p| p.virtual_).collect();
        direct.sort_by_cached_key(newest);
        placeholder.sort_by_cached_key(newest);
        let mut cache = self.encoded[self.alt_screen() as usize].borrow_mut();
        let mut kept = HashMap::new();
        let mut left = budget;
        // Encoding is under the lock: past this much tried in one paint, the rest waits for another.
        let mut effort = PAINT_BYTES;
        let mut placements_left = PAINT_PLACEMENTS;
        let mut sent = HashSet::new();
        let mut refused = HashSet::new();
        let mut replay = Replay::default();
        let cell = self.hooks.cell;
        for (list, out) in [(&direct, &mut replay.direct), (&placeholder, &mut replay.placeholder)] {
            let mut placed = Vec::new();
            let mut newest_first = Vec::new();
            for p in list.iter() {
                if !sent.contains(&p.image) {
                    if refused.contains(&p.image) {
                        continue;
                    }
                    let fits = sent.len() < PAINT_IMAGES
                        && self.image(p.image).is_some_and(|image| {
                            let fits = transmit(&image, &mut cache, &mut kept, &mut left, &mut effort);
                            if fits {
                                newest_first.push(image.generation);
                            }
                            fits
                        });
                    if !fits {
                        refused.insert(p.image);
                        continue;
                    }
                    sent.insert(p.image);
                }
                let command = if p.virtual_ { placeholder_vt(p) } else { direct_vt(p, cell) };
                if placements_left > 0 && command.len() <= left {
                    placements_left -= 1;
                    left -= command.len();
                    placed.extend(command);
                }
            }
            // Oldest first: a viewer short of room then lets the same ones go as the screen did.
            for generation in newest_first.iter().rev() {
                if let Some(Encoded::Commands(commands)) = kept.get(generation) {
                    out.extend_from_slice(commands);
                }
            }
            out.extend(placed);
        }
        *cache = kept;
        replay
    }
}

fn transmit(
    image: &Image,
    cache: &mut HashMap<u64, Encoded>,
    kept: &mut HashMap<u64, Encoded>,
    left: &mut usize,
    effort: &mut usize,
) -> bool {
    let commands = match cache.remove(&image.generation) {
        Some(Encoded::Commands(commands)) => Some(commands),
        Some(Encoded::TooBig(size)) if size > *left => {
            kept.insert(image.generation, Encoded::TooBig(size));
            return false;
        }
        _ if *effort == 0 => return false,
        _ => {
            let limit = (*left).min(*effort);
            let commands = catch_unwind(AssertUnwindSafe(|| encode(image, limit))).ok().flatten();
            *effort -= commands.as_ref().map_or(limit, |commands| commands.len().min(limit));
            let Some(commands) = commands else {
                kept.insert(image.generation, Encoded::TooBig(limit + 1));
                return false;
            };
            Some(commands)
        }
    };
    let Some(commands) = commands else { return false };
    if commands.len() > *left {
        kept.insert(image.generation, Encoded::TooBig(commands.len()));
        return false;
    }
    *left -= commands.len();
    kept.insert(image.generation, Encoded::Commands(commands));
    true
}

/// Stops an encoder as soon as what it wrote can't fit.
struct Within {
    out: Vec<u8>,
    limit: usize,
}

impl Write for Within {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.out.len() + bytes.len() > self.limit {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        self.out.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Pixels as a PNG. Row filtering makes photos a third smaller than zlib alone.
/// zlib-rs, not miniz: Ghostty rejects a block with no distance codes.
fn png_file(image: &Image, pixels: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut file = Within { out: Vec::new(), limit };
    let mut encoder = png::Encoder::new(&mut file, image.width, image.height);
    encoder.set_color(if image.format == FORMAT_RGB { png::ColorType::Rgb } else { png::ColorType::Rgba });
    encoder.set_filter(png::Filter::Paeth);
    encoder.set_deflate_compression(png::DeflateCompression::Level(2));
    let mut writer = encoder.write_header().ok()?;
    let mut stream = writer.stream_writer().ok()?;
    stream.write_all(pixels).ok()?;
    stream.finish().ok()?;
    writer.finish().ok()?;
    Some(file.out)
}

/// The `a=t` commands for one image. `None`: past `limit`, known before the end when it's far past.
fn encode(image: &Image, limit: usize) -> Option<Vec<u8>> {
    #[cfg(test)]
    tests::ENCODES.with(|n| n.set(n.get() + 1));
    #[cfg(test)]
    assert!(!tests::ENCODE_PANICS.get());
    if image.data.is_null() {
        return None;
    }
    let pixels = unsafe { std::slice::from_raw_parts(image.data, image.len) };
    Some(transmission(image.id, &base64(&png_file(image, pixels, limit / 4 * 3)?)))
}

fn transmission(id: u32, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + body.len() / CHUNK * 16 + 64);
    for (i, chunk) in body.as_bytes().chunks(CHUNK).enumerate() {
        let more = ((i + 1) * CHUNK < body.len()) as u8;
        if i == 0 {
            out.extend(format!("\x1b_Ga=t,f=100,i={id},q=2,m={more};").bytes());
        } else {
            out.extend(format!("\x1b_Gq=2,m={more};").bytes());
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// A direct placement at its place. `C=1`: the cursor stays put, and the screen
/// it landed on doesn't scroll.
fn direct_vt(p: &ImagePlacement, cell: (u32, u32)) -> Vec<u8> {
    // Ghostty keeps an offset inside the cell, whatever cell it was given for.
    let offset = (p.offset.0.min(cell.0.saturating_sub(1)), p.offset.1.min(cell.1.saturating_sub(1)));
    let (x, width) = (p.source[0], p.source[2]);
    let (mut row, mut y_offset, mut y, mut height) = (p.row, offset.1, p.source[1], p.source[3]);
    let (col, columns, mut lines) = (p.col.max(0), p.columns, p.lines);
    if p.row < 0 {
        // Partly off the top: only what stays is sent, as it can't start above the screen.
        let hidden = p.row.unsigned_abs();
        let gone = hidden.saturating_mul(cell.1.max(1)).saturating_sub(offset.1);
        let cut = ((height as u64 * gone as u64 / p.pixels.1.max(1) as u64) as u32).min(height.saturating_sub(1));
        (row, y_offset, y, height) = (0, 0, y + cut, height - cut);
        lines = lines.saturating_sub(hidden);
    }
    let mut size = String::new();
    if columns > 0 {
        size.push_str(&format!(",c={columns}"));
    }
    if lines > 0 {
        size.push_str(&format!(",r={lines}"));
    }
    let id = if p.id != 0 { format!(",p={}", p.id) } else { String::new() };
    format!(
        "\x1b[{};{}H\x1b_Ga=p,i={}{id},x={x},y={y},w={width},h={height},X={},Y={y_offset}{size},z={},C=1,q=2\x1b\\",
        row + 1,
        col + 1,
        p.image,
        offset.0,
        p.z,
    )
    .into_bytes()
}

fn placeholder_vt(p: &ImagePlacement) -> Vec<u8> {
    let mut out = format!("\x1b_Ga=p,U=1,i={}", p.image);
    if p.id != 0 {
        out.push_str(&format!(",p={}", p.id));
    }
    if p.columns > 0 {
        out.push_str(&format!(",c={}", p.columns));
    }
    if p.lines > 0 {
        out.push_str(&format!(",r={}", p.lines));
    }
    if p.given_source != [0; 4] {
        let [x, y, width, height] = p.given_source;
        out.push_str(&format!(",x={x},y={y},w={width},h={height}"));
    }
    if p.z != 0 {
        out.push_str(&format!(",z={}", p.z));
    }
    out.push_str(",q=2\x1b\\");
    out.into_bytes()
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { TABLE[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests;
