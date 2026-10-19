//! UTF-8 input editing. Cursor positions are byte boundaries, display positions are columns.
use unicode_width::UnicodeWidthStr;

#[derive(Default, Clone, Debug)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
}
impl Editor {
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
    }
    pub fn insert(&mut self, text: &str) {
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .take(100_000)
            .collect::<String>()
            .replace('\t', "    ");
        if self.text.len() + text.len() > 200_000 {
            return;
        }
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }
    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.text.drain(self.cursor..end);
    }
    pub fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
    }
    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i)
            .unwrap_or(self.text.len());
    }
    pub fn word_back(&mut self) {
        let end = self.cursor;
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            self.left();
        }
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.left();
        }
        self.text.drain(self.cursor..end);
    }
    pub fn line_up(&mut self) {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if start == 0 {
            return;
        }
        let col = self.text[start..self.cursor].chars().count();
        let previous = self.text[..start - 1]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.cursor = previous
            + self.text[previous..start - 1]
                .chars()
                .take(col)
                .map(char::len_utf8)
                .sum::<usize>();
    }
    pub fn line_down(&mut self) {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let col = self.text[start..self.cursor].chars().count();
        let Some(next) = self.text[self.cursor..]
            .find('\n')
            .map(|i| self.cursor + i + 1)
        else {
            return;
        };
        let end = self.text[next..]
            .find('\n')
            .map(|i| next + i)
            .unwrap_or(self.text.len());
        self.cursor = next
            + self.text[next..end]
                .chars()
                .take(col)
                .map(char::len_utf8)
                .sum::<usize>();
    }
    pub fn position(&self, width: usize) -> (usize, usize) {
        let mut row = 0;
        let mut col = 0;
        let width = width.max(1);
        for c in self.text[..self.cursor].chars() {
            if c == '\n' {
                row += 1;
                col = 0;
                continue;
            }
            let w = c.to_string().width();
            if col + w > width {
                row += 1;
                col = 0;
            }
            col += w;
        }
        if col == width {
            row += 1;
            col = 0;
        }
        (row, col)
    }
}
