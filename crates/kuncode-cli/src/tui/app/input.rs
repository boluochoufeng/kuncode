//! Cursor-safe editing for the TUI input buffer.

use super::App;

impl App {
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Deletes the char before the cursor (Backspace). No-op at the start.
    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.input.remove(prev);
            self.cursor = prev;
        }
    }

    /// Deletes the char at the cursor (Delete). No-op at the end.
    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.cursor = prev;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.cursor = next;
        }
    }

    /// Moves to the start of the current logical line (after the preceding `\n`).
    pub fn move_home(&mut self) {
        self.cursor = self.current_line().0;
    }

    /// Moves to the end of the current logical line (before the next `\n`).
    pub fn move_end(&mut self) {
        self.cursor = self.current_line().1;
    }

    /// Moves to the previous logical line, keeping the column. No-op on the first
    /// line; a shorter target line clamps the cursor to its end.
    pub fn move_up(&mut self) {
        let (start, _) = self.current_line();
        if start == 0 {
            return;
        }
        let col = self.input[start..self.cursor].chars().count();
        let prev_end = start - 1;
        let prev_start = self.input[..prev_end].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = self.byte_at_column(prev_start, prev_end, col);
    }

    /// Moves to the next logical line, keeping the column. No-op on the last line.
    pub fn move_down(&mut self) {
        let (start, end) = self.current_line();
        if end == self.input.len() {
            return;
        }
        let col = self.input[start..self.cursor].chars().count();
        let next_start = end + 1;
        let next_end = self.input[next_start..]
            .find('\n')
            .map_or(self.input.len(), |rel| next_start + rel);
        self.cursor = self.byte_at_column(next_start, next_end, col);
    }

    /// Takes the current input, leaving the box empty and the cursor at the start.
    pub fn take_input(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.input[..self.cursor]
            .chars()
            .next_back()
            .map(|c| self.cursor - c.len_utf8())
    }

    fn next_boundary(&self) -> Option<usize> {
        self.input[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
    }

    fn current_line(&self) -> (usize, usize) {
        let start = self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let end = self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |rel| self.cursor + rel);
        (start, end)
    }

    fn byte_at_column(&self, start: usize, end: usize, col: usize) -> usize {
        self.input[start..end]
            .char_indices()
            .nth(col)
            .map_or(end, |(off, _)| start + off)
    }
}

#[cfg(test)]
mod tests {
    use kuncode_agent::permission::PermissionMode;

    use super::App;

    #[test]
    fn input_edits_then_take_clears() {
        let mut app = app();
        app.insert_char('h');
        app.insert_char('i');
        app.insert_newline();
        app.insert_char('x');
        app.backspace();
        assert_eq!(app.input, "hi\n");
        assert_eq!(app.take_input(), "hi\n");
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0, "take_input resets the cursor");
    }

    fn app() -> App {
        App::new("model", PermissionMode::Default)
    }

    fn typed(text: &str) -> App {
        let mut app = app();
        for c in text.chars() {
            app.insert_char(c);
        }
        app
    }

    #[test]
    fn insert_and_delete_happen_at_the_cursor() {
        let mut app = typed("helo");
        app.move_left();
        app.move_left();
        app.insert_char('l');
        assert_eq!(app.input, "hello");

        app.move_home();
        app.delete();
        assert_eq!(app.input, "ello");
        assert_eq!(app.cursor, 0);

        app.move_end();
        app.backspace();
        assert_eq!(app.input, "ell");
    }

    #[test]
    fn movement_stops_at_bounds_and_respects_utf8() {
        let mut app = app();
        app.move_left();
        app.delete();
        let mut app = typed("你好");
        app.move_left();
        assert_eq!(app.cursor, 3, "left lands on a char boundary, not mid-byte");
        app.move_right();
        app.move_right();
        assert_eq!(app.cursor, 6);
        app.backspace();
        assert_eq!(app.input, "你");
    }

    #[test]
    fn home_end_act_on_the_current_logical_line() {
        let mut app = typed("ab\ncd");
        app.move_home();
        assert_eq!(app.cursor, 3, "home → start of the current line");
        app.insert_char('Z');
        assert_eq!(app.input, "ab\nZcd");
        app.move_end();
        assert_eq!(app.cursor, app.input.len(), "end → end of the current line");
    }

    #[test]
    fn up_down_move_by_logical_line_clamping_the_column() {
        let mut app = typed("abcd\nxy\nwxyz");
        app.move_up();
        assert_eq!(&app.input[..app.cursor], "abcd\nxy");
        app.move_up();
        assert_eq!(&app.input[..app.cursor], "ab");
        app.move_down();
        assert_eq!(&app.input[..app.cursor], "abcd\nxy");
        app.move_up();
        app.move_up();
        assert_eq!(&app.input[..app.cursor], "ab");
    }
}
