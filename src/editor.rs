//! The text input widget: the text, the caret, and the selection anchor.
//!
//! Pure logic. Nothing here knows about Wayland, the clipboard, or the results
//! list; the client translates key presses into these calls and takes the
//! answers back. That is what makes the editing rules testable, and they need
//! to be: cursor and anchor interact in ways that are easy to get subtly wrong
//! and hard to notice.
//!
//! Positions are char offsets, not byte offsets. Every public boundary speaks
//! chars, because that is what a caret is; bytes appear only where the text is
//! actually indexed.

use crate::platform::arena::{ArrayString, ArrayVec};

/// Capacity of the search input field.
pub const INPUT_CAP: usize = 1024;

/// The text being edited, the caret, and where a selection started.
///
/// An anchor equal to the cursor is not a selection: Shift+Left then
/// Shift+Right lands exactly there, and treating it as one selects a character
/// the user never asked for.
#[derive(Default)]
pub struct Editor {
    text: ArrayString<INPUT_CAP>,
    cursor: usize,
    anchor: Option<usize>,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    /// A copy of the text. The caret and the results both need it while the
    /// editor itself is borrowed, and the text is small and Copy.
    pub fn text_owned(&self) -> ArrayString<INPUT_CAP> {
        self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// How many chars the text holds. The caret can sit at any of them, or one
    /// past the last.
    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// The selection as (start, end) char offsets, or None when there is none.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<ArrayString<INPUT_CAP>> {
        let (start, end) = self.selection()?;
        let mut out: ArrayString<INPUT_CAP> = ArrayString::new();
        out.push_str(&self.text.as_str()[self.byte(start)..self.byte(end)])
            .ok()?;
        Some(out)
    }

    /// Clear everything.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = None;
    }

    /// Type a character, replacing the selection. Ignored when the text is full:
    /// advancing the caret past text that was never inserted desyncs it, and
    /// every later edit lands in the wrong place.
    pub fn insert(&mut self, ch: char) -> bool {
        self.delete_selection();
        let at = self.byte(self.cursor);
        let inserted = self.text.insert(at, ch).is_ok();
        if inserted {
            self.cursor += 1;
        }
        self.anchor = None;
        inserted
    }

    /// Replace the selection with s, or insert it at the caret when there is no
    /// selection.
    ///
    /// Nothing changes when the result would not fit. Deleting the selection
    /// first and then failing to insert would leave the user with neither their
    /// own text nor the pasted text.
    pub fn paste(&mut self, s: &str) -> bool {
        let selected_bytes = match self.selection() {
            Some((start, end)) => self.byte(end) - self.byte(start),
            None => 0,
        };
        if self.text.len() - selected_bytes + s.len() > INPUT_CAP {
            return false;
        }
        self.delete_selection();
        let at = self.byte(self.cursor);
        if self.text.insert_str(at, s).is_err() {
            return false;
        }
        self.cursor += s.chars().count();
        self.anchor = None;
        true
    }

    /// Delete the selection, or the char before the caret.
    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        // The anchor survives an edit it took no part in, and once the delete
        // moves the caret off it, it reads as a one-character selection the next
        // keystroke would eat.
        self.anchor = None;
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = self.byte(self.cursor);
            self.text.remove(at);
        }
    }

    /// Delete the selection, or the char after the caret.
    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        self.anchor = None;
        if self.cursor < self.len() {
            let at = self.byte(self.cursor);
            self.text.remove(at);
        }
    }

    /// Delete the selection and put the caret where it started. False when there
    /// was nothing selected.
    pub fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        // The offsets come from the text's own char boundaries, so this only
        // fails if that mapping is wrong. Leave the text and the caret alone
        // rather than move the caret to a position the text does not have.
        if self
            .text
            .delete_range(self.byte(start), self.byte(end))
            .is_err()
        {
            return false;
        }
        self.cursor = start;
        self.anchor = None;
        true
    }

    /// Move the caret one char left. With shift, extend the selection; without
    /// it, a selection collapses to its start rather than moving the caret.
    pub fn left(&mut self, shift: bool) {
        if shift {
            self.anchor.get_or_insert(self.cursor);
        } else if let Some((start, _)) = self.selection() {
            self.cursor = start;
            self.anchor = None;
            return;
        } else {
            self.anchor = None;
        }
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the caret one char right, or collapse a selection to its end.
    pub fn right(&mut self, shift: bool) {
        if shift {
            self.anchor.get_or_insert(self.cursor);
        } else if let Some((_, end)) = self.selection() {
            self.cursor = end;
            self.anchor = None;
            return;
        } else {
            self.anchor = None;
        }
        if self.cursor < self.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self, shift: bool) {
        if shift {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = 0;
    }

    pub fn end(&mut self, shift: bool) {
        if shift {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = self.len();
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.len();
    }

    /// A click at a char offset. One click places the caret, two select the word
    /// under it, three or more select everything.
    pub fn click(&mut self, at: usize, count: u32) {
        match count {
            2 => {
                let (start, end) = self.word_at(at);
                self.anchor = Some(start);
                self.cursor = end;
            }
            n if n >= 3 => self.select_all(),
            _ => {
                self.cursor = at.min(self.len());
                self.anchor = None;
            }
        }
    }

    /// A drag to a char offset, extending from wherever the drag began.
    pub fn drag(&mut self, to: usize) {
        self.anchor.get_or_insert(self.cursor);
        self.cursor = to.min(self.len());
    }

    /// The word around a char offset, as (start, end) char offsets. A click in
    /// the run of separators between two words selects that run, which is what
    /// every other text field does.
    fn word_at(&self, at: usize) -> (usize, usize) {
        let mut chars: ArrayVec<char, INPUT_CAP> = ArrayVec::new();
        for c in self.text.chars() {
            if chars.push(c).is_err() {
                break;
            }
        }
        let len = chars.len();
        if len == 0 {
            return (0, 0);
        }
        let pos = at.min(len - 1);
        let is_word = |c: char| c.is_alphanumeric() || c == '_';

        // Grow both ways over chars of the same kind as the one clicked.
        let want = is_word(chars[pos]);
        let mut start = pos;
        while start > 0 && is_word(chars[start - 1]) == want {
            start -= 1;
        }
        let mut end = pos;
        while end < len && is_word(chars[end]) == want {
            end += 1;
        }
        (start, end)
    }

    /// The byte offset of a char offset. Past the end maps to the end, so a
    /// caret one past the last char indexes the text's length.
    fn byte(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        let mut e = Editor::new();
        for c in text.chars() {
            assert!(e.insert(c), "insert {c}");
        }
        e
    }

    #[test]
    fn typing_appends_and_moves_the_caret() {
        let e = editor("abc");
        assert_eq!(e.text(), "abc");
        assert_eq!(e.cursor(), 3);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn a_full_input_refuses_the_character_and_leaves_the_caret_alone() {
        let mut e = Editor::new();
        for _ in 0..INPUT_CAP {
            assert!(e.insert('x'));
        }
        assert!(!e.insert('y'), "the text is full");
        assert_eq!(e.cursor(), INPUT_CAP, "the caret did not run past the text");
        assert_eq!(e.len(), INPUT_CAP);
        // The caret is still where the text ends, so a backspace still bites.
        e.backspace();
        assert_eq!(e.len(), INPUT_CAP - 1);
    }

    /// Shift+Left then Shift+Right leaves the anchor sitting on the cursor,
    /// which is not a selection. If backspace left it standing, the caret it
    /// then moves would be one char away from it, and that reads as a
    /// one-character selection nobody made: the next keystroke would replace a
    /// character the user never selected.
    #[test]
    fn backspace_clears_an_anchor_that_is_not_a_selection() {
        let mut e = editor("abcd");
        e.left(true); // Shift+Left: anchor at 4, cursor 3
        e.right(true); // Shift+Right: cursor back to 4, on the anchor
        assert_eq!(
            e.selection(),
            None,
            "an anchor on the cursor is no selection"
        );

        e.backspace(); // deletes 'd'
        assert_eq!(e.text(), "abc");
        assert_eq!(e.selection(), None, "no phantom selection is left behind");

        e.insert('x');
        assert_eq!(e.text(), "abcx", "typing did not eat a second character");
    }

    #[test]
    fn delete_clears_an_anchor_that_is_not_a_selection() {
        let mut e = editor("abcd");
        e.home(false);
        e.right(true);
        e.left(true); // anchor and cursor both at 0
        e.delete(); // deletes 'a'
        assert_eq!(e.text(), "bcd");
        e.insert('x');
        assert_eq!(e.text(), "xbcd");
    }

    #[test]
    fn backspace_and_delete_take_the_selection_when_there_is_one() {
        let mut e = editor("abcd");
        e.home(false);
        e.right(true);
        e.right(true); // "ab" selected
        e.backspace();
        assert_eq!(e.text(), "cd");
        assert_eq!(e.cursor(), 0);

        let mut e = editor("abcd");
        e.select_all();
        e.delete();
        assert!(e.text().is_empty());
    }

    #[test]
    fn a_multibyte_caret_moves_by_character_not_by_byte() {
        let mut e = editor("aéb");
        assert_eq!(e.cursor(), 3);
        e.left(false);
        e.backspace(); // deletes the 'é', which is two bytes
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor(), 1);
        e.insert('é');
        assert_eq!(e.text(), "aéb");
    }

    #[test]
    fn selection_extends_and_collapses() {
        let mut e = editor("hello");
        e.home(false);
        e.right(true);
        e.right(true);
        assert_eq!(e.selection(), Some((0, 2)));
        assert_eq!(e.selected_text().expect("selected").as_str(), "he");

        // Left without shift collapses to the start rather than stepping back.
        e.left(false);
        assert_eq!(e.selection(), None);
        assert_eq!(e.cursor(), 0);

        e.end(true);
        assert_eq!(e.selection(), Some((0, 5)));
        // Right without shift collapses to the end.
        e.right(false);
        assert_eq!(e.cursor(), 5);
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut e = editor("abcd");
        e.select_all();
        e.insert('x');
        assert_eq!(e.text(), "x");
        assert_eq!(e.cursor(), 1);
    }

    #[test]
    fn paste_replaces_the_selection() {
        let mut e = editor("abcd");
        e.home(false);
        e.right(true);
        e.right(true);
        assert!(e.paste("XY"));
        assert_eq!(e.text(), "XYcd");
        assert_eq!(e.cursor(), 2);
    }

    /// A paste is all or nothing. Deleting the selection first and then finding
    /// the text does not fit would leave the user with neither what they had nor
    /// what they pasted.
    #[test]
    fn a_paste_that_does_not_fit_changes_nothing() {
        let mut e = editor("abc");
        e.select_all();
        let huge = "x".repeat(INPUT_CAP + 1);
        assert!(!e.paste(&huge));
        assert_eq!(e.text(), "abc", "the selection survives");
        assert_eq!(e.selection(), Some((0, 3)), "and is still selected");
    }

    #[test]
    fn a_paste_that_exactly_fills_the_input_is_accepted() {
        let mut e = editor("ab");
        e.select_all();
        let exact = "x".repeat(INPUT_CAP);
        assert!(e.paste(&exact));
        assert_eq!(e.len(), INPUT_CAP);
    }

    #[test]
    fn a_double_click_selects_the_word_under_it() {
        let mut e = editor("hello wide world");
        e.click(7, 2); // inside "wide"
        assert_eq!(e.selection(), Some((6, 10)));
        assert_eq!(e.selected_text().expect("selected").as_str(), "wide");
    }

    #[test]
    fn a_triple_click_selects_everything() {
        let mut e = editor("hello world");
        e.click(3, 3);
        assert_eq!(e.selection(), Some((0, 11)));
    }

    #[test]
    fn a_single_click_places_the_caret_and_clears_the_selection() {
        let mut e = editor("hello");
        e.select_all();
        e.click(2, 1);
        assert_eq!(e.cursor(), 2);
        assert_eq!(e.selection(), None);
        // A click past the end lands at the end.
        e.click(99, 1);
        assert_eq!(e.cursor(), 5);
    }

    #[test]
    fn a_drag_extends_from_where_it_began() {
        let mut e = editor("hello");
        e.click(1, 1);
        e.drag(4);
        assert_eq!(e.selection(), Some((1, 4)));
        assert_eq!(e.selected_text().expect("selected").as_str(), "ell");
        // Dragging back across the start flips the selection's direction.
        e.drag(0);
        assert_eq!(e.selection(), Some((0, 1)));
    }

    #[test]
    fn escape_style_clear_resets_everything() {
        let mut e = editor("abc");
        e.select_all();
        e.clear();
        assert!(e.text().is_empty());
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.selection(), None);
    }
}
