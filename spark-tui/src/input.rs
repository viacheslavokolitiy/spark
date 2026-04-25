//! Editable single-line and multi-line text input state.

/// A simple text input that supports both single-line and multi-line editing.
#[derive(Debug)]
pub struct TextInput {
    /// Lines of text content.
    pub lines: Vec<String>,
    /// Current cursor row (line index).
    pub cursor_row: usize,
    /// Current cursor column (character index within the current line).
    pub cursor_col: usize,
    /// Whether newlines may be inserted.
    pub multiline: bool,
}

impl Default for TextInput {
    fn default() -> Self {
        Self::single_line()
    }
}

impl TextInput {
    /// Creates a single-line input (Tab and Enter are not consumed for newlines).
    pub fn single_line() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            multiline: false,
        }
    }

    /// Creates a multi-line input where Enter inserts a newline.
    pub fn multi_line() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            multiline: true,
        }
    }

    /// Returns the full text content with lines joined by `\n`.
    pub fn content(&self) -> String {
        self.lines.join("\n")
    }

    /// Replaces content, placing the cursor at the end.
    pub fn set_content(&mut self, text: &str) {
        self.lines = text.split('\n').map(String::from).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    /// Inserts a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        let byte_idx = char_to_byte_idx(&self.lines[self.cursor_row], self.cursor_col);
        self.lines[self.cursor_row].insert(byte_idx, c);
        self.cursor_col += 1;
    }

    /// Inserts a newline at the cursor (only has effect in multi-line mode).
    pub fn insert_newline(&mut self) {
        if !self.multiline {
            return;
        }
        let byte_idx = char_to_byte_idx(&self.lines[self.cursor_row], self.cursor_col);
        let remainder = self.lines[self.cursor_row][byte_idx..].to_string();
        self.lines[self.cursor_row].truncate(byte_idx);
        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, remainder);
        self.cursor_col = 0;
    }

    /// Deletes the character immediately before the cursor (backspace behaviour).
    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let byte_end = char_to_byte_idx(line, self.cursor_col);
            let byte_start = char_to_byte_idx(line, self.cursor_col - 1);
            line.drain(byte_start..byte_end);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 && self.multiline {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            let prev_char_len = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current);
            self.cursor_col = prev_char_len;
        }
    }

    /// Moves the cursor one character to the left.
    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 && self.multiline {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    /// Moves the cursor one character to the right.
    pub fn move_right(&mut self) {
        let line_len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.multiline && self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    /// Moves the cursor one line up.
    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_col();
        }
    }

    /// Moves the cursor one line down.
    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.clamp_col();
        }
    }

    /// Moves the cursor to the start of the current line.
    pub fn move_to_line_start(&mut self) {
        self.cursor_col = 0;
    }

    /// Moves the cursor to the end of the current line.
    pub fn move_to_line_end(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].chars().count();
    }

    /// Clamps the cursor column to the length of the current line.
    fn clamp_col(&mut self) {
        let line_len = self.lines[self.cursor_row].chars().count();
        self.cursor_col = self.cursor_col.min(line_len);
    }
}

/// Converts a character index to the corresponding byte offset in `s`.
fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(b, _)| b)
}
