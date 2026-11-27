#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "dummy-token-replace-me";
    const TIME: u64 = 1795543200;
    const CLEAR: &[u8] =
        b"pid=74877:time=1795543200:clear=1:mac=c30603369f98fef581a4098be889175b0c74f0641e49f8b25f4326cb38f97c93";

    #[test]
    fn authenticates_the_published_reports() {
        for (body, expected) in [
            (
                "pid=74877:time=1795543200:argv=bnZpbQ==,LVM=,L2hvbWUvdXNlci8ubG9jYWwvc3RhdGUvbnZpbS9zZXNzaW9ucy84ZjJjMWQudmlt:mac=9dbb99bfb856749a87563839d7ce9062818d78cf25909db93f309e0cd3b1909b",
                "nvim",
            ),
            (
                "pid=74877:time=1795543200:argv=bGF6eWdpdA==:cwd=L2hvbWUvdXNlci9zcmMvcHJvamVjdA==:mac=35cb909805c380336efd3689a125acc774a9dec8422a1d21492ebec06ca758e1",
                "lazygit",
            ),
            (
                "pid=90412:time=1795543200:argv=cmVzdW1lLXdyYXA=,c3No,ZGV2Ym94:mac=e76149d0ba54a83570ffab0583867dacdfa23928ba9d18604f0badea88b62b65",
                "resume-wrap",
            ),
        ] {
            let report = authenticate(body.as_bytes(), TOKEN, TIME).unwrap();
            if cfg!(windows) && expected == "lazygit" {
                assert_eq!(report.action, None, "the example cwd is Unix-only");
            } else {
                let Some(Action::Set { argv, .. }) = report.action else { panic!("set report") };
                assert_eq!(argv[0], expected);
            }
        }
        assert_eq!(authenticate(CLEAR, TOKEN, TIME).unwrap().action, Some(Action::Clear));
        assert!(authenticate(CLEAR, TOKEN, TIME - 60).is_some());
        assert!(authenticate(CLEAR, TOKEN, TIME + 60).is_some());
        assert!(authenticate(CLEAR, TOKEN, TIME - 61).is_none());
        assert!(authenticate(CLEAR, TOKEN, TIME + 61).is_none());
        assert!(authenticate(CLEAR, "other-token", TIME).is_none());
    }

    #[test]
    fn frames_at_every_split_and_restarts_after_an_unterminated_string() {
        for end in [b"\x07".as_slice(), b"\x1b\\"] {
            let frame = [OPEN, CLEAR, end].concat();
            for split in 0..=frame.len() {
                let mut stream = Stream::default();
                let mut reports = stream.feed(&frame[..split]);
                reports.extend(stream.feed(&frame[split..]));
                assert_eq!(reports, [(frame.len() as u64, CLEAR.to_vec())]);
            }
            let mut stream = Stream::default();
            let interrupted = [b"output\x1b]0;".as_slice(), &frame].concat();
            assert_eq!(stream.feed(&interrupted), [(interrupted.len() as u64, CLEAR.to_vec())]);
            assert!(stream.feed(&[OPEN, CLEAR, b"\x18"].concat()).is_empty());
            assert_eq!(stream.feed(&frame).len(), 1);

            let size = LIMIT - OPEN.len() - end.len();
            assert_eq!(stream.feed(&[OPEN, &vec![b'a'; size], end].concat()).len(), 1);
            assert!(stream.feed(&[OPEN, &vec![b'a'; size + 1], end].concat()).is_empty());
        }
    }

    #[test]
    fn authenticated_malformed_reports_reject_but_untrusted_ones_are_ignored() {
        let parse = |fields| {
            let body = format!("pid=5:time={TIME}:{fields}");
            let body = format!("{body}:mac={}", mac(body.as_bytes(), TOKEN));
            authenticate(body.as_bytes(), TOKEN, TIME)
        };
        for bad in [
            "argv=Yg==:argv=Yg==",
            "argv=Yg==:clear=1",
            "argv=Yg==:need=later",
            "argv=Yg==:Tag=x",
            "argv=Yg==:unknown=!",
            "argv=Yg==:need=",
            "argv=Yg=",
            "argv=Yh==",
            "argv=Yg===",
            "argv=Yg==,wqA=,AA==",
            "argv=Yg==,f4A=",
            "argv=Yg==,woA=",
            "argv=",
            "argv=Li9h",
            "argv=LWE=",
            "argv=Yg==:cwd=cmVs",
            "clear=0",
            "clear=1:pid=5",
        ] {
            assert_eq!(parse(bad).unwrap().action, None, "{bad}");
        }
        assert_eq!(
            parse("argv=Yg,LA==,:unused=abc").unwrap().action,
            Some(Action::Set { argv: vec!["b".into(), ",".into(), "".into()], cwd: None })
        );
        assert!(parse("clear=1:pid=6").is_none());
        assert!(parse("clear=1:time=99999999999999999999999").is_none());
        let args = format!("argv=Yg{}", ",YQ".repeat(63));
        assert!(parse(&args).unwrap().action.is_some());
        assert_eq!(parse(&format!("{args},YQ")).unwrap().action, None);
    }
}
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;

const OPEN: &[u8] = b"\x1b]7577;";
const LIMIT: usize = 8192;

#[derive(Clone, Default, PartialEq)]
pub(super) struct Stream {
    offset: u64,
    frame: Frame,
    body: Vec<u8>,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Frame {
    #[default]
    Ground,
    Escape {
        report: bool,
    },
    Osc(usize),
    Body,
    Discard {
        bel: bool,
    },
}

impl Stream {
    pub(super) fn at(offset: u64) -> Self {
        Self { offset, ..Self::default() }
    }

    pub(super) fn feed(&mut self, mut bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let mut reports = Vec::new();
        loop {
            if matches!(self.frame, Frame::Ground) {
                let skip = bytes.iter().position(|&byte| byte == 0x1b).unwrap_or(bytes.len());
                self.offset += skip as u64;
                bytes = &bytes[skip..];
            }
            let Some((&byte, rest)) = bytes.split_first() else { break };
            bytes = rest;
            self.offset += 1;
            let frame = self.frame;
            if matches!((frame, byte), (Frame::Escape { report: true }, b'\\') | (Frame::Body, 7)) {
                let terminator_len = if byte == 7 { 1 } else { 2 };
                if OPEN.len() + self.body.len() + terminator_len <= LIMIT {
                    reports.push((self.offset, std::mem::take(&mut self.body)));
                }
                self.body.clear();
                self.frame = Frame::Ground;
                continue;
            }
            if matches!(frame, Frame::Escape { .. }) || matches!(byte, 7 | 0x18 | 0x1a) {
                self.body.clear();
            }
            self.frame = match (frame, byte) {
                (_, 0x1b) => Frame::Escape { report: matches!(frame, Frame::Body) },
                (Frame::Escape { .. }, b']') => Frame::Osc(2),
                (Frame::Escape { .. }, b'P' | b'X' | b'^' | b'_') => Frame::Discard { bel: false },
                (Frame::Ground | Frame::Escape { .. }, _) | (_, 0x18 | 0x1a) => Frame::Ground,
                (Frame::Discard { bel: false }, _) => frame,
                (_, 7) => Frame::Ground,
                (Frame::Osc(i), byte) if byte == OPEN[i] && i + 1 == OPEN.len() => Frame::Body,
                (Frame::Osc(i), byte) if byte == OPEN[i] => Frame::Osc(i + 1),
                (Frame::Body, byte) if OPEN.len() + self.body.len() + 1 < LIMIT => {
                    self.body.push(byte);
                    Frame::Body
                }
                _ => Frame::Discard { bel: true },
            };
        }
        reports
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Action {
    Clear,
    Set { argv: Vec<String>, cwd: Option<String> },
}

pub(super) struct Report {
    pub pid: u32,
    pub time: u64,
    pub action: Option<Action>,
}

fn mac(body: &[u8], token: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(body);
    hash.update(b":");
    hash.update(token);
    format!("{:x}", hash.finalize())
}

fn number<T: std::str::FromStr>(s: &[u8]) -> Option<T> {
    if s.first().is_none_or(|b| !matches!(b, b'1'..=b'9')) || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

fn envelope<'a>(body: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut values = body.split(|b| *b == b':').filter_map(|pair| pair.strip_prefix(key));
    let first = values.next()?;
    values.all(|value| value == first).then_some(first)
}

pub(super) fn authenticate(body: &[u8], token: &str, now: u64) -> Option<Report> {
    let end = body.len().checked_sub(b":mac=".len() + 64)?;
    let (body, suffix) = body.split_at(end);
    let sent = suffix.strip_prefix(b":mac=")?;
    let expected = mac(body, token);
    if sent.iter().zip(expected.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) != 0 {
        return None;
    }
    let time: u64 = number(envelope(body, b"time=")?)?;
    if now.abs_diff(time) > 60 {
        return None;
    }
    let pid = number(envelope(body, b"pid=")?)?;
    Some(Report { pid, time, action: std::str::from_utf8(body).ok().and_then(parse) })
}

fn decode(text: &str) -> Option<String> {
    let bytes = crate::put::decode(text)?;
    let canonical = crate::put::encode(&bytes);
    if text != canonical && text != canonical.trim_end_matches('=') {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    (!text.chars().any(char::is_control)).then_some(text)
}

fn command(text: &str) -> bool {
    let path = Path::new(text);
    path.is_absolute()
        || (!text.starts_with('-')
            && !text.chars().any(std::path::is_separator)
            && matches!(path.components().next(), Some(std::path::Component::Normal(_))))
}

fn parse(body: &str) -> Option<Action> {
    const KEYS: &[&str] = &["pid", "time", "mac", "argv", "cwd", "clear", "need"];
    let mut keys = HashSet::from(["mac"]);
    let (mut argv, mut cwd, mut clear) = (None, None, false);
    for pair in body.split(':') {
        let (key, value) = pair.split_once('=')?;
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_lowercase())
            || !keys.insert(key)
            || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b))
        {
            return None;
        }
        match key {
            "argv" => {
                let args = value.split(',').take(65).map(decode).collect::<Option<Vec<_>>>()?;
                if args.len() > 64 || !command(&args[0]) {
                    return None;
                }
                argv = Some(args);
            }
            "cwd" => {
                let dir = decode(value)?;
                if !Path::new(&dir).is_absolute() {
                    return None;
                }
                cwd = Some(dir);
            }
            "clear" if value == "1" => clear = true,
            "clear" => return None,
            "need" if !value.split(',').all(|key| KEYS.contains(&key)) => return None,
            _ => {}
        }
    }
    match (clear, argv) {
        (true, None) => Some(Action::Clear),
        (false, Some(argv)) => Some(Action::Set { argv, cwd }),
        _ => None,
    }
}
