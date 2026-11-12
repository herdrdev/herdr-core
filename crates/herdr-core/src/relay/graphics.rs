//! Kitty reply framing and per-viewer correlation, independent of input scheduling.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_kitty_apcs_do_not_affect_answer_correlation() {
        let mut answers = Answers::default();
        answers.expect(key(b"\x1b_Gi=31;OK\x1b\\").unwrap(), 1);
        assert!(answers.feed(b"\x1b_;OK\x1b\\").is_empty());
        assert!(answers.feed(b"\x1b_;ERROR:bad\x1b\\").is_empty());
        assert_eq!(answers.feed(b"\x1b_Gi=31;OK\x1b\\"), [(12, 1)]);
    }

    #[test]
    fn number_answers_can_include_an_allocated_id_and_silent_viewers_use_bounded_memory() {
        assert!(expects_answer(b"\x1b_Ga=T,i=31,U=1;AAAA\x1b\\"));
        assert!(!expects_answer(b"\x1b_Ga=q,i=31,U=1\x1b\\"));
        assert!(!expects_answer(b"\x1b_Gi=31,q=2\x1b\\"));
        assert!(!expects_answer(b"\x1b_Gi=31,U=1,a=q\x1b\\"));
        let mut answers = Answers::default();
        answers.expect(key(b"\x1b_GI=3,a=q;AAAA\x1b\\").unwrap(), 1);
        assert_eq!(answers.feed(b"\x1b_Gi=42,I=3;OK\x1b\\"), [(16, 1)]);
        let key = key(b"\x1b_Gi=31;OK\x1b\\").unwrap();
        for query in 1..10_000 {
            answers.expect(key.clone(), query);
        }
        assert!(answers.saturated);
        assert!(answers.pending.is_empty());
        assert!(answers.feed(b"\x1b_Gi=31;OK\x1b\\").is_empty(), "uncertain history cannot release a newer wait");
    }
}

use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct Parser {
    state: u8,
    bytes: Vec<u8>,
    previous: u8,
    paste: bool,
    overflow: bool,
}

impl Parser {
    pub(super) fn push(&mut self, byte: u8) -> Option<Vec<u8>> {
        match self.state {
            0 => {
                if byte == 0x1b {
                    self.bytes.clear();
                    self.bytes.push(byte);
                    self.state = 1;
                }
            }
            1 => {
                self.bytes.push(byte);
                self.state = match byte {
                    b'_' if self.paste => 0,
                    b'_' => 2,
                    b'[' => 3,
                    0x1b => {
                        self.bytes.pop();
                        1
                    }
                    _ => 0,
                };
                self.overflow = false;
            }
            2 => {
                if self.bytes.len() < 512 {
                    self.bytes.push(byte);
                } else {
                    self.overflow = true;
                }
                if self.previous == 0x1b && byte == b'\\' {
                    self.state = 0;
                    self.previous = 0;
                    if !self.paste {
                        return Some(std::mem::take(&mut self.bytes));
                    }
                }
            }
            _ => {
                self.bytes.push(byte);
                if (0x40..=0x7e).contains(&byte) || self.bytes.len() > 64 {
                    if self.bytes == b"\x1b[200~" {
                        self.paste = true;
                    } else if self.bytes == b"\x1b[201~" {
                        self.paste = false;
                    }
                    self.state = 0;
                }
            }
        }
        self.previous = byte;
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Key(Vec<(u8, u32)>);

fn header(sequence: &[u8]) -> Option<&[u8]> {
    // Long uploads retain only their prefix; complete commands include ST.
    let sequence = sequence.strip_suffix(b"\x1b\\").unwrap_or(sequence);
    sequence.strip_prefix(b"\x1b_G")?.split(|b| *b == b';').next()
}

pub(super) fn key(sequence: &[u8]) -> Option<Key> {
    let header = header(sequence)?;
    let mut ids = Vec::new();
    for field in header.split(|b| *b == b',') {
        if matches!(field.first(), Some(b'i' | b'I' | b'p')) {
            if field.get(1) != Some(&b'=') {
                return None;
            }
            let number = std::str::from_utf8(&field[2..]).ok()?.parse::<u32>().ok()?;
            ids.push((field[0], number));
        }
    }
    ids.sort_unstable();
    (!ids.is_empty()).then_some(Key(ids))
}

pub(super) fn expects_answer(sequence: &[u8]) -> bool {
    let Some(header) = header(sequence) else {
        return false;
    };
    let fields: Vec<_> = header.split(|b| *b == b',').collect();
    // Extended feature queries may deliberately receive no answer. U=1 on
    // an upload, however, is an ordinary Unicode-placeholder image.
    let feature =
        fields.contains(&b"a=q".as_slice()) && fields.iter().any(|f| f.starts_with(b"e=") || f.starts_with(b"U="));
    !fields.contains(&b"q=2".as_slice()) && !feature
}

#[derive(Default)]
pub(crate) struct Answers {
    parser: Parser,
    pending: VecDeque<(Key, u64)>,
    saturated: bool,
}

impl Answers {
    pub(super) fn expect(&mut self, key: Key, query: u64) {
        if self.saturated {
            return;
        }
        // A silent viewer must not retain unbounded history. Once correlation
        // is ambiguous, this viewer uses the bounded timeout until reattached.
        if self.pending.len() == 4096 {
            self.pending.clear();
            self.saturated = true;
        } else {
            self.pending.push_back((key, query));
        }
    }

    pub(crate) fn feed(&mut self, input: &[u8]) -> Vec<(usize, u64)> {
        let mut answered = Vec::new();
        for (at, &byte) in input.iter().enumerate() {
            let Some(sequence) = self.parser.push(byte) else { continue };
            if self.parser.overflow {
                continue;
            }
            let Some(body) =
                sequence.strip_suffix(b"\x1b\\").and_then(|s| s.iter().position(|b| *b == b';').map(|at| &s[at + 1..]))
            else {
                continue;
            };
            let error = body
                .iter()
                .position(|b| *b == b':')
                .is_some_and(|at| at > 1 && body[..at].iter().all(u8::is_ascii_uppercase) && body[0] == b'E');
            if body != b"OK" && !error {
                continue;
            }
            let Some(header) = header(&sequence) else { continue };
            if header.split(|b| *b == b',').any(|f| !matches!(f.first(), Some(b'i' | b'I' | b'p'))) {
                continue;
            }
            let Some(key) = key(&sequence) else { continue };
            if let Some(pending) =
                self.pending.iter().position(|(expected, _)| expected.0.iter().all(|id| key.0.contains(id)))
            {
                answered.push((at + 1, self.pending.remove(pending).unwrap().1));
            }
        }
        answered
    }
}
