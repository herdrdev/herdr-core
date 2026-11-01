//! Reading a full-screen app's history by scrolling it: glue each page onto
//! what was already read. Rows are screen rows, each exactly as wide as the screen.

use crate::runtime::Core;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Of the rows two screens could share, how many must match to be "the same screen".
const SIMILAR_PERCENT: usize = 70;
/// Of the rows two pages overlap in, how many must match to trust a scroll distance.
const ALIGN_PERCENT: usize = 30;

#[derive(Debug, PartialEq)]
pub enum Merge {
    /// The page scrolled up: this many new rows were added on top.
    Advanced(usize),
    /// Nothing moved (at the top, or the app ignored the scroll).
    Unchanged,
    /// Can't tell how far it moved: don't trust anything more.
    Unaligned,
}

const TRACK: char = '│';
const THUMBS: [char; 2] = ['┃', '█'];

/// What a row says, for comparing: trailing space trimmed, and a scrollbar
/// column (a run of 3+ rows ending in track and thumb glyphs) left out, since
/// it moves while the text doesn't. The rows themselves are never changed.
fn identities(rows: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = rows.iter().map(|r| r.trim_end().to_string()).collect();
    let last = |r: &String| r.chars().last();
    let mut start = 0;
    while start < rows.len() {
        let width = rows[start].chars().count();
        let (mut end, mut track, mut thumb) = (start, false, false);
        while end < rows.len() && rows[end].chars().count() == width {
            match last(&rows[end]) {
                Some(TRACK) => track = true,
                Some(c) if THUMBS.contains(&c) => thumb = true,
                _ => break,
            }
            end += 1;
        }
        if end - start >= 3 && track && thumb {
            for (id, row) in ids[start..end].iter_mut().zip(&rows[start..end]) {
                let mut text: String = row.chars().collect();
                text.pop();
                *id = text.trim_end().to_string();
            }
        }
        start = end.max(start + 1);
    }
    ids
}

/// Whether two screens show the same thing, give or take a small changing
/// region (a timer, a spinner).
pub fn similar(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let (a, b) = (identities(a), identities(b));
    let pairs: Vec<(&String, &String)> = a.iter().zip(&b).filter(|(x, y)| !x.is_empty() || !y.is_empty()).collect();
    let same = pairs.iter().filter(|(x, y)| x == y).count();
    pairs.is_empty() || same * 100 >= pairs.len() * SIMILAR_PERCENT
}

/// How far `next` scrolled up from `previous`, and a row that proves it: a
/// matching row unique on both pages. Two plausible distances = ambiguous.
fn alignment(previous: &[String], next: &[String]) -> Option<(usize, usize)> {
    let count = |rows: &[String]| {
        let mut n: HashMap<String, usize> = HashMap::new();
        for r in rows {
            *n.entry(r.clone()).or_default() += 1;
        }
        n
    };
    let (in_prev, in_next) = (count(previous), count(next));
    let mut found = None;
    for shift in 1..previous.len() {
        let (mut comparable, mut matches, mut anchor) = (0, 0, None);
        for i in 0..previous.len() - shift {
            let (before, after) = (&previous[i], &next[i + shift]);
            if before.is_empty() || after.is_empty() {
                continue;
            }
            comparable += 1;
            if before == after {
                matches += 1;
                if in_prev[before] == 1 && in_next[after] == 1 {
                    anchor.get_or_insert(i);
                }
            }
        }
        let Some(anchor) = anchor else { continue };
        if matches * 100 < comparable * ALIGN_PERCENT {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some((shift, anchor));
    }
    found
}

/// Glues `next` (one scroll up from `previous`) onto `history`, which starts
/// with `previous`. Overlapping rows are refreshed too: a pinned header may
/// have hidden transcript rows on the previous page.
pub fn merge_up(history: &mut Vec<String>, previous: &[String], next: &[String]) -> Merge {
    if previous.len() != next.len() {
        return Merge::Unaligned;
    }
    let (prev_ids, next_ids) = (identities(previous), identities(next));
    if prev_ids == next_ids {
        return Merge::Unchanged;
    }
    let Some((shift, anchor)) = alignment(&prev_ids, &next_ids) else {
        // Not scrolled, but a small region (a timer, a status row) redrew.
        return if similar(previous, next) { Merge::Unchanged } else { Merge::Unaligned };
    };
    let history_ids = identities(history);
    let raw = previous[anchor].trim_end();
    let Some(at) = history_ids.iter().take(previous.len()).position(|id| *id == prev_ids[anchor] || id == raw) else {
        return Merge::Unaligned;
    };
    let boundary = anchor + shift;
    history.splice(0..at, next[..boundary].iter().cloned());
    Merge::Advanced(boundary - at)
}

/// Wheel notches per scroll step.
const NOTCHES: usize = 3;
/// Output silent this long (and no synchronized frame open) = the redraw is done.
const SETTLE: Duration = Duration::from_millis(10);
/// How long one scroll step may take to show.
const STEP: Duration = Duration::from_millis(120);
const RESTORE_MAX: Duration = Duration::from_secs(10);
/// The longest a step may stretch to for a slow app.
const MAX_PACE: Duration = Duration::from_secs(1);

fn limit() -> Duration {
    Duration::from_millis(std::env::var("HC_STITCH_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(15_000))
}

/// What a stitched read captured: the rows (history, then the screen), how
/// many of them are the screen, and whether it reached the top.
pub struct Capture {
    rows: Vec<String>,
    screen: usize,
    top: bool,
}

impl Capture {
    /// Covers `lines` of history: it reached the top, or holds that many.
    fn covers(&self, lines: usize) -> bool {
        self.top || self.rows.len().saturating_sub(self.screen) >= lines
    }

    /// The screen plus up to `lines` of history, and whether that is all of it.
    fn answer(&self, lines: usize) -> (String, bool) {
        let keep = (lines + self.screen).min(self.rows.len());
        (text(&self.rows[self.rows.len() - keep..]), self.covers(lines))
    }
}

impl Core {
    /// Reads a full-screen app's history by scrolling it with the mouse wheel,
    /// then scrolls it back. Only for an app on the alternate screen that takes
    /// the wheel and isn't working, blocked or in error. `None`: read the plain
    /// screen instead, marked incomplete (any doubt ends up here).
    pub async fn stitch(&self, id: u32, lines: usize) -> Option<(String, bool)> {
        // Nothing printed since a read that covers this one: answer from memory.
        let ready = |st: &crate::runtime::State| -> Result<Option<(String, bool)>, ()> {
            let t = st.reg.terminals.get(&id).filter(|t| t.is_live()).ok_or(())?;
            let screen = st.screens.get(&id).ok_or(())?;
            let busy = t.status.as_ref().is_some_and(|s| matches!(s.state.as_str(), "working" | "blocked" | "error"));
            if !screen.alt_screen() || busy || screen.wheel(true).is_empty() {
                return Err(());
            }
            Ok(st
                .stitched
                .get(&id)
                .filter(|(seq, capture)| st.output_seq.get(&id) == Some(seq) && capture.covers(lines))
                .map(|(_, capture)| capture.answer(lines)))
        };
        if let Some(answer) = ready(&self.lock()).ok()? {
            return Some(answer);
        }
        // The input queue: no send, keys or note in between. A person's keys wait.
        let queue = self.lock().queues.entry(id).or_default().clone();
        let _turn = queue.lock().await;
        {
            // Whatever went in while we queued (a send) may have changed that.
            let mut st = self.lock();
            if let Some(answer) = ready(&st).ok()? {
                return Some(answer);
            }
            st.gates.entry(id).or_default().set_busy(true);
            st.stitching.insert(id);
        }
        let result = self.scroll_and_read(id, lines).await;
        let mut st = self.lock();
        st.stitching.remove(&id);
        let held = st.gates.entry(id).or_default().set_busy(false);
        if let Some(link) = st.link(id) {
            link.input(id, &held);
        }
        if st.screens.contains_key(&id) {
            let paint = crate::viewers::first_paint(&st, id, 0);
            let recovering = st.screens[&id].continuation().is_none();
            for v in st.viewers.get_mut(&id).into_iter().flatten() {
                v.recovering = recovering;
                v.conn.with(|out| {
                    out.data(v.channel, &paint);
                });
            }
        }
        let capture = result?;
        let answer = capture.answer(lines);
        if held.is_empty() {
            let seq = st.output_seq.get(&id).copied().unwrap_or(0);
            st.stitched.insert(id, (seq, capture));
        }
        Some(answer)
    }

    async fn scroll_and_read(&self, id: u32, lines: usize) -> Option<Capture> {
        let deadline = Instant::now() + limit();
        let mut pace = STEP;
        let initial = self.settled(id, deadline).await?;
        // Already scrolled up by someone? Then we can't know where "back" is.
        // One notch: a view that close to the bottom still moves all of it,
        // so one notch up puts it back exactly. Judge the last frame of the
        // step: a notch may redraw via in-between frames ("loading...").
        self.wheel(id, false, 1);
        let started = Instant::now();
        let probe = loop {
            match self.next_page(id, started, &mut pace, deadline, true).await {
                Some((page, true)) => break page,
                Some(_) => {}
                None => {
                    // A key (or the time limit) mid-probe. At the bottom the probe
                    // did nothing; above it, it moved the view down: undo only that.
                    // A screen that never settles can't be judged: leave it.
                    let short = Instant::now() + MAX_PACE * 2;
                    if self.quiet_page(id, short).await.is_some_and(|now| !similar(&now, &initial)) {
                        self.undo_probe(id).await;
                    }
                    return None;
                }
            }
        };
        if !similar(&probe, &initial) {
            self.undo_probe(id).await;
            return None;
        }
        let want = lines + initial.len();
        let (mut history, mut previous) = (initial.clone(), initial.clone());
        let (mut ups, mut top, mut valid, mut still) = (0, false, true, false);
        'scroll: while history.len() < want {
            self.wheel(id, true, NOTCHES);
            ups += NOTCHES;
            let started = Instant::now();
            loop {
                let Some((next, step_over)) = self.next_page(id, started, &mut pace, deadline, true).await else {
                    valid = false;
                    break 'scroll;
                };
                match merge_up(&mut history, &previous, &next) {
                    Merge::Advanced(_) => {
                        (previous, still) = (next, false);
                        continue 'scroll;
                    }
                    // A slow redraw looks like the top: believe it after two quiet
                    // steps and then a quiet second (wheel events may still be queued).
                    Merge::Unchanged if step_over && still => {
                        let Some(last) = self.quiet_page(id, deadline).await else {
                            valid = false;
                            break 'scroll;
                        };
                        if !matches!(merge_up(&mut history, &previous, &last), Merge::Advanced(_)) {
                            top = true;
                            break 'scroll;
                        }
                        (previous, still) = (last, false);
                        continue 'scroll;
                    }
                    Merge::Unchanged if step_over => {
                        still = true;
                        continue 'scroll;
                    }
                    Merge::Unaligned if step_over => {
                        valid = false;
                        break 'scroll;
                    }
                    _ => {} // half a redraw so far: wait for the rest
                }
            }
        }
        self.wheel(id, false, ups);
        let restored = self.put_back(id, &initial, &mut pace, ups).await;
        (valid && restored).then_some(Capture { rows: history, screen: initial.len(), top })
    }

    /// The probe moved a view someone scrolled up: one notch back, then let it draw.
    async fn undo_probe(&self, id: u32) {
        self.wheel(id, true, 1);
        let _ = self.quiet_page(id, Instant::now() + MAX_PACE * 2).await;
    }

    fn wheel(&self, id: u32, up: bool, notches: usize) {
        let st = self.lock();
        if let (Some(screen), Some(link)) = (st.screens.get(&id), st.link(id)) {
            link.input(id, &screen.wheel(up).repeat(notches));
        }
    }

    fn rows(&self, id: u32) -> Option<Vec<String>> {
        self.lock().screens.get(&id).map(|s| s.screen_rows())
    }

    /// A person pressed a key: stop scrolling (the key goes in after we put the view back).
    fn interrupted(&self, id: u32) -> bool {
        self.lock().gates.get(&id).is_some_and(|g| g.has_held())
    }

    /// The screen once output has gone quiet.
    async fn settled(&self, id: u32, deadline: Instant) -> Option<Vec<String>> {
        loop {
            let quiet = {
                let st = self.lock();
                let sync = st.screens.get(&id).is_some_and(|s| s.dec_mode(2026));
                st.last_output.get(&id).is_none_or(|t| t.elapsed() >= SETTLE) && !sync
            };
            if quiet {
                return self.rows(id);
            }
            if Instant::now() >= deadline || self.interrupted(id) {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The screen after the next redraw (new output, then quiet), or as it is
    /// once the step (from `started`, `pace` long) is over with nothing printed.
    /// Says whether the step is over. The pace stretches to 3x the slowest
    /// redraw seen, so a slow app isn't mistaken for one that didn't move.
    async fn next_page(
        &self,
        id: u32,
        started: Instant,
        pace: &mut Duration,
        deadline: Instant,
        stoppable: bool,
    ) -> Option<(Vec<String>, bool)> {
        let seen = self.lock().output_seq.get(&id).copied();
        let mut measured = false;
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if Instant::now() >= deadline || (stoppable && self.interrupted(id)) {
                return None;
            }
            let (printed, quiet) = {
                let st = self.lock();
                let sync = st.screens.get(&id).is_some_and(|s| s.dec_mode(2026));
                let quiet = st.last_output.get(&id).is_none_or(|t| t.elapsed() >= SETTLE) && !sync;
                (st.output_seq.get(&id).copied() != seen, quiet)
            };
            if printed && !measured {
                measured = true;
                *pace = (*pace).max(started.elapsed() * 3).min(MAX_PACE);
            }
            let over = started.elapsed() >= *pace;
            if (printed && quiet) || (!printed && over) {
                return Some((self.rows(id)?, over));
            }
        }
    }

    /// The screen once a whole `MAX_PACE` passed with nothing printed: every
    /// queued wheel step has been drawn.
    async fn quiet_page(&self, id: u32, deadline: Instant) -> Option<Vec<String>> {
        loop {
            let mut quiet = MAX_PACE;
            if let (rows, true) = self.next_page(id, Instant::now(), &mut quiet, deadline, false).await? {
                return Some(rows);
            }
        }
    }

    /// Scrolls down until the screen matches where it started. A slow app gets
    /// as long as its `steps` queued wheel steps take at its pace.
    async fn put_back(&self, id: u32, initial: &[String], pace: &mut Duration, steps: usize) -> bool {
        let deadline = Instant::now() + RESTORE_MAX.max(*pace * steps as u32 + MAX_PACE);
        loop {
            let Some((rows, over)) = self.next_page(id, Instant::now(), pace, deadline, false).await else {
                return false;
            };
            if similar(&rows, initial) {
                // A slow app may still have wheel events queued: believe it once
                // a whole second passed with nothing printed.
                match self.quiet_page(id, deadline).await {
                    Some(rows) if similar(&rows, initial) => return true,
                    Some(_) => continue,
                    None => return false,
                }
            }
            if over {
                self.wheel(id, false, (initial.len() / 2).max(1));
            }
        }
    }
}

/// Rows as text: trailing space trimmed, trailing blank rows dropped.
pub fn text(rows: &[String]) -> String {
    let mut lines: Vec<&str> = rows.iter().map(|r| r.trim_end()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    //! Full-screen history reconstruction regression tests.
    use super::*;

    const W: usize = 20;

    fn page(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| format!("{l:<W$}")).collect()
    }

    fn ids(rows: &[String]) -> Vec<String> {
        identities(rows)
    }

    #[test]
    fn similar_screens_tolerate_a_small_changing_region() {
        let initial = page(&["line 1", "line 2", "worked for 2s", "prompt"]);
        assert!(similar(&initial, &page(&["line 1", "line 2", "worked for 3s", "prompt"])));
        assert!(!similar(&initial, &page(&["older", "line 1", "line 2", "prompt"])));
    }

    #[test]
    fn a_scroll_up_adds_only_the_new_rows() {
        let previous = page(&["line 3", "line 4", "line 5", "status"]);
        let next = page(&["line 1", "line 2", "line 3", "line 4"]);
        let mut history = previous.clone();
        assert_eq!(merge_up(&mut history, &previous, &next), Merge::Advanced(2));
        assert_eq!(ids(&history), ["line 1", "line 2", "line 3", "line 4", "line 5", "status"]);
    }

    #[test]
    fn a_sticky_header_is_not_repeated() {
        let previous = page(&["sticky", "line 4", "line 5", "line 6", "line 7"]);
        let next = page(&["sticky", "line 2", "line 3", "line 4", "line 5"]);
        let mut history = previous.clone();
        assert_eq!(merge_up(&mut history, &previous, &next), Merge::Advanced(2));
        assert_eq!(ids(&history), ["sticky", "line 2", "line 3", "line 4", "line 5", "line 6", "line 7"]);
    }

    #[test]
    fn text_a_pinned_header_hid_comes_back_on_the_next_page() {
        let pages = [
            page(&["line 4", "line 5", "line 6", "line 7", "line 8"]),
            page(&["pinned", "line 3", "line 4", "line 5", "line 6"]),
            page(&["pinned", "line 1", "line 2", "line 3", "line 4"]),
            page(&["title", "line 0", "line 1", "line 2", "line 3"]),
        ];
        let mut history = pages[0].clone();
        for pair in pages.windows(2) {
            assert!(matches!(merge_up(&mut history, &pair[0], &pair[1]), Merge::Advanced(_)));
        }
        let want = ["title", "line 0", "line 1", "line 2", "line 3", "line 4", "line 5", "line 6", "line 7", "line 8"];
        assert_eq!(ids(&history), want);
    }

    #[test]
    fn repeated_continuation_rows_survive_scrolling() {
        let rows: Vec<String> = (1..=18)
            .flat_map(|n| [format!("line {n:03} begins"), "same continuation".into(), "same ending".into()])
            .map(|l| format!("{l:<W$}"))
            .collect();
        for shift in [3, 15] {
            for offset in 0..3 {
                let previous = rows[offset + shift..offset + shift + 36].to_vec();
                let next = rows[offset..offset + 36].to_vec();
                let mut history = previous.clone();
                assert_eq!(
                    merge_up(&mut history, &previous, &next),
                    Merge::Advanced(shift),
                    "shift {shift} offset {offset}"
                );
                assert_eq!(&history[..shift], &next[..shift]);
                assert_eq!(&history[shift..], &previous[..]);
            }
        }
    }

    /// Ten transcript rows with an optional scrollbar in the last column (thumb
    /// on rows 3..6), then a prompt and a status row.
    fn transcript(first: usize, scrollbar: bool) -> Vec<String> {
        let mut rows: Vec<String> = (0..10)
            .map(|i| {
                let edge = match (scrollbar, (3..6).contains(&i)) {
                    (false, _) => ' ',
                    (true, true) => '┃',
                    (true, false) => TRACK,
                };
                format!("{:<19}{edge}", format!("line {}", first + i))
            })
            .collect();
        rows.extend(page(&["prompt", "status"]));
        rows
    }

    fn set_edge(row: &mut String, at: usize, c: char) {
        *row = row.chars().enumerate().map(|(i, x)| if i == at { c } else { x }).collect();
    }

    #[test]
    fn an_appearing_scrollbar_is_ignored_and_rows_are_kept_as_they_were() {
        let initial = transcript(10, false);
        let scrolled = transcript(7, true);
        let mut history = initial.clone();
        assert_eq!(merge_up(&mut history, &initial, &scrolled), Merge::Advanced(3));
        assert_eq!(&history[..3], &scrolled[..3]);
        assert_eq!(&history[3..], &initial[..]);
        assert!(similar(&initial, &transcript(10, true)));
        assert!(!similar(&initial, &scrolled));

        // The thumb moves on the next page: still aligned.
        let mut older = transcript(4, true);
        for row in &mut older[..10] {
            let swapped = if row.ends_with(TRACK) { '█' } else { TRACK };
            set_edge(row, 19, swapped);
        }
        assert_eq!(merge_up(&mut history, &scrolled, &older), Merge::Advanced(3));
        assert_eq!(&history[..3], &older[..3]);
    }

    #[test]
    fn a_retained_anchor_still_matches_after_the_thumb_moves() {
        let initial = transcript(10, true);
        let mut scrolled = transcript(7, true);
        let mut older = transcript(4, true);
        for row in &mut scrolled[..3] {
            *row = format!("{:<19}{TRACK}", "repeated");
        }
        for row in &mut older[3..6] {
            *row = format!("{:<19}┃", "repeated");
        }
        let mut history = initial.clone();
        assert_eq!(merge_up(&mut history, &initial, &scrolled), Merge::Advanced(3));
        assert_eq!(merge_up(&mut history, &scrolled, &older), Merge::Advanced(3));
        assert_eq!(&history[..6], &older[..6]);
        assert_eq!(&history[6..], &initial[..]);
    }

    #[test]
    fn scrollbar_handling_never_hides_real_text_or_aligns_unrelated_pages() {
        let initial = transcript(10, false);
        let mut changed = transcript(10, true);
        for row in &mut changed[..10] {
            set_edge(row, 18, 'x');
        }
        assert!(!similar(&initial, &changed));
        let mut history = initial.clone();
        assert_eq!(merge_up(&mut history, &initial, &changed), Merge::Unaligned);
        assert_eq!(history, initial);

        let mut real_edge = initial.clone();
        for row in &mut real_edge[..10] {
            set_edge(row, 19, 'x');
        }
        assert!(!similar(&real_edge, &transcript(10, true)));

        // A box border is not a scrollbar: no thumb.
        let boxed = vec!["one   │".to_string(), "two   │".into(), "three │".into(), "four  │".into()];
        assert_eq!(ids(&boxed), ["one   │", "two   │", "three │", "four  │"]);
    }

    #[test]
    fn unchanged_and_unaligned_pages_leave_history_alone() {
        let previous = page(&["line 1", "line 2", "line 3"]);
        let mut history = previous.clone();
        assert_eq!(merge_up(&mut history, &previous, &previous), Merge::Unchanged);
        assert_eq!(merge_up(&mut history, &previous, &page(&["other a", "other b", "other c"])), Merge::Unaligned);
        assert_eq!(history, previous);
    }

    #[test]
    fn at_the_top_a_changing_status_row_still_counts_as_unchanged() {
        let previous = page(&["line 1", "line 2", "line 3", "line 4", "wheels=3"]);
        let next = page(&["line 1", "line 2", "line 3", "line 4", "wheels=6"]);
        let mut history = previous.clone();
        assert_eq!(merge_up(&mut history, &previous, &next), Merge::Unchanged);
        assert_eq!(history, previous);
    }

    #[test]
    fn competing_unique_anchors_do_not_prove_a_distance() {
        let previous = page(&["U1", "U2", "U3", "c", "d"]);
        let mut history = previous.clone();
        assert_eq!(merge_up(&mut history, &previous, &page(&["a", "b", "U2", "U3", "U1"])), Merge::Unaligned);
        assert_eq!(history, previous);
    }

    #[test]
    fn repeated_rows_alone_do_not_prove_a_distance() {
        let previous = page(&["a", "b", "a", "b", "a", "b"]);
        let mut history = previous.clone();
        assert_eq!(merge_up(&mut history, &previous, &page(&["b", "a", "b", "a", "b", "a"])), Merge::Unaligned);
        assert_eq!(history, previous);
    }
}
