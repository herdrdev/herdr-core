//! Program output → the core's screen and the bytes viewers get.
//! Every byte reaches the screen. Viewers get every byte too, except queries
//! the screen answered itself: a viewer seeing them would answer a second time.
//! Images (APC: kitty graphics) are the other way round: the screen takes
//! them (they move the cursor) but its replies are dropped, since it can't
//! show them; a viewer that can answers.

use ghostty_vt::Terminal as Screen;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// Queries are short; anything longer is passed through unexamined.
const MAX_CSI: usize = 64;
const MAX_STRING: usize = 512;

#[derive(Default)]
pub struct Relay {
    /// An escape sequence still arriving, kept back until it can be judged.
    held: Vec<u8>,
    /// Inside an APC: how many reply bytes were waiting when it began.
    apc: Option<usize>,
    /// The program's latest win32-input-mode request (DEC 9001, ConPTY's ask
    /// for key events), until taken. The screen doesn't keep it.
    pub win32_input: Option<bool>,
}

enum Seq {
    Pending,
    Done { maybe_query: bool },
}

impl Relay {
    /// In-band resize belongs to the viewer, which knows the pixel geometry.
    /// Ghostty also emits it when 2048 is enabled and when its screen resizes.
    pub fn take_responses(screen: &mut Screen) -> Vec<u8> {
        let replies = screen.take_responses();
        let mut out = Vec::with_capacity(replies.len());
        let mut i = 0;
        while i < replies.len() {
            if let Some(rest) = replies[i..].strip_prefix(b"\x1b[48;") {
                let n = rest.iter().take_while(|b| b.is_ascii_digit() || **b == b';').count();
                if rest.get(n) == Some(&b't') {
                    i += 5 + n + 1;
                    continue;
                }
            }
            out.push(replies[i]);
            i += 1;
        }
        out
    }

    /// Feeds program output; returns what viewers should get. Replies the
    /// screen produced are left in it for the caller to send to the program.
    pub fn feed(&mut self, screen: &mut Screen, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut run = 0;
        for (i, &b) in input.iter().enumerate() {
            if self.held.is_empty() {
                if b == ESC {
                    pass(screen, &mut out, &input[run..i]);
                    self.held.push(b);
                }
                continue;
            }
            self.held.push(b);
            if let Seq::Done { maybe_query } = self.judge() {
                let seq = std::mem::take(&mut self.held);
                if maybe_query {
                    let before = screen.pending_responses();
                    screen.write(&seq);
                    if screen.pending_responses() == before {
                        out.extend_from_slice(&seq); // not answered here: the viewer may
                    }
                } else {
                    if let Some(on) = win32_input(&seq) {
                        self.win32_input = Some(on);
                    }
                    if seq == b"\x1b_" {
                        self.apc = Some(screen.pending_responses());
                    }
                    pass(screen, &mut out, &seq);
                    // An APC ends with ST; whatever the screen answered to it goes.
                    if seq == b"\x1b\\"
                        && let Some(keep) = self.apc.take()
                    {
                        screen.drop_responses_after(keep);
                    }
                }
                run = i + 1;
            }
        }
        if self.held.is_empty() {
            pass(screen, &mut out, &input[run..]);
        }
        out
    }

    /// A viewer missed a sequence too large to replay. Consume just its tail,
    /// capturing a repaint at ground even if this read also starts another image.
    pub fn feed_to_ground(&mut self, screen: &mut Screen, input: &[u8]) -> (Vec<u8>, Option<(usize, Vec<u8>)>) {
        let mut bytes = std::mem::take(&mut self.held);
        bytes.extend_from_slice(input);
        let Some(n) = screen.write_until_ground(&bytes) else {
            return (bytes, None);
        };
        if let Some(keep) = self.apc.take() {
            screen.drop_responses_after(keep);
        }
        let paint = screen.paint(0);
        let mut out = bytes[..n].to_vec();
        out.extend(self.feed(screen, &bytes[n..]));
        (out, Some((n, paint)))
    }

    /// Writes a half-received sequence to the screen (before a snapshot).
    pub fn flush(&mut self, screen: &mut Screen) {
        screen.write(&std::mem::take(&mut self.held));
    }

    fn judge(&self) -> Seq {
        let h = &self.held;
        let last = *h.last().expect("held starts with ESC");
        match h.get(1) {
            None => Seq::Pending,
            Some(b'[') if h.len() > 2 && (0x40..=0x7e).contains(&last) => {
                Seq::Done { maybe_query: b"cnpqtu".contains(&last) }
            }
            Some(b'[') if h.len() > MAX_CSI => Seq::Done { maybe_query: false },
            Some(b'[') => Seq::Pending,
            Some(b']') | Some(b'P') => {
                let ended = last == BEL && h[1] == b']' || h.ends_with(b"\x1b\\") && h.len() > 3;
                if ended {
                    let body = &h[2..];
                    let query = match h[1] {
                        b']' => body.contains(&b'?'),
                        _ => body.starts_with(b"$q") || body.starts_with(b"+q"),
                    };
                    Seq::Done { maybe_query: query }
                } else if h.len() > MAX_STRING {
                    Seq::Done { maybe_query: false }
                } else {
                    Seq::Pending
                }
            }
            Some(_) => Seq::Done { maybe_query: false },
        }
    }
}

/// `CSI ? … 9001 … h` or `l`: whether it turns win32-input-mode on.
fn win32_input(seq: &[u8]) -> Option<bool> {
    let (last, params) = seq.strip_prefix(b"\x1b[?")?.split_last()?;
    let on = match last {
        b'h' => true,
        b'l' => false,
        _ => return None,
    };
    params.split(|b| *b == b';').any(|p| p == b"9001").then_some(on)
}

fn pass(screen: &mut Screen, out: &mut Vec<u8>, bytes: &[u8]) {
    if !bytes.is_empty() {
        screen.write(bytes);
        out.extend_from_slice(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_bytewise(relay: &mut Relay, screen: &mut Screen, input: &[u8]) -> Vec<u8> {
        input.iter().flat_map(|b| relay.feed(screen, std::slice::from_ref(b))).collect()
    }

    #[test]
    fn strips_only_the_queries_the_screen_answered() {
        for split in [false, true] {
            let mut screen = Screen::new(80, 24, 1 << 20);
            let mut relay = Relay::default();
            let input = b"hi \x1b[31mred\x1b[0m \x1b[c\x1b]11;?\x07\x1b[6n\x1bP+q544e\x1b\\\x1b]2;title\x07 end";
            let out =
                if split { feed_bytewise(&mut relay, &mut screen, input) } else { relay.feed(&mut screen, input) };
            // Answered here: DA1, background color, cursor position. Not answered: XTGETTCAP.
            assert_eq!(
                String::from_utf8_lossy(&out),
                "hi \x1b[31mred\x1b[0m \x1bP+q544e\x1b\\\x1b]2;title\x07 end",
                "split={split}"
            );
            let replies = String::from_utf8_lossy(&screen.take_responses()).into_owned();
            assert!(replies.starts_with("\x1b[?62;22c\x1b]11;rgb:") && replies.ends_with("R"), "{replies:?}");
            assert_eq!(screen.title(), "title");
        }
    }

    #[test]
    fn resize_reports_are_dropped_but_query_answers_are_kept() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        let mut relay = Relay::default();
        let out = relay.feed(&mut screen, b"\x1b[c\x1b[?2048h\x1b[6n");
        assert_eq!(out, b"\x1b[?2048h");
        screen.resize(100, 30);
        let replies = Relay::take_responses(&mut screen);
        assert_eq!(replies, b"\x1b[?62;22c\x1b[1;1R");
    }

    #[test]
    fn images_are_answered_by_viewers_only() {
        // Kitty graphics: the screen can't show images; a viewer that can
        // answers. Two answers (the screen's EINVAL and the viewer's OK)
        // confused programs.
        for split in [false, true] {
            let mut screen = Screen::new(80, 24, 1 << 20);
            let mut relay = Relay::default();
            let input = b"a\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\b\x1b_Gi=7,a=T,f=100;iVBORw0KGgo=\x1b\\c\x1b[6n";
            let out =
                if split { feed_bytewise(&mut relay, &mut screen, input) } else { relay.feed(&mut screen, input) };
            assert_eq!(
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&input[..input.len() - 4]),
                "split={split}"
            );
            let replies = String::from_utf8_lossy(&screen.take_responses()).into_owned();
            assert!(replies.ends_with('R') && !replies.contains("_G"), "only the cursor report: {replies:?}");
        }
    }

    #[test]
    fn recovery_finds_ground_between_images_even_when_every_read_ends_mid_image() {
        for opener in [b"\x1b_Gm=1;".as_slice(), b"\x1b]2;", b"\x1bPtmux;", b"\x1b^"] {
            for split_st in [false, true] {
                let mut screen = Screen::new(80, 24, 1 << 20);
                let mut relay = Relay::default();
                let mut prefix = b"before\r\n".to_vec();
                prefix.extend_from_slice(opener);
                prefix.extend(vec![b'A'; 70_000]);
                if split_st {
                    prefix.push(ESC);
                }
                let passed = relay.feed(&mut screen, &prefix);
                assert!(screen.continuation().is_none());
                let tail = if split_st { b"\\\x1b_Gm=1;AAAA".as_slice() } else { b"AA\x1b\\\x1b_Gm=1;AAAA" };
                let (out, recovered) = relay.feed_to_ground(&mut screen, tail);
                let (at, paint) = recovered.expect("ground inside the read, not at its end");
                assert_eq!([passed, out.clone()].concat(), [prefix, tail.to_vec()].concat());
                let mut viewer = Screen::new(80, 24, 1 << 20);
                viewer.write(b"\x1b_Gm=1;partial");
                viewer.write(b"\x18");
                viewer.write(&paint);
                viewer.write(&out[at..]);
                viewer.write(&relay.feed(&mut screen, b"AAAA\x1b\\\r\nafter"));
                let text = viewer.plain_text();
                assert!(text.contains("before") && text.contains("after"), "{text:?}");
                assert!(!text.contains("AAAA"), "image data leaked: {text:?}");
                assert!(screen.take_responses().is_empty(), "no core kitty acknowledgements");
            }
        }
    }

    #[test]
    fn long_strings_pass_through_untouched() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        let mut relay = Relay::default();
        let long = format!("\x1b]52;c;{}?\x07after", "A".repeat(2000));
        let out = feed_bytewise(&mut relay, &mut screen, long.as_bytes());
        assert_eq!(out, long.as_bytes());
    }
}
