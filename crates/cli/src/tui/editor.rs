//! Minimal multi-line text editor for comment input overlays.
//!
//! Deliberately small: insert, backspace/delete, newline, arrow/home/end
//! movement, and a char-accurate cursor. No undo, no selections; comment
//! bodies are short.

#[derive(Debug, Clone)]
pub struct Editor {
    /// Lines of text (always at least one).
    lines: Vec<String>,
    /// Cursor row (line index).
    row: usize,
    /// Cursor column in CHARS (not bytes).
    col: usize,
    /// Single-line mode: Enter is handled by the caller (submit) instead of
    /// inserting a newline.
    single_line: bool,
}

impl Editor {
    pub fn new(single_line: bool) -> Self {
        Editor {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            single_line,
        }
    }

    pub fn with_text(text: &str, single_line: bool) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let row = lines.len() - 1;
        let col = lines[row].chars().count();
        Editor {
            lines,
            row,
            col,
            single_line,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn is_single_line(&self) -> bool {
        self.single_line
    }

    fn byte_col(&self) -> usize {
        let line = &self.lines[self.row];
        line.char_indices()
            .nth(self.col)
            .map(|(i, _)| i)
            .unwrap_or(line.len())
    }

    fn clamp_col(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col > len {
            self.col = len;
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let byte = self.byte_col();
        self.lines[self.row].insert(byte, c);
        self.col += 1;
    }

    #[cfg_attr(not(test), allow(dead_code))] // paste support will want this
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.newline();
            } else {
                self.insert_char(c);
            }
        }
    }

    pub fn newline(&mut self) {
        if self.single_line {
            return;
        }
        let byte = self.byte_col();
        let rest = self.lines[self.row].split_off(byte);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let line = &mut self.lines[self.row];
            let byte = line
                .char_indices()
                .nth(self.col - 1)
                .map(|(i, _)| i)
                .unwrap_or(0);
            line.remove(byte);
            self.col -= 1;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
            self.lines[self.row].push_str(&current);
        }
    }

    pub fn delete(&mut self) {
        let len = self.lines[self.row].chars().count();
        if self.col < len {
            let byte = self.byte_col();
            self.lines[self.row].remove(byte);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].chars().count();
        }
    }

    pub fn right(&mut self) {
        if self.col < self.lines[self.row].chars().count() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.clamp_col();
        }
    }

    pub fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.clamp_col();
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].chars().count();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_newlines() {
        let mut e = Editor::new(false);
        e.insert_str("hello");
        e.newline();
        e.insert_str("world");
        assert_eq!(e.text(), "hello\nworld");
        assert_eq!(e.cursor(), (1, 5));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut e = Editor::with_text("ab\ncd", false);
        e.up();
        e.home();
        e.down();
        // cursor at start of "cd"
        assert_eq!(e.cursor(), (1, 0));
        e.backspace();
        assert_eq!(e.text(), "abcd");
        assert_eq!(e.cursor(), (0, 2));
    }

    #[test]
    fn multibyte_chars_edit_correctly() {
        let mut e = Editor::new(false);
        e.insert_str("caf\u{e9}\u{4f60}");
        e.backspace();
        assert_eq!(e.text(), "caf\u{e9}");
        e.left();
        e.insert_char('X');
        assert_eq!(e.text(), "cafX\u{e9}");
    }

    #[test]
    fn single_line_mode_ignores_newline() {
        let mut e = Editor::new(true);
        e.insert_str("one");
        e.newline();
        e.insert_str(" two");
        assert_eq!(e.text(), "one two");
    }

    #[test]
    fn delete_at_line_end_joins_next() {
        let mut e = Editor::with_text("ab\ncd", false);
        e.up();
        e.end();
        e.delete();
        assert_eq!(e.text(), "abcd");
    }
}
