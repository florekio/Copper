//! Editable address bar — buffer + cursor + selection + word/line helpers.
//!
//! Stores text as `Vec<char>` so cursor and selection use simple indices.
//! URLs are mostly ASCII so the cost of glyph-by-glyph indexing is irrelevant.

#[derive(Debug, Default, Clone)]
pub struct AddressInput {
    pub focused: bool,
    pub text: Vec<char>,
    /// Caret position as a char index in `text`. Always in `0..=text.len()`.
    pub cursor: usize,
    /// When `Some`, marks the *other* end of a selection running between
    /// `selection_anchor` and `cursor`. The visible range is the
    /// (min, max) pair.
    pub selection_anchor: Option<usize>,
}

impl AddressInput {
    pub fn focus_with(&mut self, url: &str) {
        self.text = url.chars().collect();
        self.cursor = self.text.len();
        self.selection_anchor = Some(0); // matches Chrome's "select all on focus"
        self.focused = true;
    }

    pub fn blur(&mut self) {
        self.focused = false;
        self.selection_anchor = None;
    }

    pub fn text_string(&self) -> String {
        self.text.iter().collect()
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection_anchor.map(|a| {
            if a < self.cursor {
                (a, self.cursor)
            } else if a > self.cursor {
                (self.cursor, a)
            } else {
                // anchor == cursor → no real selection
                (a, a)
            }
        })
    }

    pub fn has_selection(&self) -> bool {
        matches!(self.selection_range(), Some((a, b)) if a < b)
    }

    pub fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection_range()?;
        if a >= b {
            return None;
        }
        Some(self.text[a..b].iter().collect())
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection_range() else { return false };
        if a >= b {
            self.selection_anchor = None;
            return false;
        }
        self.text.drain(a..b);
        self.cursor = a;
        self.selection_anchor = None;
        true
    }

    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        for c in s.chars() {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        self.text.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    /// Move cursor by `step` (positive = right). When `extending`, the
    /// anchor stays put; otherwise the selection collapses.
    pub fn move_by(&mut self, step: Step, extending: bool) {
        let new_cursor = match step {
            Step::Char(d) => signed_add_clamp(self.cursor, d, self.text.len()),
            Step::Word(d) => {
                if d > 0 {
                    next_word_boundary(&self.text, self.cursor)
                } else {
                    prev_word_boundary(&self.text, self.cursor)
                }
            }
            Step::LineStart => 0,
            Step::LineEnd => self.text.len(),
        };
        if extending {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = new_cursor;
    }

    pub fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Select the word at `pos` (clamped). Returns `true` if a non-empty
    /// selection was made.
    pub fn select_word_at(&mut self, pos: usize) -> bool {
        let pos = pos.min(self.text.len());
        // If pos is on a non-word char, select just that one character.
        if pos < self.text.len() && !is_word_char(self.text[pos]) {
            self.selection_anchor = Some(pos);
            self.cursor = pos + 1;
            return true;
        }
        let mut start = pos;
        while start > 0 && is_word_char(self.text[start - 1]) {
            start -= 1;
        }
        let mut end = pos;
        while end < self.text.len() && is_word_char(self.text[end]) {
            end += 1;
        }
        if start == end {
            return false;
        }
        self.selection_anchor = Some(start);
        self.cursor = end;
        true
    }

    pub fn place_cursor(&mut self, pos: usize) {
        self.cursor = pos.min(self.text.len());
        self.selection_anchor = None;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Step {
    Char(i32),
    Word(i32),
    LineStart,
    LineEnd,
}

fn signed_add_clamp(value: usize, delta: i32, max: usize) -> usize {
    if delta < 0 {
        value.saturating_sub((-delta) as usize)
    } else {
        (value + delta as usize).min(max)
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn prev_word_boundary(text: &[char], cursor: usize) -> usize {
    let mut i = cursor;
    while i > 0 && !is_word_char(text[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_word_char(text[i - 1]) {
        i -= 1;
    }
    i
}

fn next_word_boundary(text: &[char], cursor: usize) -> usize {
    let mut i = cursor;
    while i < text.len() && !is_word_char(text[i]) {
        i += 1;
    }
    while i < text.len() && is_word_char(text[i]) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(input: &str) -> AddressInput {
        let mut a = AddressInput::default();
        a.focus_with(input);
        a
    }

    #[test]
    fn focus_selects_all() {
        let a = s("hello");
        assert_eq!(a.cursor, 5);
        assert_eq!(a.selection_range(), Some((0, 5)));
        assert_eq!(a.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn typing_replaces_selection() {
        let mut a = s("abc");
        a.insert_char('X');
        assert_eq!(a.text_string(), "X");
        assert_eq!(a.cursor, 1);
        assert!(a.selection_anchor.is_none());
    }

    #[test]
    fn backspace_handles_both_modes() {
        let mut a = s("abcdef");
        a.move_by(Step::LineEnd, false);
        a.backspace();
        assert_eq!(a.text_string(), "abcde");
        // Now selection: select last 2 chars and backspace removes them.
        a.selection_anchor = Some(3);
        a.cursor = 5;
        a.backspace();
        assert_eq!(a.text_string(), "abc");
        assert_eq!(a.cursor, 3);
    }

    #[test]
    fn delete_forward_at_eof_is_noop() {
        let mut a = s("abc");
        a.move_by(Step::LineEnd, false);
        a.delete_forward();
        assert_eq!(a.text_string(), "abc");
    }

    #[test]
    fn arrows_clear_or_extend_selection() {
        let mut a = s("abcdef");
        // Selection covers all (focus default). Pressing Right collapses to end.
        a.move_by(Step::Char(1), false);
        assert_eq!(a.cursor, 6);
        assert!(a.selection_anchor.is_none());
        // Shift+Left starts a new selection.
        a.move_by(Step::Char(-1), true);
        assert_eq!(a.cursor, 5);
        assert_eq!(a.selection_range(), Some((5, 6)));
    }

    #[test]
    fn word_motion_jumps_correctly() {
        let mut a = s("https://example.com/path");
        // Cursor at end (focus default). Move one word left.
        a.selection_anchor = None; // collapse focus's "select all"
        a.cursor = a.text.len();
        a.move_by(Step::Word(-1), false);
        assert_eq!(a.cursor, "https://example.com/".len());
        a.move_by(Step::Word(-1), false);
        assert_eq!(a.cursor, "https://example.".len());
    }

    #[test]
    fn select_word_at_word_char() {
        let mut a = s("hello world");
        a.selection_anchor = None;
        let hit = a.select_word_at(2); // middle of "hello"
        assert!(hit);
        assert_eq!(a.selection_range(), Some((0, 5)));
        assert_eq!(a.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn select_word_at_non_word_char() {
        let mut a = s("a b c");
        a.selection_anchor = None;
        a.select_word_at(1); // on the space
        assert_eq!(a.selected_text().as_deref(), Some(" "));
    }

    #[test]
    fn line_jumps() {
        let mut a = s("https://example.com");
        a.selection_anchor = None;
        a.cursor = 10;
        a.move_by(Step::LineStart, false);
        assert_eq!(a.cursor, 0);
        a.move_by(Step::LineEnd, false);
        assert_eq!(a.cursor, a.text.len());
    }

    #[test]
    fn paste_replaces_selection() {
        let mut a = s("abc"); // selection covers all
        a.insert_str("xy");
        assert_eq!(a.text_string(), "xy");
        assert_eq!(a.cursor, 2);
    }

    #[test]
    fn place_cursor_clamps_and_clears_selection() {
        let mut a = s("abc");
        a.place_cursor(99);
        assert_eq!(a.cursor, 3);
        assert!(a.selection_anchor.is_none());
    }
}
