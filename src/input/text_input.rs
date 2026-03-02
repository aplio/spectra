fn char_len(text: &str) -> usize {
    text.chars().count()
}

pub fn byte_index_from_char(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
}

pub fn delete_prev_word_input(input: &mut String) {
    let cursor = char_len(input);
    let chars: Vec<char> = input.chars().collect();
    let mut start = cursor;
    while start > 0 && chars[start - 1].is_whitespace() {
        start -= 1;
    }
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    if start < cursor {
        let start_byte = byte_index_from_char(input, start);
        let end_byte = byte_index_from_char(input, cursor);
        input.replace_range(start_byte..end_byte, "");
    }
}

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    pub cursor: usize,
    pub min_cursor: usize,
}

impl TextInput {
    pub fn new(text: String, min_cursor: usize) -> Self {
        let cursor = char_len(&text);
        Self {
            text,
            cursor,
            min_cursor,
        }
    }

    pub fn with_text(text: &str) -> Self {
        let cursor = char_len(text);
        Self {
            text: text.to_string(),
            cursor,
            min_cursor: 0,
        }
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = char_len(&self.text);
    }

    pub fn clear(&mut self) {
        self.text
            .truncate(byte_index_from_char(&self.text, self.min_cursor));
        self.cursor = self.min_cursor;
    }

    pub fn char_len(&self) -> usize {
        char_len(&self.text)
    }

    pub fn byte_index_at_cursor(&self) -> usize {
        byte_index_from_char(&self.text, self.cursor)
    }

    pub fn clamp(&mut self) {
        let max = char_len(&self.text);
        let min = self.min_cursor.min(max);
        self.cursor = self.cursor.min(max).max(min);
    }

    pub fn move_left(&mut self) {
        self.clamp();
        if self.cursor > self.min_cursor {
            self.cursor -= 1;
        }
    }

    pub fn move_right(&mut self) {
        self.clamp();
        let max = char_len(&self.text);
        if self.cursor < max {
            self.cursor += 1;
        }
    }

    pub fn move_word_left(&mut self) {
        self.clamp();
        if self.cursor <= self.min_cursor {
            return;
        }

        let chars: Vec<char> = self.text.chars().collect();
        let mut pos = self.cursor;
        while pos > self.min_cursor && chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        while pos > self.min_cursor && !chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        self.cursor = pos;
    }

    pub fn move_word_right(&mut self) {
        self.clamp();
        let max = char_len(&self.text);
        if self.cursor >= max {
            return;
        }

        let chars: Vec<char> = self.text.chars().collect();
        let mut pos = self.cursor;
        while pos < max && !chars[pos].is_whitespace() {
            pos += 1;
        }
        while pos < max && chars[pos].is_whitespace() {
            pos += 1;
        }
        self.cursor = pos;
    }

    pub fn move_start(&mut self) {
        self.cursor = self.min_cursor;
    }

    pub fn move_end(&mut self) {
        self.cursor = char_len(&self.text);
    }

    pub fn insert_char(&mut self, c: char) {
        self.clamp();
        let byte_pos = byte_index_from_char(&self.text, self.cursor);
        self.text.insert(byte_pos, c);
        self.cursor += 1;
    }

    pub fn insert_text(&mut self, s: &str) -> bool {
        if s.is_empty() {
            return false;
        }
        self.clamp();
        let byte_pos = byte_index_from_char(&self.text, self.cursor);
        self.text.insert_str(byte_pos, s);
        self.cursor += s.chars().count();
        true
    }

    pub fn backspace(&mut self) -> bool {
        self.clamp();
        if self.cursor <= self.min_cursor {
            return false;
        }

        let end = byte_index_from_char(&self.text, self.cursor);
        let start = byte_index_from_char(&self.text, self.cursor - 1);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        true
    }

    pub fn delete_prev_word(&mut self) -> bool {
        self.clamp();
        if self.cursor <= self.min_cursor {
            return false;
        }

        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.cursor;
        while start > self.min_cursor && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > self.min_cursor && !chars[start - 1].is_whitespace() {
            start -= 1;
        }

        let start_byte = byte_index_from_char(&self.text, start);
        let end_byte = byte_index_from_char(&self.text, self.cursor);
        self.text.replace_range(start_byte..end_byte, "");
        self.cursor = start;
        true
    }

    pub fn delete_to_end(&mut self) -> bool {
        self.clamp();
        let start = byte_index_from_char(&self.text, self.cursor);
        if start >= self.text.len() {
            return false;
        }
        self.text.truncate(start);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_index_handles_utf8() {
        let text = "aあb";
        assert_eq!(byte_index_from_char(text, 0), 0);
        assert_eq!(byte_index_from_char(text, 1), 1);
        assert_eq!(byte_index_from_char(text, 2), 4);
        assert_eq!(byte_index_from_char(text, 3), 5);
        assert_eq!(byte_index_from_char(text, 99), 5);
    }

    #[test]
    fn new_places_cursor_at_end() {
        let ti = TextInput::new("hello".into(), 0);
        assert_eq!(ti.cursor, 5);
        assert_eq!(ti.min_cursor, 0);
    }

    #[test]
    fn with_text_places_cursor_at_end() {
        let ti = TextInput::with_text("ab");
        assert_eq!(ti.cursor, 2);
        assert_eq!(ti.min_cursor, 0);
    }

    #[test]
    fn set_text_moves_cursor_to_end() {
        let mut ti = TextInput::new(">".into(), 1);
        ti.set_text(">open file".into());
        assert_eq!(ti.cursor, 10);
    }

    #[test]
    fn clear_respects_min_cursor() {
        let mut ti = TextInput::new(">open file".into(), 1);
        ti.clear();
        assert_eq!(ti.text, ">");
        assert_eq!(ti.cursor, 1);
    }

    #[test]
    fn clamp_respects_bounds_and_min() {
        let mut ti = TextInput::new("hello".into(), 3);
        ti.cursor = 99;
        ti.clamp();
        assert_eq!(ti.cursor, 5);

        ti.cursor = 0;
        ti.clamp();
        assert_eq!(ti.cursor, 3);
    }

    #[test]
    fn word_movement_uses_whitespace_boundaries() {
        let mut ti = TextInput::with_text("foo  bar baz");
        ti.cursor = 7;
        ti.move_word_left();
        assert_eq!(ti.cursor, 5);

        ti.move_word_left();
        assert_eq!(ti.cursor, 0);

        ti.move_word_right();
        assert_eq!(ti.cursor, 5);

        ti.move_word_right();
        assert_eq!(ti.cursor, 9);
    }

    #[test]
    fn insert_and_backspace_handle_multibyte_chars() {
        let mut ti = TextInput::with_text("ab");
        ti.cursor = 1;
        ti.insert_char('あ');
        assert_eq!(ti.text, "aあb");
        assert_eq!(ti.cursor, 2);

        assert!(ti.backspace());
        assert_eq!(ti.text, "ab");
        assert_eq!(ti.cursor, 1);
    }

    #[test]
    fn delete_prev_word_honors_min_cursor() {
        let mut ti = TextInput::new(">open file".into(), 1);
        assert!(ti.delete_prev_word());
        assert_eq!(ti.text, ">open ");
        assert_eq!(ti.cursor, 6);

        assert!(ti.delete_prev_word());
        assert_eq!(ti.text, ">");
        assert_eq!(ti.cursor, 1);

        assert!(!ti.delete_prev_word());
    }

    #[test]
    fn delete_to_end_truncates_from_cursor() {
        let mut ti = TextInput::with_text("hello world");
        ti.cursor = 5;
        assert!(ti.delete_to_end());
        assert_eq!(ti.text, "hello");
        assert_eq!(ti.cursor, 5);
        assert!(!ti.delete_to_end());
    }

    #[test]
    fn delete_prev_word_input_trims_last_token() {
        let mut value = String::from("hello world");
        delete_prev_word_input(&mut value);
        assert_eq!(value, "hello ");
    }

    #[test]
    fn insert_text_at_cursor() {
        let mut ti = TextInput::with_text("ab");
        ti.cursor = 1;
        assert!(ti.insert_text("XY"));
        assert_eq!(ti.text, "aXYb");
        assert_eq!(ti.cursor, 3);
    }

    #[test]
    fn move_left_respects_min() {
        let mut ti = TextInput::new(">x".into(), 1);
        ti.cursor = 1;
        ti.move_left();
        assert_eq!(ti.cursor, 1);
    }

    #[test]
    fn move_right_respects_max() {
        let mut ti = TextInput::with_text("ab");
        ti.cursor = 2;
        ti.move_right();
        assert_eq!(ti.cursor, 2);
    }

    #[test]
    fn backspace_respects_min() {
        let mut ti = TextInput::new(">".into(), 1);
        ti.cursor = 1;
        assert!(!ti.backspace());
        assert_eq!(ti.text, ">");
    }
}
