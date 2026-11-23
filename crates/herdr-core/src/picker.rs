//! A small fuzzy picker for commands run in a terminal without a target, and
//! the row layout `hc ls` shares with it.

use std::collections::HashSet;

use crate::attach::{RawMode, out, read_stdin, window_size};

/// A parent with more children than this shows the first `FOLD_KEEP` and a
/// "N more" row until opened or searched.
const FOLD: usize = 3;
const FOLD_KEEP: usize = 2;

#[derive(Clone)]
pub struct Item {
    pub value: String,
    /// One column left of the tree guides, like a colored status dot.
    pub mark: String,
    /// The column the tree guides indent, like the address.
    pub head: String,
    /// The rest of the row, already lined up across items.
    pub rest: String,
    /// What a narrow screen shows instead of `rest`: a few words, then `tail`
    /// at the right edge.
    pub short: String,
    pub tail: String,
    /// The parent item's value: filtering keeps parents of a match on screen.
    pub parent: Option<String>,
}

#[derive(PartialEq, Debug)]
enum Row {
    Item(usize),
    /// Children of the item at `parent` folded away, `hidden` items in all.
    More {
        parent: usize,
        hidden: usize,
    },
}

/// The `shown` items in tree order with guide prefixes: each child right
/// under its parent, and a child whose parent isn't shown is a root. With
/// `fold`, parents not in it fold their children.
fn layout(items: &[Item], shown: &[usize], fold: Option<&HashSet<usize>>) -> Vec<(Row, String)> {
    let kids = |i: usize| -> Vec<usize> {
        shown.iter().copied().filter(|&k| items[k].parent.as_deref() == Some(items[i].value.as_str())).collect()
    };
    fn count(k: usize, kids: &dyn Fn(usize) -> Vec<usize>) -> usize {
        1 + kids(k).into_iter().map(|c| count(c, kids)).sum::<usize>()
    }
    fn walk(
        i: usize,
        own: String,
        below: String,
        kids: &dyn Fn(usize) -> Vec<usize>,
        fold: Option<&HashSet<usize>>,
        out: &mut Vec<(Row, String)>,
    ) {
        out.push((Row::Item(i), own));
        let kids_here = kids(i);
        let keep = match fold {
            Some(open) if kids_here.len() > FOLD && !open.contains(&i) => FOLD_KEEP,
            _ => kids_here.len(),
        };
        for (n, &k) in kids_here[..keep].iter().enumerate() {
            let last = n + 1 == kids_here.len();
            let (own, next) = if last { ("\u{2514}\u{2500} ", "   ") } else { ("\u{251c}\u{2500} ", "\u{2502}  ") };
            walk(k, format!("{below}{own}"), format!("{below}{next}"), kids, fold, out);
        }
        if keep < kids_here.len() {
            let hidden = kids_here[keep..].iter().map(|&k| count(k, kids)).sum();
            out.push((Row::More { parent: i, hidden }, format!("{below}\u{2514}\u{2500} ")));
        }
    }
    let mut out = Vec::new();
    for &i in shown {
        let root = items[i].parent.as_deref().is_none_or(|p| !shown.iter().any(|&s| items[s].value == p));
        if root {
            walk(i, String::new(), String::new(), &kids, fold, &mut out);
        }
    }
    out
}

/// One line per row, the heads padded so the rest lines up. Rows wider than
/// `cols` switch to the short form, cut to fit with the tail kept.
fn draw(items: &[Item], rows: &[(Row, String)], cols: usize) -> Vec<String> {
    let head = |i: usize, prefix: &str| format!("{prefix}{}", items[i].head);
    let width = rows
        .iter()
        .filter_map(|(row, prefix)| match row {
            Row::Item(i) => Some(head(*i, prefix).chars().count()),
            Row::More { .. } => None,
        })
        .max()
        .unwrap_or(0);
    let lines = |rest: &dyn Fn(&Item) -> String| -> Vec<String> {
        rows.iter()
            .map(|(row, prefix)| match row {
                Row::Item(i) => {
                    let head = head(*i, prefix);
                    let pad = width - head.chars().count();
                    format!("{} {head}{}  {}", items[*i].mark, " ".repeat(pad), rest(&items[*i])).trim_end().to_string()
                }
                Row::More { hidden, .. } => format!("  {prefix}\x1b[2m{hidden} more\x1b[22m"),
            })
            .collect()
    };
    let wide = lines(&|item| item.rest.clone());
    if wide.iter().all(|line| plain(line).chars().count() <= cols) {
        return wide;
    }
    lines(&|item| {
        let tail = plain(&item.tail).chars().count();
        let room = cols.saturating_sub(width + 4 + tail + 2 * usize::from(tail > 0));
        let short = match item.short.chars().count() {
            n if n <= room => item.short.clone(),
            _ => clip(&item.short, room.saturating_sub(1)).trim_end().to_string() + "\u{2026}",
        };
        let gap = room.saturating_sub(plain(&short).chars().count());
        let sep = if tail > 0 { "  " } else { "" };
        format!("{short}{}{sep}{}", " ".repeat(gap), item.tail)
    })
}

/// Every item as a tree, nothing folded: the `hc ls` table.
pub fn table(items: &[Item], cols: usize) -> Vec<String> {
    let all: Vec<usize> = (0..items.len()).collect();
    draw(items, &layout(items, &all, None), cols)
}

/// Items matching `query`, plus the parents of each match.
fn visible(items: &[Item], query: &str) -> Vec<usize> {
    let mut keep: Vec<bool> =
        items.iter().map(|i| matches(&format!("{} {} {}", i.head, i.rest, i.short), query)).collect();
    for i in 0..items.len() {
        if !keep[i] {
            continue;
        }
        let mut parent = items[i].parent.as_deref();
        while let Some(p) = parent.and_then(|p| items.iter().position(|it| it.value == p)) {
            keep[p] = true;
            parent = items[p].parent.as_deref();
        }
    }
    (0..items.len()).filter(|&i| keep[i]).collect()
}

/// Labels may carry color codes (`ESC [ ... m`); this walks the visible text.
fn shown_chars(label: &str) -> impl Iterator<Item = (bool, char)> + '_ {
    let mut in_code = false;
    label.chars().map(move |c| {
        let code = in_code || c == '\x1b';
        in_code = code && c != 'm';
        (!code, c)
    })
}

/// The label without its color codes.
pub fn plain(label: &str) -> String {
    shown_chars(label).filter(|(shown, _)| *shown).map(|(_, c)| c).collect()
}

/// The first `n` visible characters, color codes kept.
fn clip(label: &str, n: usize) -> String {
    let mut shown = 0;
    shown_chars(label)
        .take_while(|(v, _)| {
            shown += usize::from(*v);
            shown <= n
        })
        .map(|(_, c)| c)
        .collect()
}

/// Every character of `query` appears in `text`, in order.
fn matches(text: &str, query: &str) -> bool {
    let text = plain(text);
    let mut chars = text.chars().flat_map(char::to_lowercase);
    query.chars().flat_map(char::to_lowercase).all(|q| chars.any(|c| c == q))
}

#[derive(PartialEq, Debug)]
enum Key {
    Text(char),
    Enter,
    Cancel,
    Up,
    Down,
    Back,
    Tab,
    Other,
}

/// Every key in one read (a read may hold several, or text plus Enter).
fn keys(input: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let rest = &input[i..];
        let (key, len) = match rest {
            [b'\r' | b'\n', ..] => (Key::Enter, 1),
            [0x03, ..] | [0x1b] => (Key::Cancel, 1),
            [0x10, ..] => (Key::Up, 1),
            [0x0e, ..] => (Key::Down, 1),
            [0x7f | 0x08, ..] => (Key::Back, 1),
            [b'\t', ..] => (Key::Tab, 1),
            [0x1b, b'[' | b'O', more @ ..] => {
                let end = more.iter().position(|b| (0x40..=0x7e).contains(b)).unwrap_or(more.len().saturating_sub(1));
                let key = match more.get(end) {
                    Some(b'A') => Key::Up,
                    Some(b'B') => Key::Down,
                    _ => Key::Other,
                };
                (key, end + 3)
            }
            [0x1b, ..] => (Key::Other, 2),
            _ => {
                let len = (1..=4.min(rest.len())).find(|n| std::str::from_utf8(&rest[..*n]).is_ok()).unwrap_or(1);
                match std::str::from_utf8(&rest[..len]).ok().and_then(|s| s.chars().next()) {
                    Some(c) if !c.is_control() => (Key::Text(c), len),
                    _ => (Key::Other, len),
                }
            }
        };
        keys.push(key);
        i += len.min(rest.len()).max(1);
    }
    keys
}

/// The next input, whole sequences only: a cut-off one waits for its rest, or
/// 50 ms (then it was a lone Esc). Empty after 100 ms of nothing, so a resize
/// gets redrawn. None: stdin closed or we were told to stop.
fn next_input(carry: &mut Vec<u8>) -> Option<Vec<u8>> {
    if crate::attach::terminated() {
        return None;
    }
    let chunk = read_stdin(if carry.is_empty() { 100 } else { 50 })?;
    let mut input = std::mem::take(carry);
    if !chunk.is_empty() {
        input.extend(chunk);
        *carry = input.split_off(crate::attach::incomplete_tail(&input));
    }
    Some(input)
}

/// The whole screen: title, prompt with the order and match count, a rule,
/// the rows scrolled so `selected` shows, and key hints on the last line.
fn screen(
    title: &str,
    query: &str,
    sort: &str,
    count: (usize, usize),
    lines: &[String],
    selected: usize,
    top: usize,
) -> String {
    let (cols, rows) = window_size();
    let (cols, rows) = (cols as usize, rows as usize);
    let tally = format!("{sort}  {}/{}", count.0, count.1);
    let gap = cols.saturating_sub(query.chars().count() + tally.len() + 6);
    let mut s = format!("\x1b[H\x1b[2J \x1b[1m{title}\x1b[22m\r\n");
    s.push_str(&format!(" \x1b[36m\u{203a}\x1b[39m {query}\u{258f}{}\x1b[2m{tally}\x1b[22m\r\n", " ".repeat(gap)));
    s.push_str(&format!("\x1b[2m{}\x1b[22m\r\n", "\u{2500}".repeat(cols)));
    for (i, line) in lines.iter().enumerate().skip(top).take(rows.saturating_sub(4)) {
        let line = clip(line, cols.saturating_sub(2));
        if i == selected {
            let pad = cols.saturating_sub(plain(&line).chars().count() + 2);
            s.push_str(&format!("\x1b[7m\x1b[36m\u{203a}\x1b[39m {line}{}\x1b[0m\r\n", " ".repeat(pad)));
        } else {
            s.push_str(&format!("  {line}\r\n"));
        }
    }
    s.push_str(&format!("\x1b[{rows};1H \x1b[2m\u{2191}\u{2193} move   enter choose   tab sort   esc quit\x1b[22m"));
    s
}

/// Shows the items in the first of `orders` (Tab moves to the next); returns
/// the chosen value, or None on Esc / ctrl+c. Each order holds the same items.
pub fn pick(title: &str, orders: &[(&str, Vec<Item>)]) -> Option<String> {
    let mut sort = 0;
    let _raw = RawMode::enter();
    crate::attach::catch_termination();
    out(b"\x1b[?1049h\x1b[?25l");
    let (mut query, mut selected, mut top, mut carry) = (String::new(), 0usize, 0usize, Vec::new());
    let (mut open, mut drawn) = (HashSet::new(), String::new());
    let chosen = loop {
        let (name, items) = &orders[sort];
        let shown = visible(items, &query);
        let rows = layout(items, &shown, query.is_empty().then_some(&open));
        selected = selected.min(rows.len().saturating_sub(1));
        let height = (window_size().1 as usize).saturating_sub(4).max(1);
        top = top.min(selected).max((selected + 1).saturating_sub(height));
        let cols = (window_size().0 as usize).saturating_sub(2);
        let now = screen(title, &query, name, (shown.len(), items.len()), &draw(items, &rows, cols), selected, top);
        if now != drawn {
            out(now.as_bytes());
            drawn = now;
        }

        let Some(input) = next_input(&mut carry) else { break None };
        let mut leave = None;
        for key in keys(&input) {
            match key {
                Key::Cancel => leave = Some(None),
                // Whatever was typed in the same read filters first.
                Key::Enter => {
                    let items = &orders[sort].1;
                    let rows = layout(items, &visible(items, &query), query.is_empty().then_some(&open));
                    match rows.get(selected.min(rows.len().saturating_sub(1))) {
                        Some((Row::Item(i), _)) => leave = Some(Some(items[*i].value.clone())),
                        Some((Row::More { parent, .. }, _)) => {
                            open.insert(*parent);
                        }
                        None => leave = Some(None),
                    }
                }
                Key::Up => selected = selected.saturating_sub(1),
                Key::Down => selected += 1,
                Key::Back => {
                    query.pop();
                }
                Key::Text(c) => query.push(c),
                // Folds are kept by row index, which another order moves.
                Key::Tab => {
                    (sort, selected) = ((sort + 1) % orders.len(), 0);
                    open.clear();
                }
                Key::Other => {}
            }
            if leave.is_some() {
                break;
            }
        }
        if let Some(result) = leave {
            break result;
        }
    };
    out(b"\x1b[?25h\x1b[?1049l");
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(value: &str, rest: &str, parent: Option<&str>) -> Item {
        let (short, tail) = (String::new(), String::new());
        Item {
            value: value.into(),
            mark: "o".into(),
            head: value.into(),
            rest: rest.into(),
            short,
            tail,
            parent: parent.map(Into::into),
        }
    }

    #[test]
    fn a_narrow_screen_shows_the_short_form_and_keeps_the_status() {
        let mut it = item("t1", "a-long-name  pi  some-folder  done", None);
        (it.short, it.tail) = ("herdr-core / review".into(), "done".into());
        let rows = layout(std::slice::from_ref(&it), &[0], None);
        assert_eq!(draw(std::slice::from_ref(&it), &rows, 80), ["o t1  a-long-name  pi  some-folder  done"]);
        let narrow = draw(std::slice::from_ref(&it), &rows, 24).remove(0);
        assert_eq!(narrow.chars().count(), 24, "{narrow}");
        assert!(
            narrow.starts_with("o t1  herdr-core") && narrow.contains('\u{2026}') && narrow.ends_with("  done"),
            "{narrow}"
        );
        assert!(visible(std::slice::from_ref(&it), "review") == [0], "the short form is searched too");
    }

    #[test]
    fn one_read_can_hold_many_keys() {
        use Key::*;
        assert_eq!(keys(b"be\t\r"), [Text('b'), Text('e'), Tab, Enter]);
        assert_eq!(keys(b"\x1b[A\x1bOB\x1b"), [Up, Down, Cancel]);
        assert_eq!(keys("\u{e9}\x7f\x1bx\x1b[1;5C".as_bytes()), [Text('\u{e9}'), Back, Other, Other]);
    }

    #[test]
    fn children_sit_under_their_parent_with_guide_lines() {
        let items = [
            item("t1", "", None),
            item("t2", "", None),
            item("t4", "", Some("t1")),
            item("t5", "", Some("t1")),
            item("t6", "", Some("t4")),
            item("t7", "", Some("t9")),
        ];
        let want = [
            "o t1",
            "o \u{251c}\u{2500} t4",
            "o \u{2502}  \u{2514}\u{2500} t6",
            "o \u{2514}\u{2500} t5",
            "o t2",
            "o t7",
        ];
        assert_eq!(table(&items, usize::MAX), want, "a child of a finished parent is a root");
    }

    #[test]
    fn many_children_fold_until_opened() {
        let mut items = vec![item("t1", "", None)];
        items.extend((2..=6).map(|n| item(&format!("t{n}"), "", Some("t1"))));
        items.push(item("t7", "", Some("t6")));
        let all: Vec<usize> = (0..items.len()).collect();
        let rows: Vec<Row> = layout(&items, &all, Some(&HashSet::new())).into_iter().map(|(r, _)| r).collect();
        assert_eq!(rows, [Row::Item(0), Row::Item(1), Row::Item(2), Row::More { parent: 0, hidden: 4 }]);
        assert_eq!(layout(&items, &all, Some(&HashSet::from([0]))).len(), items.len());
    }

    #[test]
    fn colors_are_kept_but_never_matched_or_counted() {
        let label = "t1 \x1b[33mworking\x1b[22;39m here";
        assert_eq!(plain(label), "t1 working here");
        assert!(!matches(label, "33m"));
        assert_eq!(clip(label, 6), "t1 \x1b[33mwor");
    }

    #[test]
    fn a_match_keeps_its_parents_on_screen() {
        let items = [
            item("t1", "boss", None),
            item("t2", "web", None),
            item("t4", "mid", Some("t1")),
            item("t6", "tests", Some("t4")),
        ];
        assert_eq!(visible(&items, "tests"), [0, 2, 3]);
        assert_eq!(visible(&items, ""), [0, 1, 2, 3]);
    }

    #[test]
    fn the_selected_row_uses_the_terminals_own_colors() {
        let lines = ["o t1".to_string(), "o t2".to_string()];
        let s = screen("hc", "", "recent", (2, 2), &lines, 1, 0);
        assert!(s.contains("\x1b[7m\x1b[36m\u{203a}\x1b[39m o t2"), "reverse video suits any theme: {s:?}");
        assert!(!s.contains("\x1b[48;5;"), "a fixed palette background is dark on dark in light themes: {s:?}");
    }

    #[test]
    fn fuzzy_matching_is_an_ordered_subsequence() {
        assert!(matches("t3  reviewer  live", "rvw"));
        assert!(matches("t3  Reviewer", "REV"));
        assert!(!matches("t3  reviewer", "rt"));
        assert!(matches("anything", ""));
    }
}
