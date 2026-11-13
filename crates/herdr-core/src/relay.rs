//! Program output → the core's screen and the bytes viewers get.
//! Every byte reaches the screen. Viewers get every byte too, except queries
//! the screen answered itself: a viewer seeing them would answer a second time.
//! Images (APC: kitty graphics) are the other way round: the screen takes
//! them (they move the cursor) but its replies are dropped, since it can't
//! show them; a viewer that can answers. Programs read replies in order (DA1
//! after kitty's query is how they learn images don't work), so the screen's
//! DA1 after an image waits for the viewer's answer, briefly.
//! A title the program restores from its title stack (XTWINOPS 23) follows
//! the restore as an OSC 2, so the screen and every viewer take it, whether
//! or not the viewer keeps a title stack of its own.

use ghostty_vt::Terminal as Screen;
use std::time::Duration;

mod graphics;
pub(crate) use graphics::Answers;

/// How long DA1 waits behind an image for a viewer's answer.
pub fn image_answer_wait() -> Duration {
    crate::input::env_ms("HC_IMAGE_ANSWER_MS", 100)
}

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
    /// An image the screen answered went to viewers, and none has answered yet.
    waiting: bool,
    /// Where, in the screen's pending replies, those after that image begin.
    behind_from: Option<usize>,
    /// The screen's replies waiting for a viewer's answer.
    behind: Vec<u8>,
    /// Counts waits, so a deadline only ends the wait it was set for.
    wait: u64,
    wait_started: bool,
    graphics: graphics::Parser,
    query: u64,
    queries: Vec<(graphics::Key, u64)>,
    unanswered: Vec<u64>,
}

enum Seq {
    Pending,
    Done { maybe_query: bool },
}

impl Relay {
    /// In-band resize belongs to the viewer, which knows the pixel geometry.
    /// Ghostty also emits it when 2048 is enabled and when its screen resizes.
    pub fn take_responses(screen: &mut Screen) -> Vec<u8> {
        without_resize_reports(&screen.take_responses())
    }

    /// Only DA1 after an image waits until a viewer answers or the wait ends.
    /// With no viewer to answer (`answering`
    /// false), everything goes. Returns the wait that began, which the caller
    /// ends with `expire` after `image_answer_wait()`.
    pub fn take_replies(&mut self, screen: &mut Screen, answering: bool) -> (Vec<u8>, Option<u64>) {
        let replies = screen.take_responses();
        if !answering {
            self.queries.clear();
            self.wait_started = false;
            let mut now = self.end_wait();
            now.extend(without_resize_reports(&replies));
            return (now, None);
        }
        let split = match self.behind_from.take() {
            Some(at) => at.min(replies.len()),
            None if self.waiting => 0,
            None => replies.len(),
        };
        let mut now = without_resize_reports(&replies[..split]);
        let after = without_resize_reports(&replies[split..]);
        let mut i = 0;
        while i < after.len() {
            if let Some(n) = da1_len(&after[i..]) {
                self.behind.extend_from_slice(&after[i..i + n]);
                i += n;
                continue;
            }
            now.push(after[i]);
            i += 1;
        }
        let started = std::mem::take(&mut self.wait_started).then_some(self.wait);
        (now, started)
    }

    pub(crate) fn expect_answers<'a>(&mut self, viewers: impl Iterator<Item = &'a mut Answers>) {
        let queries = std::mem::take(&mut self.queries);
        for viewer in viewers {
            for (key, query) in &queries {
                viewer.expect(key.clone(), *query);
            }
        }
    }

    /// Only complete, correlated answers can end this wait.
    pub(crate) fn answered(&mut self, queries: &[(usize, u64)]) -> Vec<u8> {
        self.unanswered.retain(|query| !queries.iter().any(|(_, answer)| query == answer));
        if self.waiting && self.unanswered.is_empty() { self.end_wait() } else { Vec::new() }
    }

    /// Ends `wait` if it is still going: no viewer answered in time.
    pub fn expire(&mut self, wait: u64) -> Vec<u8> {
        if self.wait == wait && self.waiting { self.end_wait() } else { Vec::new() }
    }

    pub(crate) fn end_wait(&mut self) -> Vec<u8> {
        self.unanswered.clear();
        self.waiting = false;
        self.behind_from = None;
        std::mem::take(&mut self.behind)
    }

    /// Feeds program output; returns what viewers should get. Replies the
    /// screen produced are left in it for the caller to send to the program.
    pub fn feed(&mut self, screen: &mut Screen, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut run = 0;
        for (i, &b) in input.iter().enumerate() {
            let image = self.graphics.push(b);
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
                    match title_stack(&seq) {
                        Some(TitleOp::Push { which, index }) => screen.push_title(which, index),
                        Some(TitleOp::Pop { which, index }) => {
                            if let Some(title) = screen.pop_title(which, index) {
                                pass(screen, &mut out, format!("\x1b]2;{title}\x1b\\").as_bytes());
                            }
                        }
                        None => {}
                    }
                } else {
                    if let Some(on) = win32_input(&seq) {
                        self.win32_input = Some(on);
                    }
                    if seq == b"\x1b_" {
                        self.apc = Some(screen.pending_responses());
                    }
                    pass(screen, &mut out, &seq);
                    // An APC ends with ST; whatever the screen answered to it goes,
                    // and an answer means a viewer that shows images answers too.
                    if seq == b"\x1b\\"
                        && let Some(keep) = self.apc.take()
                    {
                        if screen.pending_responses() > keep
                            && let Some(image) = image.filter(|s| graphics::expects_answer(s))
                            && let Some(key) = graphics::key(&image)
                        {
                            self.query += 1;
                            self.queries.push((key, self.query));
                            self.unanswered.push(self.query);
                            if !self.waiting {
                                self.waiting = true;
                                self.wait += 1;
                                self.wait_started = true;
                                self.behind_from = Some(keep);
                            }
                        }
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

pub(crate) fn da1_len(bytes: &[u8]) -> Option<usize> {
    let rest = bytes.strip_prefix(b"\x1b[?")?;
    let n = rest.iter().take_while(|b| b.is_ascii_digit() || **b == b';').count();
    (rest.get(n) == Some(&b'c')).then_some(3 + n + 1)
}

fn without_resize_reports(replies: &[u8]) -> Vec<u8> {
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

#[derive(Debug, PartialEq)]
enum TitleOp {
    Push { which: u16, index: u16 },
    Pop { which: u16, index: u16 },
}

/// `CSI 22 ; which ; index t` saves a title, `CSI 23 ; …` restores one.
/// A missing or empty parameter is 0, as in xterm.
fn title_stack(seq: &[u8]) -> Option<TitleOp> {
    let params = seq.strip_prefix(b"\x1b[")?.strip_suffix(b"t")?;
    if !params.iter().all(|b| b.is_ascii_digit() || *b == b';') {
        return None;
    }
    let mut values = params.split(|b| *b == b';').map(|p| match p {
        b"" => Some(0),
        digits => std::str::from_utf8(digits).ok()?.parse::<u16>().ok(),
    });
    let op = values.next()??;
    let which = values.next().unwrap_or(Some(0))?;
    let index = values.next().unwrap_or(Some(0))?;
    if values.next().is_some() {
        return None;
    }
    match op {
        22 => Some(TitleOp::Push { which, index }),
        23 => Some(TitleOp::Pop { which, index }),
        _ => None,
    }
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
    fn a_restored_title_reaches_the_screen_and_viewers_as_an_osc_title() {
        // Neovim with `title` under xterm-256color: smcup saves too, then its own
        // save, its title, and on exit both restores. A shell may have set no title.
        let open = b"\x1b[?1049h\x1b[22;0;0t\x1b[22;0t\x1b]0;file.md - Nvim\x07text";
        let exit = b"\x1b[23;0t\x1b[?1049l\x1b[23;0;0t$ ";
        for (before, split) in [("shell", false), ("shell", true), ("", false)] {
            let mut screen = Screen::new(80, 24, 1 << 20);
            let mut relay = Relay::default();
            let mut feed = |screen: &mut Screen, bytes: &[u8]| {
                if split { feed_bytewise(&mut relay, screen, bytes) } else { relay.feed(screen, bytes) }
            };
            let set = format!("\x1b]0;{before}\x07");
            feed(&mut screen, set.as_bytes());
            assert_eq!(feed(&mut screen, open), open);
            screen.take_title_changed();
            let restore = format!("\x1b]2;{before}\x1b\\");
            let want = [&b"\x1b[23;0t"[..], restore.as_bytes(), b"\x1b[?1049l\x1b[23;0;0t", restore.as_bytes(), b"$ "];
            let out = feed(&mut screen, exit);
            assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(&want.concat()), "{before:?} {split}");
            assert!(screen.take_title_changed(), "{before:?} {split}");
            assert_eq!(screen.title(), before);
            assert!(screen.title_stack().is_empty() && screen.take_responses().is_empty());
        }
    }

    #[test]
    fn a_restore_with_nothing_saved_or_no_window_title_changes_nothing() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        let mut relay = Relay::default();
        let input = b"\x1b]2;kept\x07\x1b[23;0t\x1b[22;1t\x1b]2;other\x07\x1b[23;2t";
        assert_eq!(relay.feed(&mut screen, input), input, "only what the program sent");
        assert_eq!(screen.title(), "other");
        assert!(screen.title_stack().is_empty());
    }

    #[test]
    fn title_stack_sequences_follow_xterms_parameters() {
        use TitleOp::*;
        for (seq, op) in [
            (&b"\x1b[22;0t"[..], Some(Push { which: 0, index: 0 })),
            (b"\x1b[22t", Some(Push { which: 0, index: 0 })),
            (b"\x1b[22;t", Some(Push { which: 0, index: 0 })),
            (b"\x1b[22;2;3t", Some(Push { which: 2, index: 3 })),
            (b"\x1b[23;1t", Some(Pop { which: 1, index: 0 })),
            (b"\x1b[23;2t", Some(Pop { which: 2, index: 0 })),
            (b"\x1b[22;0;0;0t", None),
            (b"\x1b[21t", None),
            (b"\x1b[?22;0t", None),
            (b"\x1b[22:0t", None),
            (b"\x1b[22;99999t", None),
            (b"\x1b[22;0u", None),
        ] {
            assert_eq!(title_stack(seq), op, "{:?}", String::from_utf8_lossy(seq));
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
    fn replies_after_an_image_wait_for_a_viewer_to_answer() {
        // Kitty's query, then a feature query viewers may leave unanswered, then DA1.
        let probe = b"\x1b[6n\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b_Ga=q,i=32,e=T,U=1\x1b\\\x1b[c";
        let mut screen = Screen::new(80, 24, 1 << 20);
        let mut relay = Relay::default();
        relay.feed(&mut screen, probe);
        assert_eq!(relay.take_replies(&mut screen, false), (b"\x1b[1;1R\x1b[?62;22c".to_vec(), None), "no viewer");
        // A viewer: the reply before the image goes, DA1 waits for its answer.
        relay.feed(&mut screen, probe);
        let (now, wait) = relay.take_replies(&mut screen, true);
        assert_eq!(now, b"\x1b[1;1R");
        let mut answers = Answers::default();
        relay.expect_answers(std::iter::once(&mut answers));
        assert_eq!(relay.answered(&answers.feed(b"\x1b_Gi=31;OK\x1b\\")), b"\x1b[?62;22c");
        assert!(relay.expire(wait.expect("a wait began")).is_empty(), "nothing left once answered");
        // No answer in time: DA1 goes when the wait ends.
        relay.feed(&mut screen, probe);
        let (now, wait) = relay.take_replies(&mut screen, true);
        assert_eq!(now, b"\x1b[1;1R");
        assert_eq!(relay.expire(wait.expect("a wait began")), b"\x1b[?62;22c");
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
    fn only_da1_waits_and_answers_must_be_complete_and_correlated() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        let mut relay = Relay::default();
        let mut viewer = Answers::default();
        let mut second = Answers::default();
        let probe = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[6n\x1b[5n";
        relay.feed(&mut screen, probe);
        relay.expect_answers([&mut viewer, &mut second].into_iter());
        let (now, wait) = relay.take_replies(&mut screen, true);
        assert_eq!(now, b"\x1b[1;1R\x1b[0n", "prompt queries must not wait");
        for part in [
            b"\x1b[200~\x1b_Gi=31;OK\x1b\\\x1b[201~".as_slice(),
            b"\x1b[200~\x1b_Garbitrary pasted text\x1b[201~",
            b"\x1b_Garbitrary\x1b\\",
            b"\x1b_Gi=31,a=q;OK\x1b\\",
            b"\x1b",
            b"_",
            b"Gi=31;OK",
            b"\x1b",
        ] {
            assert!(relay.answered(&viewer.feed(part)).is_empty());
        }
        assert_eq!(relay.answered(&viewer.feed(b"\\")), b"\x1b[?62;22c");
        assert!(relay.expire(wait.unwrap()).is_empty());

        // The second viewer's duplicate belongs to the first query, even
        // when the program reuses its image ID for the next query.
        relay.feed(&mut screen, probe);
        relay.expect_answers([&mut viewer, &mut second].into_iter());
        let (_, wait) = relay.take_replies(&mut screen, true);
        assert!(relay.answered(&second.feed(b"\x1b_Gi=31;OK\x1b\\")).is_empty());
        assert_eq!(relay.answered(&viewer.feed(b"\x1b_Gi=31;OK\x1b\\")), b"\x1b[?62;22c");
        assert!(relay.expire(wait.unwrap()).is_empty());

        // An expired query's late answer cannot release a newer ID's DA1.
        relay.feed(&mut screen, probe);
        relay.expect_answers(std::iter::once(&mut viewer));
        let (_, wait) = relay.take_replies(&mut screen, true);
        assert_eq!(relay.expire(wait.unwrap()), b"\x1b[?62;22c");
        relay.feed(&mut screen, b"\x1b_Gi=32,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c");
        relay.expect_answers(std::iter::once(&mut viewer));
        let (_, wait) = relay.take_replies(&mut screen, true);
        assert!(relay.answered(&viewer.feed(b"\x1b_Gi=31;OK\x1b\\")).is_empty());
        assert_eq!(relay.answered(&viewer.feed(b"\x1b_Gi=32;OK\x1b\\")), b"\x1b[?62;22c");
        assert!(relay.expire(wait.unwrap()).is_empty());
    }

    #[test]
    fn no_viewer_preserves_reply_order_even_when_prompt_queries_follow_da1() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        let mut relay = Relay::default();
        relay.feed(&mut screen, b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[6n\x1b[5n");
        assert_eq!(relay.take_replies(&mut screen, false), (b"\x1b[?62;22c\x1b[1;1R\x1b[0n".to_vec(), None));
    }

    #[test]
    fn coalesced_queries_need_each_answer_but_skipped_feature_probes_do_not() {
        let mut screen = Screen::new(80, 24, 1 << 20);
        let mut relay = Relay::default();
        let mut viewer = Answers::default();
        relay.feed(&mut screen, b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b_Gi=32,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b_Ga=q,i=33,e=T,U=1\x1b\\\x1b[c");
        relay.expect_answers(std::iter::once(&mut viewer));
        let (_, wait) = relay.take_replies(&mut screen, true);
        assert!(relay.answered(&viewer.feed(b"\x1b_Gi=31;OK\x1b\\")).is_empty());
        assert_eq!(relay.answered(&viewer.feed(b"\x1b_Gi=32;ENOENT:no image\x1b\\")), b"\x1b[?62;22c");
        assert!(relay.expire(wait.unwrap()).is_empty());
    }

    #[test]
    fn payload_free_commands_wait_regardless_of_parameter_order() {
        for command in [b"\x1b_Ga=p,i=31\x1b\\".as_slice(), b"\x1b_Gi=31,a=p\x1b\\"] {
            let mut screen = Screen::new(80, 24, 1 << 20);
            let mut relay = Relay::default();
            let mut viewer = Answers::default();
            relay.feed(&mut screen, command);
            relay.feed(&mut screen, b"\x1b[c");
            relay.expect_answers(std::iter::once(&mut viewer));
            let (now, wait) = relay.take_replies(&mut screen, true);
            assert!(now.is_empty(), "{command:?}: DA1 must wait");
            assert!(wait.is_some(), "{command:?}: placement starts a wait");
            assert_eq!(relay.answered(&viewer.feed(b"\x1b_Gi=31;ENOENT: image not found\x1b\\")), b"\x1b[?62;22c");
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
