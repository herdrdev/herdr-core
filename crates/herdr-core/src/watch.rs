//! `run --watch`: the lines a program prints that contain watched text.

/// Longest unfinished line kept while it waits for its end.
const LINE_MAX: usize = 4096;

/// A terminal's output so far, cut into lines as a person would read them.
#[derive(Default)]
pub struct Lines {
    held: Vec<u8>,
    line: String,
}

impl Lines {
    /// Finished lines in this output that contain any of `watch`, escape
    /// sequences stripped; an unfinished line waits for the rest.
    pub fn feed(&mut self, watch: &[String], bytes: &[u8]) -> Vec<String> {
        self.line.push_str(&crate::wait::text_of(&mut self.held, bytes));
        let mut found = Vec::new();
        while let Some(end) = self.line.find('\n') {
            let line: String = self.line.drain(..=end).collect();
            let line = line.trim_end();
            if watch.iter().any(|w| line.contains(w.as_str())) {
                found.push(line.to_string());
            }
        }
        if self.line.len() > LINE_MAX {
            let cut = self.line.len() - LINE_MAX / 2;
            let cut = (cut..self.line.len()).find(|i| self.line.is_char_boundary(*i)).unwrap_or(0);
            self.line.drain(..cut);
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_lines_with_watched_text_however_the_output_is_cut() {
        let watch = ["ready".to_string(), "ERR".to_string()];
        let out = "booting\r\nready on :3000 ✓\r\nERR boom\nnot ready yet".as_bytes();
        for cut in 0..=out.len() {
            let mut lines = Lines::default();
            let mut found = lines.feed(&watch, &out[..cut]);
            found.extend(lines.feed(&watch, &out[cut..]));
            assert_eq!(found, ["ready on :3000 ✓", "ERR boom"], "cut at byte {cut}");
        }
        let mut lines = Lines::default();
        lines.feed(&watch, b"not ready yet");
        assert_eq!(lines.feed(&watch, b"\n"), ["not ready yet"], "a line counts once it ends");
        // The relay hands over whole escape sequences.
        assert_eq!(lines.feed(&watch, b"\x1b[32mrea\x1b[0mdy\n"), ["ready"]);
    }
}
