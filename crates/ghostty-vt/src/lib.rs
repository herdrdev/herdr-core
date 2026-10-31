//! The few libghostty-vt calls herdr-core needs, behind a safe `Terminal`.

use std::ffi::c_void;
use std::ptr;

mod ffi {
    use std::ffi::c_void;

    pub type Terminal = *mut c_void;
    pub type Formatter = *mut c_void;
    pub type Decoder = *mut c_void;
    pub type KeyEncoder = *mut c_void;
    pub type KeyEvent = *mut c_void;
    pub type MouseEncoder = *mut c_void;
    pub type MouseEvent = *mut c_void;

    #[repr(C)]
    pub struct MouseSize {
        pub size: usize,
        pub screen_width: u32,
        pub screen_height: u32,
        pub cell_width: u32,
        pub cell_height: u32,
        pub padding_top: u32,
        pub padding_bottom: u32,
        pub padding_right: u32,
        pub padding_left: u32,
    }

    #[repr(C)]
    pub struct MousePosition {
        pub x: f32,
        pub y: f32,
    }

    pub const SUCCESS: i32 = 0;
    pub const OPT_USERDATA: i32 = 0;
    pub const OPT_WRITE_PTY: i32 = 1;
    pub const OPT_XTVERSION: i32 = 4;
    pub const OPT_TITLE_CHANGED: i32 = 5;
    pub const OPT_PROGRAM_STATUS: i32 = 46;
    pub const OPT_SIZE: i32 = 6;
    pub const OPT_COLOR_SCHEME: i32 = 7;
    pub const OPT_DEVICE_ATTRIBUTES: i32 = 8;
    pub const OPT_COLOR_FOREGROUND: i32 = 11;
    pub const OPT_COLOR_BACKGROUND: i32 = 12;
    pub const OPT_COLOR_CURSOR: i32 = 13;
    pub const OPT_SCROLLBACK_MAX_BYTES: i32 = 27;
    pub const OPT_CONTINUATION_MAX_BYTES: i32 = 31;
    pub const DATA_COLS: i32 = 1;
    pub const DATA_ROWS: i32 = 2;
    pub const DATA_ACTIVE_SCREEN: i32 = 6;
    pub const DATA_SCROLLBAR: i32 = 9;
    pub const DATA_TITLE: i32 = 12;
    pub const DATA_MODE: i32 = 37;
    pub const FORMAT_PLAIN: i32 = 0;
    pub const FORMAT_VT: i32 = 1;
    pub const POINT_ACTIVE: i32 = 0;
    pub const POINT_HISTORY: i32 = 3;

    #[repr(C)]
    #[derive(Default)]
    pub struct ScreenExtra {
        pub size: usize,
        pub cursor: bool,
        pub style: bool,
        pub hyperlink: bool,
        pub protection: bool,
        pub kitty_keyboard: bool,
        pub charsets: bool,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct TerminalExtra {
        pub size: usize,
        pub palette: bool,
        pub modes: bool,
        pub scrolling_region: bool,
        pub tabstops: bool,
        pub pwd: bool,
        pub keyboard: bool,
        pub screen: ScreenExtra,
    }

    #[repr(C)]
    pub struct FormatterOptions {
        pub size: usize,
        pub emit: i32,
        pub unwrap: bool,
        pub trim: bool,
        pub extra: TerminalExtra,
        pub selection: *const Selection,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct GridRef {
        pub size: usize,
        pub node: *mut c_void,
        pub x: u16,
        pub y: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Coord {
        pub x: u16,
        pub y: u32,
    }

    #[repr(C)]
    pub union PointValue {
        pub coord: Coord,
        pub _pad: [u64; 2],
    }

    #[repr(C)]
    pub struct Point {
        pub tag: i32,
        pub value: PointValue,
    }

    #[repr(C)]
    pub struct Selection {
        pub size: usize,
        pub start: GridRef,
        pub end: GridRef,
        pub rectangle: bool,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Scrollbar {
        pub total: u64,
        pub offset: u64,
        pub len: u64,
    }

    #[repr(C)]
    pub struct Str {
        pub ptr: *const u8,
        pub len: usize,
    }

    #[repr(C)]
    pub struct SizeReport {
        pub rows: u16,
        pub columns: u16,
        pub cell_width: u32,
        pub cell_height: u32,
    }

    #[repr(C)]
    pub struct DeviceAttributes {
        pub conformance_level: u16,
        pub features: [u16; 64],
        pub num_features: usize,
        pub device_type: u16,
        pub firmware_version: u16,
        pub rom_cartridge: u16,
        pub unit_id: u32,
    }

    #[repr(C)]
    pub struct ProgramStatus {
        pub size: usize,
        pub state: i32,
        pub kind: i32,
        pub progress: i8,
        pub id: Str,
        pub app: Str,
        pub title: Str,
        pub message: Str,
    }

    #[repr(C)]
    pub struct ModeConfig {
        pub mode: u16,
        pub value: bool,
    }

    unsafe extern "C" {
        pub fn ghostty_terminal_new(alloc: *const c_void, out: *mut Terminal, cols: u16, rows: u16) -> i32;
        pub fn ghostty_terminal_free(t: Terminal);
        pub fn ghostty_terminal_set(t: Terminal, opt: i32, value: *const c_void) -> i32;
        pub fn ghostty_terminal_get(t: Terminal, data: i32, out: *mut c_void) -> i32;
        pub fn ghostty_terminal_vt_write(t: Terminal, data: *const u8, len: usize);
        pub fn ghostty_terminal_resize(t: Terminal, cols: u16, rows: u16, cw: u32, ch: u32) -> i32;
        pub fn ghostty_terminal_grid_ref(t: Terminal, point: Point, out: *mut GridRef) -> i32;
        pub fn ghostty_formatter_terminal_new(
            alloc: *const c_void,
            out: *mut Formatter,
            t: Terminal,
            opts: FormatterOptions,
        ) -> i32;
        pub fn ghostty_formatter_format_alloc(
            f: Formatter,
            alloc: *const c_void,
            out: *mut *mut u8,
            len: *mut usize,
        ) -> i32;
        pub fn ghostty_formatter_free(f: Formatter);
        pub fn ghostty_free(alloc: *const c_void, ptr: *mut u8, len: usize);
        pub fn ghostty_snapshot_encode_alloc(
            t: Terminal,
            alloc: *const c_void,
            out: *mut *mut u8,
            len: *mut usize,
        ) -> i32;
        pub fn ghostty_snapshot_decoder_new_buf(
            alloc: *const c_void,
            out: *mut Decoder,
            ptr: *const u8,
            len: usize,
        ) -> i32;
        pub fn ghostty_snapshot_decoder_decode(d: Decoder, out: *mut Terminal) -> i32;
        pub fn ghostty_snapshot_decoder_free(d: Decoder);
        pub fn ghostty_key_encoder_new(alloc: *const c_void, out: *mut KeyEncoder) -> i32;
        pub fn ghostty_key_encoder_free(e: KeyEncoder);
        pub fn ghostty_key_encoder_setopt_from_terminal(e: KeyEncoder, t: Terminal);
        pub fn ghostty_key_encoder_setopt(e: KeyEncoder, opt: i32, value: *const c_void);
        pub fn ghostty_key_encoder_encode(
            e: KeyEncoder,
            ev: KeyEvent,
            out: *mut u8,
            cap: usize,
            len: *mut usize,
        ) -> i32;
        pub fn ghostty_key_event_new(alloc: *const c_void, out: *mut KeyEvent) -> i32;
        pub fn ghostty_key_event_free(ev: KeyEvent);
        pub fn ghostty_key_event_set_action(ev: KeyEvent, action: i32);
        pub fn ghostty_key_event_set_key(ev: KeyEvent, key: i32);
        pub fn ghostty_key_event_set_mods(ev: KeyEvent, mods: u16);
        pub fn ghostty_key_event_set_utf8(ev: KeyEvent, utf8: *const u8, len: usize);
        pub fn ghostty_key_event_set_unshifted_codepoint(ev: KeyEvent, cp: u32);
        pub fn ghostty_mouse_encoder_new(alloc: *const c_void, out: *mut MouseEncoder) -> i32;
        pub fn ghostty_mouse_encoder_free(e: MouseEncoder);
        pub fn ghostty_mouse_encoder_setopt(e: MouseEncoder, opt: i32, value: *const c_void);
        pub fn ghostty_mouse_encoder_setopt_from_terminal(e: MouseEncoder, t: Terminal);
        pub fn ghostty_mouse_encoder_encode(
            e: MouseEncoder,
            ev: MouseEvent,
            out: *mut u8,
            cap: usize,
            len: *mut usize,
        ) -> i32;
        pub fn ghostty_mouse_event_new(alloc: *const c_void, out: *mut MouseEvent) -> i32;
        pub fn ghostty_mouse_event_free(ev: MouseEvent);
        pub fn ghostty_mouse_event_set_action(ev: MouseEvent, action: i32);
        pub fn ghostty_mouse_event_set_button(ev: MouseEvent, button: i32);
        pub fn ghostty_mouse_event_set_position(ev: MouseEvent, pos: MousePosition);
    }
}

/// Unfinished escape sequences kept so a snapshot can be taken at any byte.
const CONTINUATION_MAX_BYTES: usize = 64 * 1024;
/// Cell size reported in pixels until a viewer tells us its own.
const DEFAULT_CELL: (u32, u32) = (10, 20);
/// Ghostty's default theme, answered until a viewer tells us its own colors.
const DEFAULT_FG: Rgb = [0xff, 0xff, 0xff];
const DEFAULT_BG: Rgb = [0x28, 0x2c, 0x34];

pub type Rgb = [u8; 3];

/// One OSC 7501 report, exactly as the program sent it (strings decoded).
#[derive(Clone, Debug, PartialEq)]
pub struct StatusReport {
    /// idle, working, done, blocked, error, or clear (remove records).
    pub state: &'static str,
    /// permission, question or auth, only when blocked and said.
    pub kind: Option<&'static str>,
    /// 0..=100, only for working and blocked when sent.
    pub progress: Option<u8>,
    /// Empty for the root record.
    pub id: String,
    pub app: String,
    pub title: String,
    pub message: String,
}

/// What the terminal answers on the program's behalf, set by the core.
struct Hooks {
    responses: Vec<u8>,
    statuses: Vec<StatusReport>,
    title_changed: bool,
    xtversion: String,
    cell: (u32, u32),
    dark: bool,
}

/// One parsed terminal. Owns its libghostty handle.
pub struct Terminal {
    raw: ffi::Terminal,
    hooks: Box<Hooks>,
}

// The handle is only touched through `&mut self` / `&self`; libghostty keeps no thread state.
unsafe impl Send for Terminal {}

extern "C" fn on_write_pty(_: ffi::Terminal, ud: *mut c_void, data: *const u8, len: usize) {
    let hooks = unsafe { &mut *ud.cast::<Hooks>() };
    hooks.responses.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
}

fn string(s: &ffi::Str) -> String {
    if s.ptr.is_null() {
        return String::new();
    }
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(s.ptr, s.len) }).into_owned()
}

extern "C" fn on_program_status(_: ffi::Terminal, ud: *mut c_void, r: *const ffi::ProgramStatus) {
    let hooks = unsafe { &mut *ud.cast::<Hooks>() };
    let r = unsafe { &*r };
    let Some(state) = ["idle", "working", "done", "blocked", "error", "clear"].get(r.state as usize) else { return };
    hooks.statuses.push(StatusReport {
        state,
        kind: [None, Some("permission"), Some("question"), Some("auth")].get(r.kind as usize).copied().flatten(),
        progress: u8::try_from(r.progress).ok(),
        id: string(&r.id),
        app: string(&r.app),
        title: string(&r.title),
        message: string(&r.message),
    });
}

extern "C" fn on_title_changed(_: ffi::Terminal, ud: *mut c_void) {
    unsafe { (*ud.cast::<Hooks>()).title_changed = true };
}

extern "C" fn on_xtversion(_: ffi::Terminal, ud: *mut c_void) -> ffi::Str {
    let hooks = unsafe { &*ud.cast::<Hooks>() };
    ffi::Str { ptr: hooks.xtversion.as_ptr(), len: hooks.xtversion.len() }
}

extern "C" fn on_size(t: ffi::Terminal, ud: *mut c_void, out: *mut ffi::SizeReport) -> bool {
    let hooks = unsafe { &*ud.cast::<Hooks>() };
    let (mut cols, mut rows) = (0u16, 0u16);
    unsafe {
        ffi::ghostty_terminal_get(t, ffi::DATA_COLS, (&raw mut cols).cast());
        ffi::ghostty_terminal_get(t, ffi::DATA_ROWS, (&raw mut rows).cast());
        *out = ffi::SizeReport { rows, columns: cols, cell_width: hooks.cell.0, cell_height: hooks.cell.1 };
    }
    true
}

extern "C" fn on_color_scheme(_: ffi::Terminal, ud: *mut c_void, out: *mut i32) -> bool {
    let hooks = unsafe { &*ud.cast::<Hooks>() };
    unsafe { *out = hooks.dark as i32 };
    true
}

/// Answers like Ghostty: `CSI ? 62 ; 22 c` and `CSI > 1 ; 10 ; 0 c`.
extern "C" fn on_device_attributes(_: ffi::Terminal, _: *mut c_void, out: *mut ffi::DeviceAttributes) -> bool {
    let mut features = [0u16; 64];
    features[0] = 22;
    unsafe {
        *out = ffi::DeviceAttributes {
            conformance_level: 62,
            features,
            num_features: 1,
            device_type: 1,
            firmware_version: 10,
            rom_cartridge: 0,
            unit_id: 0,
        }
    };
    true
}

impl Terminal {
    pub fn new(cols: u16, rows: u16, scrollback_bytes: usize) -> Terminal {
        let mut raw = ptr::null_mut();
        let rc = unsafe { ffi::ghostty_terminal_new(ptr::null(), &mut raw, cols.max(1), rows.max(1)) };
        assert_eq!(rc, ffi::SUCCESS, "ghostty_terminal_new failed");
        Terminal::wrap(raw, scrollback_bytes)
    }

    fn wrap(raw: ffi::Terminal, scrollback_bytes: usize) -> Terminal {
        let hooks = Box::new(Hooks {
            responses: Vec::new(),
            statuses: Vec::new(),
            title_changed: false,
            xtversion: String::new(),
            cell: DEFAULT_CELL,
            dark: true,
        });
        let term = Terminal { raw, hooks };
        let ud: *const Hooks = &*term.hooks;
        let continuation = CONTINUATION_MAX_BYTES;
        unsafe {
            let set = |opt, value: *const c_void| ffi::ghostty_terminal_set(raw, opt, value);
            set(ffi::OPT_SCROLLBACK_MAX_BYTES, (&raw const scrollback_bytes).cast());
            set(ffi::OPT_CONTINUATION_MAX_BYTES, (&raw const continuation).cast());
            set(ffi::OPT_USERDATA, ud.cast());
            set(ffi::OPT_WRITE_PTY, on_write_pty as *const c_void);
            set(ffi::OPT_XTVERSION, on_xtversion as *const c_void);
            set(ffi::OPT_SIZE, on_size as *const c_void);
            set(ffi::OPT_COLOR_SCHEME, on_color_scheme as *const c_void);
            set(ffi::OPT_DEVICE_ATTRIBUTES, on_device_attributes as *const c_void);
            set(ffi::OPT_PROGRAM_STATUS, on_program_status as *const c_void);
            set(ffi::OPT_TITLE_CHANGED, on_title_changed as *const c_void);
        }
        let mut term = term;
        term.set_display(Some(DEFAULT_FG), Some(DEFAULT_BG), Some(DEFAULT_FG), None);
        term
    }

    pub fn write(&mut self, bytes: &[u8]) {
        unsafe { ffi::ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) }
    }

    /// Replies to queries the program asked since the last call, for its PTY.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.hooks.responses)
    }

    /// Drops replies made after the first `keep` bytes (ones someone else gives).
    pub fn drop_responses_after(&mut self, keep: usize) {
        self.hooks.responses.truncate(keep);
    }

    /// OSC 7501 reports received since the last call, in order.
    pub fn take_status_reports(&mut self) -> Vec<StatusReport> {
        std::mem::take(&mut self.hooks.statuses)
    }

    /// Whether the title changed since the last call.
    pub fn take_title_changed(&mut self) -> bool {
        std::mem::take(&mut self.hooks.title_changed)
    }

    /// The bytes a key press sends, given the modes the program set (cursor
    /// keys, kitty keyboard, modifyOtherKeys). `key` is a `GhosttyKey` value;
    /// `mods` uses GHOSTTY_MODS bits (shift 1, ctrl 2, alt 4, super 8).
    pub fn encode_key(&self, key: i32, mods: u16, text: &str, unshifted: u32) -> Vec<u8> {
        let (mut enc, mut ev) = (ptr::null_mut(), ptr::null_mut());
        unsafe {
            if ffi::ghostty_key_encoder_new(ptr::null(), &mut enc) != ffi::SUCCESS {
                return Vec::new();
            }
            if ffi::ghostty_key_event_new(ptr::null(), &mut ev) != ffi::SUCCESS {
                ffi::ghostty_key_encoder_free(enc);
                return Vec::new();
            }
            ffi::ghostty_key_encoder_setopt_from_terminal(enc, self.raw);
            // These are named keys, not a Mac keyboard: alt always means alt.
            let option_as_alt: i32 = 1;
            ffi::ghostty_key_encoder_setopt(enc, 6, (&raw const option_as_alt).cast());
            ffi::ghostty_key_event_set_action(ev, 1);
            ffi::ghostty_key_event_set_key(ev, key);
            ffi::ghostty_key_event_set_mods(ev, mods);
            ffi::ghostty_key_event_set_utf8(ev, text.as_ptr(), text.len());
            ffi::ghostty_key_event_set_unshifted_codepoint(ev, unshifted);
            let mut buf = [0u8; 128];
            let mut len = 0usize;
            let rc = ffi::ghostty_key_encoder_encode(enc, ev, buf.as_mut_ptr(), buf.len(), &mut len);
            ffi::ghostty_key_event_free(ev);
            ffi::ghostty_key_encoder_free(enc);
            if rc == ffi::SUCCESS { buf[..len].to_vec() } else { Vec::new() }
        }
    }

    /// One mouse-wheel notch at the middle of the screen, the way the program
    /// asked for mouse reports; empty if it didn't ask (it doesn't take the wheel).
    pub fn wheel(&self, up: bool) -> Vec<u8> {
        let (cols, rows) = self.size();
        let (cw, ch) = self.hooks.cell;
        unsafe {
            let (mut enc, mut ev) = (ptr::null_mut(), ptr::null_mut());
            if ffi::ghostty_mouse_encoder_new(ptr::null(), &mut enc) != ffi::SUCCESS {
                return Vec::new();
            }
            if ffi::ghostty_mouse_event_new(ptr::null(), &mut ev) != ffi::SUCCESS {
                ffi::ghostty_mouse_encoder_free(enc);
                return Vec::new();
            }
            ffi::ghostty_mouse_encoder_setopt_from_terminal(enc, self.raw);
            let size = ffi::MouseSize {
                size: size_of::<ffi::MouseSize>(),
                screen_width: cols as u32 * cw,
                screen_height: rows as u32 * ch,
                cell_width: cw,
                cell_height: ch,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            };
            ffi::ghostty_mouse_encoder_setopt(enc, 2, (&raw const size).cast());
            ffi::ghostty_mouse_event_set_action(ev, 0);
            ffi::ghostty_mouse_event_set_button(ev, if up { 4 } else { 5 });
            let at =
                ffi::MousePosition { x: (cols / 2) as f32 * cw as f32 + 1.0, y: (rows / 2) as f32 * ch as f32 + 1.0 };
            ffi::ghostty_mouse_event_set_position(ev, at);
            let mut buf = [0u8; 64];
            let mut len = 0usize;
            let rc = ffi::ghostty_mouse_encoder_encode(enc, ev, buf.as_mut_ptr(), buf.len(), &mut len);
            ffi::ghostty_mouse_event_free(ev);
            ffi::ghostty_mouse_encoder_free(enc);
            if rc == ffi::SUCCESS { buf[..len].to_vec() } else { Vec::new() }
        }
    }

    /// The visible screen as rows, each padded to the screen width.
    pub fn screen_rows(&self) -> Vec<String> {
        let (cols, rows) = self.size();
        let last = (cols.saturating_sub(1), rows.saturating_sub(1) as u32);
        let Some(sel) = self.select(ffi::POINT_ACTIVE, (0, 0), last) else { return Vec::new() };
        let text = String::from_utf8_lossy(&self.format_with(ffi::FORMAT_PLAIN, Some(&sel), false, false)).into_owned();
        let mut out: Vec<String> =
            text.split('\n').take(rows as usize).map(|r| format!("{r:<w$}", w = cols as usize)).collect();
        out.resize(rows as usize, " ".repeat(cols as usize));
        out
    }

    /// The visible screen with its colors and styles as SGR sequences, one
    /// row per line. Each row starts from default style.
    pub fn screen_vt(&self) -> String {
        let (cols, rows) = self.size();
        let last = (cols.saturating_sub(1), rows.saturating_sub(1) as u32);
        let Some(sel) = self.select(ffi::POINT_ACTIVE, (0, 0), last) else { return String::new() };
        let text = String::from_utf8_lossy(&self.format_with(ffi::FORMAT_VT, Some(&sel), false, false)).into_owned();
        text.replace("\r\n", "\n")
            .split('\n')
            .take(rows as usize)
            .map(|r| format!("\x1b[0m{}\x1b[0m", r.trim_end_matches('\r')))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Bytes of replies waiting in `take_responses`.
    pub fn pending_responses(&self) -> usize {
        self.hooks.responses.len()
    }

    /// The program name and version XTVERSION reports.
    pub fn set_xtversion(&mut self, version: &str) {
        self.hooks.xtversion = version.to_string();
    }

    /// Display facts from a viewer: default colors and cell size in pixels.
    /// `None` keeps what was known before.
    pub fn set_display(&mut self, fg: Option<Rgb>, bg: Option<Rgb>, cursor: Option<Rgb>, cell: Option<(u32, u32)>) {
        let color = |opt, rgb: Option<Rgb>| {
            if let Some(c) = rgb {
                unsafe { ffi::ghostty_terminal_set(self.raw, opt, c.as_ptr().cast()) };
            }
        };
        color(ffi::OPT_COLOR_FOREGROUND, fg);
        color(ffi::OPT_COLOR_BACKGROUND, bg);
        color(ffi::OPT_COLOR_CURSOR, cursor);
        if let Some([r, g, b]) = bg {
            self.hooks.dark = (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000 < 128;
        }
        if let Some(cell) = cell {
            self.hooks.cell = cell;
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cw, ch) = self.hooks.cell;
        unsafe { ffi::ghostty_terminal_resize(self.raw, cols.max(1), rows.max(1), cw, ch) };
    }

    pub fn size(&self) -> (u16, u16) {
        let (mut cols, mut rows) = (0u16, 0u16);
        unsafe {
            ffi::ghostty_terminal_get(self.raw, ffi::DATA_COLS, (&raw mut cols).cast());
            ffi::ghostty_terminal_get(self.raw, ffi::DATA_ROWS, (&raw mut rows).cast());
        }
        (cols, rows)
    }

    pub fn alt_screen(&self) -> bool {
        let mut screen: i32 = 0;
        unsafe { ffi::ghostty_terminal_get(self.raw, ffi::DATA_ACTIVE_SCREEN, (&raw mut screen).cast()) };
        screen == 1
    }

    /// A DEC private mode (e.g. 1004 focus reporting).
    pub fn dec_mode(&self, mode: u16) -> bool {
        let mut cfg = ffi::ModeConfig { mode: mode & 0x7FFF, value: false };
        unsafe { ffi::ghostty_terminal_get(self.raw, ffi::DATA_MODE, (&raw mut cfg).cast()) };
        cfg.value
    }

    pub fn title(&self) -> String {
        let mut s = ffi::Str { ptr: ptr::null(), len: 0 };
        let rc = unsafe { ffi::ghostty_terminal_get(self.raw, ffi::DATA_TITLE, (&raw mut s).cast()) };
        if rc != ffi::SUCCESS || s.ptr.is_null() {
            return String::new();
        }
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(s.ptr, s.len) }).into_owned()
    }

    /// The active screen and its scrollback as plain text, soft wraps joined.
    pub fn plain_text(&self) -> String {
        String::from_utf8_lossy(&self.format(ffi::FORMAT_PLAIN, None, false)).into_owned()
    }

    fn grid_ref(&self, tag: i32, x: u16, y: u32) -> Option<ffi::GridRef> {
        let point = ffi::Point { tag, value: ffi::PointValue { coord: ffi::Coord { x, y } } };
        let mut out = ffi::GridRef { size: size_of::<ffi::GridRef>(), node: ptr::null_mut(), x: 0, y: 0 };
        (unsafe { ffi::ghostty_terminal_grid_ref(self.raw, point, &mut out) } == ffi::SUCCESS).then_some(out)
    }

    fn history_rows(&self) -> u32 {
        let mut bar = ffi::Scrollbar::default();
        unsafe { ffi::ghostty_terminal_get(self.raw, ffi::DATA_SCROLLBAR, (&raw mut bar).cast()) };
        bar.total.saturating_sub(self.size().1 as u64) as u32
    }

    fn format(&self, emit: i32, selection: Option<&ffi::Selection>, extras: bool) -> Vec<u8> {
        self.format_with(emit, selection, extras, emit == ffi::FORMAT_PLAIN)
    }

    /// `joined`: soft-wrapped lines joined and trailing space trimmed.
    fn format_with(&self, emit: i32, selection: Option<&ffi::Selection>, extras: bool, joined: bool) -> Vec<u8> {
        let screen = ffi::ScreenExtra {
            size: size_of::<ffi::ScreenExtra>(),
            cursor: extras,
            style: extras,
            hyperlink: extras,
            protection: extras,
            kitty_keyboard: extras,
            charsets: extras,
        };
        let opts = ffi::FormatterOptions {
            size: size_of::<ffi::FormatterOptions>(),
            emit,
            unwrap: joined,
            trim: joined,
            extra: ffi::TerminalExtra {
                size: size_of::<ffi::TerminalExtra>(),
                // The full palette would paint over the viewer's own theme.
                palette: false,
                modes: extras,
                scrolling_region: extras,
                tabstops: extras,
                pwd: extras,
                keyboard: extras,
                screen,
            },
            selection: selection.map_or(ptr::null(), |s| s as *const _),
        };
        let mut f = ptr::null_mut();
        if unsafe { ffi::ghostty_formatter_terminal_new(ptr::null(), &mut f, self.raw, opts) } != ffi::SUCCESS {
            return Vec::new();
        }
        let bytes = alloc_call(|out, len| unsafe { ffi::ghostty_formatter_format_alloc(f, ptr::null(), out, len) });
        unsafe { ffi::ghostty_formatter_free(f) };
        bytes.unwrap_or_default()
    }

    fn select(&self, tag: i32, (x0, y0): (u16, u32), (x1, y1): (u16, u32)) -> Option<ffi::Selection> {
        Some(ffi::Selection {
            size: size_of::<ffi::Selection>(),
            start: self.grid_ref(tag, x0, y0)?,
            end: self.grid_ref(tag, x1, y1)?,
            rectangle: false,
        })
    }

    /// Bytes that repaint this terminal on a fresh viewer: up to `history`
    /// scrollback lines (main screen only), then the screen with its modes,
    /// cursor and title.
    pub fn paint(&self, history: usize) -> Vec<u8> {
        let (cols, rows) = self.size();
        let last = (cols.saturating_sub(1), rows.saturating_sub(1) as u32);
        // A soft reset leaves mouse, focus, paste, sync and key modes alone: turn
        // them off too, so a viewer that missed their "off" (a dropped backlog)
        // ends up with exactly the screen's own modes, replayed below.
        let mut out = b"\x1b[!p\x1b[?2026l\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\
\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\x1b[?2004l\x1b[?1l\x1b[?66l\x1b[?2031l\x1b[?2048l\x1b[=0;1u\
\x1b[?1049l\x1b[?1047l\x1b[?47l\x1b[H\x1b[2J"
            .to_vec();
        let available = self.history_rows();
        if history > 0 && available > 0 && !self.alt_screen() {
            let from = available.saturating_sub(history as u32);
            if let Some(sel) = self.select(ffi::POINT_HISTORY, (0, from), (last.0, available - 1)) {
                out.extend(self.format(ffi::FORMAT_VT, Some(&sel), false));
                // Push those lines up into the viewer's scrollback before painting the screen.
                out.extend(b"\x1b[0m\r\n".iter().chain(&b"\n".repeat(rows as usize)));
                out.extend(b"\x1b[H\x1b[2J");
            }
        }
        if self.alt_screen() {
            out.extend(b"\x1b[?1049h");
        }
        if let Some(sel) = self.select(ffi::POINT_ACTIVE, (0, 0), last) {
            let screen = self.format(ffi::FORMAT_VT, Some(&sel), true);
            // Alternate-screen aliases can disagree with the active screen. Replay
            // only the screen chosen above, not the formatter's independent mode bits.
            // Synchronized output belongs to the old viewer and would freeze a new one.
            out.extend(strip(&screen, &[b"\x1b[?2026h", b"\x1b[?47h", b"\x1b[?1047h", b"\x1b[?1049h"]));
        }
        let title = self.title();
        if !title.is_empty() {
            out.extend(format!("\x1b]2;{title}\x1b\\").as_bytes());
        }
        out
    }

    /// Complete binary snapshot, including any half-received escape sequence.
    pub fn snapshot(&self) -> Option<Vec<u8>> {
        alloc_call(|out, len| unsafe { ffi::ghostty_snapshot_encode_alloc(self.raw, ptr::null(), out, len) })
    }

    /// Restores a snapshot; `None` if it doesn't decode (e.g. another Ghostty version).
    pub fn restore(bytes: &[u8], scrollback_bytes: usize) -> Option<Terminal> {
        let mut d = ptr::null_mut();
        let rc = unsafe { ffi::ghostty_snapshot_decoder_new_buf(ptr::null(), &mut d, bytes.as_ptr(), bytes.len()) };
        if rc != ffi::SUCCESS {
            return None;
        }
        let mut raw = ptr::null_mut();
        let rc = unsafe { ffi::ghostty_snapshot_decoder_decode(d, &mut raw) };
        unsafe { ffi::ghostty_snapshot_decoder_free(d) };
        if rc != ffi::SUCCESS || raw.is_null() {
            return None;
        }
        Some(Terminal::wrap(raw, scrollback_bytes))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_terminal_free(self.raw) }
    }
}

fn strip(haystack: &[u8], needles: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if let Some(needle) = needles.iter().find(|needle| haystack[i..].starts_with(needle)) {
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

fn alloc_call(f: impl FnOnce(*mut *mut u8, *mut usize) -> i32) -> Option<Vec<u8>> {
    let (mut out, mut len): (*mut u8, usize) = (ptr::null_mut(), 0);
    if f(&mut out, &mut len) != ffi::SUCCESS {
        return None;
    }
    if out.is_null() {
        return Some(Vec::new());
    }
    let bytes = unsafe { std::slice::from_raw_parts(out, len) }.to_vec();
    unsafe { ffi::ghostty_free(ptr::null::<c_void>(), out, len) };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            (reports[0].state, reports[0].progress, reports[0].app.as_str()),
            ("working", Some(40), "claude-code")
        );
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
}
